//! Headless scenario harness for package sets (P3 toolchain).
//!
//! A scenario fixture describes the world, entities with their components,
//! seeded wakeups, tick steps, and assertions. The runner assembles a real
//! authority, instantiates every behavior entry into one shared store, and
//! reports per-package diagnostics.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use torse_inventory::{Inventory, ItemCatalog, ItemStack};
use torse_sim::WorldAuthority;
use torse_sim::action::{ActionTarget, ActorContext};
use torse_types::{Compatibility, ContentKey, EntityId, WorldId};
use torse_world::{ComponentFields, ComponentValue, Position, WorldBounds};
use wasmtime::Engine;
use wasmtime::Store;
use wasmtime::component::Component;

use crate::authority::{AuthorityHost, DispatchOutcome, ModBehavior};
use crate::component::{CallBudget, Meters, encode_component, engine_with};
use crate::package::ResolvedModSet;

/// One `[[entity]]` fixture entry.
struct FixtureEntity {
    name: String,
    at: (f64, f64, f64),
    components: Vec<(ContentKey, Vec<(ContentKey, FixtureValue)>)>,
    inventory: Option<FixtureInventory>,
}

/// One `[[entity]]` fixture entry's inventory sugar.
struct FixtureInventory {
    slots: usize,
    stock: Vec<String>,
}

/// A component field value as written in the fixture.
#[derive(Debug)]
enum FixtureValue {
    U32(u32),
    I64(i64),
    Bool(bool),
    EntityRef(String),
}

/// One `[[wakeup]]` seed.
struct FixtureWakeup {
    tick: u64,
    package: String,
    entity: String,
    key: String,
}

/// One `[[assert]]` entry.
enum FixtureAssert {
    ComponentField {
        entity: String,
        schema: ContentKey,
        field: ContentKey,
        equals: FixtureValue,
    },
    InventorySlot {
        entity: String,
        slot: usize,
        item: Option<ContentKey>,
        count: u32,
    },
}

/// The parsed scenario fixture.
pub struct Scenario {
    side: u32,
    bounds: (i64, i64),
    entities: Vec<FixtureEntity>,
    wakeups: Vec<FixtureWakeup>,
    steps: Vec<u64>,
    asserts: Vec<FixtureAssert>,
}

/// Result of one assertion.
#[derive(Debug)]
pub struct AssertResult {
    pub detail: String,
    pub ok: bool,
}

/// Per-package diagnostics collected during a run.
#[derive(Clone, Debug, Default)]
pub struct PackageStats {
    pub dispatches: u32,
    pub committed: u32,
    pub rejected: u32,
    pub trapped: u32,
    pub no_plan: u32,
    pub deactivated: bool,
    pub wall: Duration,
    pub meters: Meters,
}

/// The full run report.
pub struct ScenarioReport {
    pub packages: BTreeMap<String, PackageStats>,
    pub asserts: Vec<AssertResult>,
    pub seq: u64,
    pub passed: bool,
}

fn fixture_key(text: &str) -> Result<ContentKey, String> {
    ContentKey::parse(text).map_err(|error| format!("invalid key `{text}`: {error}"))
}

fn value_from_table(entry: &toml::Value) -> Option<FixtureValue> {
    match entry {
        toml::Value::Integer(value)
            if i64::from(i32::MIN) <= *value && *value <= i64::from(i32::MAX) =>
        {
            if *value >= 0 && *value <= u32::MAX as i64 {
                Some(FixtureValue::U32(*value as u32))
            } else {
                Some(FixtureValue::I64(*value))
            }
        }
        toml::Value::Boolean(value) => Some(FixtureValue::Bool(*value)),
        toml::Value::String(text) if text.starts_with('@') => {
            Some(FixtureValue::EntityRef(text[1..].to_string()))
        }
        _ => None,
    }
}

impl Scenario {
    /// Parses a scenario fixture from its TOML text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let value: toml::Value =
            toml::from_str(text).map_err(|error| format!("scenario parse failed: {error}"))?;
        let world = value
            .get("world")
            .and_then(toml::Value::as_table)
            .ok_or("missing [world]")?;
        let side = world
            .get("side")
            .and_then(toml::Value::as_integer)
            .ok_or("world.side is required")? as u32;
        let min_y = world
            .get("min-y")
            .and_then(toml::Value::as_integer)
            .ok_or("world.min-y is required")?;
        let max_y = world
            .get("max-y")
            .and_then(toml::Value::as_integer)
            .ok_or("world.max-y is required")?;

        let mut entities = Vec::new();
        for entry in value
            .get("entity")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
        {
            let table = entry
                .as_table()
                .ok_or("[[entity]] entries must be tables")?;
            let name = table
                .get("id")
                .and_then(toml::Value::as_str)
                .ok_or("entity.id is required")?
                .to_string();
            let at = table
                .get("at")
                .and_then(toml::Value::as_array)
                .and_then(|values| {
                    let coords: Vec<f64> =
                        values.iter().filter_map(toml::Value::as_float).collect();
                    (coords.len() == 3).then_some((coords[0], coords[1], coords[2]))
                })
                .ok_or("entity.at must be three numbers")?;
            let mut components = Vec::new();
            if let Some(fields) = table.get("components").and_then(toml::Value::as_table) {
                for (schema, fields) in fields {
                    let schema = fixture_key(schema)?;
                    let fields = fields
                        .as_table()
                        .ok_or("entity.components entries must be tables")?;
                    let mut parsed = Vec::new();
                    for (field, value) in fields {
                        let field = fixture_key(field)?;
                        let value = value_from_table(value)
                            .ok_or("component values must be integers, bools, or @refs")?;
                        parsed.push((field, value));
                    }
                    components.push((schema, parsed));
                }
            }
            let inventory = table
                .get("inventory")
                .and_then(toml::Value::as_table)
                .map(|table| FixtureInventory {
                    slots: table
                        .get("slots")
                        .and_then(toml::Value::as_integer)
                        .unwrap_or(2) as usize,
                    stock: table
                        .get("stock")
                        .and_then(toml::Value::as_array)
                        .map(|entries| {
                            entries
                                .iter()
                                .filter_map(toml::Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                });
            entities.push(FixtureEntity {
                name,
                at,
                components,
                inventory,
            });
        }

        let mut wakeups = Vec::new();
        for entry in value
            .get("wakeup")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
        {
            let table = entry
                .as_table()
                .ok_or("[[wakeup]] entries must be tables")?;
            wakeups.push(FixtureWakeup {
                tick: table
                    .get("tick")
                    .and_then(toml::Value::as_integer)
                    .ok_or("wakeup.tick is required")? as u64,
                package: table
                    .get("package")
                    .and_then(toml::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                entity: table
                    .get("entity")
                    .and_then(toml::Value::as_str)
                    .ok_or("wakeup.entity is required")?
                    .to_string(),
                key: table
                    .get("key")
                    .and_then(toml::Value::as_str)
                    .ok_or("wakeup.key is required")?
                    .to_string(),
            });
        }

        let steps: Vec<u64> = value
            .get("step")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_table)
            .filter_map(|table| table.get("ticks").and_then(toml::Value::as_integer))
            .map(|ticks| ticks as u64)
            .collect();
        let steps = (!steps.is_empty())
            .then_some(steps)
            .ok_or("at least one [[step]] with ticks is required")?;

        let mut asserts = Vec::new();
        for entry in value
            .get("assert")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
        {
            let table = entry
                .as_table()
                .ok_or("[[assert]] entries must be tables")?;
            let entity = table
                .get("entity")
                .and_then(toml::Value::as_str)
                .ok_or("assert.entity is required")?
                .to_string();
            if let Some(slot) = table.get("slot").and_then(toml::Value::as_integer) {
                let item = table
                    .get("item")
                    .and_then(toml::Value::as_str)
                    .map(fixture_key)
                    .transpose()?;
                asserts.push(FixtureAssert::InventorySlot {
                    entity,
                    slot: slot as usize,
                    item,
                    count: table
                        .get("count")
                        .and_then(toml::Value::as_integer)
                        .unwrap_or(1) as u32,
                });
            } else {
                let schema = table
                    .get("component")
                    .and_then(toml::Value::as_str)
                    .map(fixture_key)
                    .transpose()?
                    .ok_or("assert needs component or slot")?;
                let field = table
                    .get("field")
                    .and_then(toml::Value::as_str)
                    .map(fixture_key)
                    .transpose()?
                    .ok_or("assert.field is required")?;
                let equals = table
                    .get("equals")
                    .and_then(value_from_table)
                    .ok_or("assert.equals must be an integer, bool, or @ref")?;
                asserts.push(FixtureAssert::ComponentField {
                    entity,
                    schema,
                    field,
                    equals,
                });
            }
        }

        Ok(Self {
            side,
            bounds: (min_y, max_y),
            entities,
            wakeups,
            steps,
            asserts,
        })
    }
}

/// Which permissions the host grants; the test default grants everything the
/// packages request, real servers pass an explicit policy.
pub enum GrantPolicy<'a> {
    AllRequested,
    Explicit(&'a [ContentKey]),
}

/// Instantiates behavior entries for every server-side package and runs the
/// scenario steps against one shared authority.
pub struct BehaviorRuntime {
    engine: Engine,
    store: Option<Store<AuthorityHost>>,
    instances: BTreeMap<ContentKey, (ModBehavior, Component)>,
    budget: CallBudget,
    stats: BTreeMap<ContentKey, PackageStats>,
}

impl BehaviorRuntime {
    pub fn new(
        set: &ResolvedModSet,
        host: AuthorityHost,
        components: BTreeMap<ContentKey, Vec<u8>>,
        budget: CallBudget,
    ) -> Result<Self, String> {
        let engine = engine_with(false)?;
        let mut store = AuthorityHost::bounded_store(&engine, host, budget)?;
        let mut linker: wasmtime::component::Linker<AuthorityHost> =
            wasmtime::component::Linker::new(&engine);
        ModBehavior::add_to_linker::<AuthorityHost, wasmtime::component::HasSelf<AuthorityHost>>(
            &mut linker,
            |state| state,
        )
        .map_err(|error| format!("linker setup failed: {error}"))?;
        let mut instances = BTreeMap::new();
        for package in &set.packages {
            let Some(entry) = &package.manifest.server_entry else {
                continue;
            };
            let bytes = components
                .get(&package.manifest.principal())
                .ok_or_else(|| format!("no built component for {}", package.manifest.id))?;
            let component = Component::new(&engine, bytes).map_err(|error| {
                format!("component for {} failed: {error}", package.manifest.id)
            })?;
            store
                .set_fuel(budget.fuel)
                .map_err(|error| format!("fuel setup failed: {error}"))?;
            store.data_mut().begin_invocation();
            store
                .data_mut()
                .bind_package(&package.manifest.principal())?;
            let bindings =
                ModBehavior::instantiate(&mut store, &component, &linker).map_err(|error| {
                    format!("instantiation for {} failed: {error}", package.manifest.id)
                })?;
            if store.data().budget_exceeded() {
                return Err("component initialization exceeded host budget".to_string());
            }
            let _ = entry;
            instances.insert(package.manifest.principal(), (bindings, component));
        }
        Ok(Self {
            engine,
            store: Some(store),
            instances,
            budget,
            stats: BTreeMap::new(),
        })
    }

    pub fn host(&self) -> &AuthorityHost {
        self.store.as_ref().expect("store present").data()
    }

    pub fn host_mut(&mut self) -> &mut AuthorityHost {
        self.store.as_mut().expect("store present").data_mut()
    }

    /// Dispatches one event to one package's instance, rebuilding it on trap.
    pub fn dispatch(
        &mut self,
        package: &ContentKey,
        tick: u64,
        event: crate::authority::InvokeEvent,
    ) -> Result<DispatchOutcome, String> {
        if self.host().is_deactivated(package) {
            let stats = self.stats.entry(package.clone()).or_default();
            stats.deactivated = true;
            return Ok(DispatchOutcome::Deactivated);
        }
        let start = Instant::now();
        let fuel = self.budget.fuel;
        self.store
            .as_mut()
            .expect("store present")
            .set_fuel(fuel)
            .map_err(|error| format!("fuel reset failed: {error}"))?;
        self.host_mut().begin_invocation();
        self.host_mut().bind_package(package)?;
        let Some(invocation) = self.host_mut().prepare_invocation(tick, &event, package) else {
            self.host_mut().on_failure(tick, &event, package);
            return Ok(DispatchOutcome::Rejected {
                reason: "host output budget exceeded".to_string(),
            });
        };
        let result = {
            let bindings = &self
                .instances
                .get(package)
                .ok_or_else(|| format!("no instance for {package}"))?
                .0;
            bindings
                .torse_behavior_invoke()
                .call_handle_event(self.store.as_mut().expect("store present"), &invocation)
        };
        let outcome = match result {
            Err(trap) => {
                let reason = trap
                    .chain()
                    .map(|cause| cause.to_string())
                    .collect::<Vec<_>>()
                    .join("; ");
                self.host_mut().on_failure(tick, &event, package);
                // Rebuild the store so trapped instances cannot accumulate
                // unreclaimable memories or consume its object count budget.
                self.rebuild(package)?;
                Ok::<DispatchOutcome, String>(DispatchOutcome::Trapped { reason })
            }
            Ok(None) if self.host_mut().plan_fits(None) => Ok(DispatchOutcome::NoPlan),
            Ok(None) => {
                self.host_mut().on_failure(tick, &event, package);
                self.rebuild(package)?;
                Ok(DispatchOutcome::Rejected {
                    reason: "host budget exceeded".to_string(),
                })
            }
            Ok(Some(plan)) => {
                let outcome = self.host_mut().apply_plan(&plan, package)?;
                if matches!(outcome, DispatchOutcome::Rejected { .. }) {
                    self.host_mut().on_failure(tick, &event, package);
                    if self.host().budget_exceeded() {
                        self.rebuild(package)?;
                    }
                }
                Ok(outcome)
            }
        }?;
        let meters = self.host().meters.clone();
        let stats = self.stats.entry(package.clone()).or_default();
        stats.dispatches += 1;
        stats.wall += start.elapsed();
        match &outcome {
            DispatchOutcome::Committed { .. } => stats.committed += 1,
            DispatchOutcome::Rejected { .. } => stats.rejected += 1,
            DispatchOutcome::Trapped { .. } => stats.trapped += 1,
            DispatchOutcome::NoPlan => stats.no_plan += 1,
            DispatchOutcome::Deactivated => {
                stats.deactivated = true;
            }
        }
        stats.meters = meters;
        Ok(outcome)
    }

    fn rebuild(&mut self, package: &ContentKey) -> Result<(), String> {
        if !self.instances.contains_key(package) {
            return Err(format!("no instance for {package}"));
        }
        let old = self.store.take().expect("store present");
        let mut store = AuthorityHost::bounded_store(&self.engine, old.into_data(), self.budget)?;
        let mut linker: wasmtime::component::Linker<AuthorityHost> =
            wasmtime::component::Linker::new(&self.engine);
        ModBehavior::add_to_linker::<AuthorityHost, wasmtime::component::HasSelf<AuthorityHost>>(
            &mut linker,
            |state| state,
        )
        .map_err(|error| format!("linker setup failed: {error}"))?;
        let mut instances = BTreeMap::new();
        for (principal, (_, component)) in &self.instances {
            store
                .set_fuel(self.budget.fuel)
                .map_err(|error| format!("fuel reset failed: {error}"))?;
            store.data_mut().begin_invocation();
            store.data_mut().bind_package(principal)?;
            let bindings = ModBehavior::instantiate(&mut store, component, &linker)
                .map_err(|error| format!("rebuild for {principal} failed: {error}"))?;
            if store.data().budget_exceeded() {
                return Err("component initialization exceeded host budget".to_string());
            }
            instances.insert(principal.clone(), (bindings, component.clone()));
        }
        self.instances = instances;
        self.store = Some(store);
        Ok(())
    }

    /// Runs the scenario steps, dispatching every due wakeup to its package.
    pub fn run_ticks(&mut self, from: u64, count: u64) -> Result<Vec<DispatchOutcome>, String> {
        let mut outcomes = Vec::new();
        for tick in from..from + count {
            let due = self.host_mut().drain_due_wakeups(tick);
            for (entity, key, package) in due {
                let event = crate::authority::WakeupTarget {
                    entity: crate::authority::WitEntityId::from_entity_id(entity),
                    key,
                };
                let event = crate::authority::InvokeEvent::Wakeup(event);
                outcomes.push(self.dispatch(&package, tick, event)?);
            }
        }
        Ok(outcomes)
    }

    pub fn stats(&self) -> &BTreeMap<ContentKey, PackageStats> {
        &self.stats
    }
}

fn fixture_component_fields(
    values: &[(ContentKey, FixtureValue)],
    entities: &BTreeMap<String, EntityId>,
) -> Result<ComponentFields, String> {
    let mut fields = ComponentFields::new();
    for (field, value) in values {
        let value = match value {
            FixtureValue::U32(value) => ComponentValue::U32(*value),
            FixtureValue::I64(value) => ComponentValue::I64(*value),
            FixtureValue::Bool(value) => ComponentValue::Bool(*value),
            FixtureValue::EntityRef(name) => ComponentValue::Entity(
                *entities
                    .get(name)
                    .ok_or_else(|| format!("fixture references unknown entity `{name}`"))?,
            ),
        };
        fields.insert(field.clone(), value);
    }
    Ok(fields)
}

/// Assembles the authority from the resolved set and fixture, then runs it.
pub fn run_scenario(
    set: &ResolvedModSet,
    scenario: &Scenario,
    policy: &GrantPolicy<'_>,
    components: BTreeMap<ContentKey, Vec<u8>>,
) -> Result<ScenarioReport, String> {
    let (registry, mut schemas) = crate::package::registries(&set.content)?;
    let catalog = ItemCatalog::new([(ContentKey::parse("base:stone").expect("fixture item"), 64)])
        .map_err(|code| format!("catalog: {code:?}"))?;
    catalog
        .bind_schemas(&mut schemas)
        .map_err(|error| format!("item validation: {error}"))?;

    let authority = WorldAuthority::new(
        WorldId(97),
        registry,
        schemas.clone(),
        scenario.side,
        WorldBounds::new(scenario.bounds.0, scenario.bounds.1)
            .map_err(|error| format!("bounds: {error:?}"))?,
        catalog.bind_compatibility(Compatibility {
            authority_schema: 3,
            rules_digest: set.content.authority_digest,
            content_digest: set.content.presentation_digest,
        }),
    )
    .map_err(|error| format!("authority: {error:?}"))?;

    let mut symbols = BTreeMap::new();
    let mut symbol_keys = Vec::new();
    {
        let intern = |text: &str, symbols: &mut BTreeMap<String, u32>, keys: &mut Vec<String>| {
            if !symbols.contains_key(text) {
                let symbol = keys.len() as u32 + 1;
                symbols.insert(text.to_string(), symbol);
                keys.push(text.to_string());
            }
        };
        for block in set.content.blocks.keys() {
            intern(&block.to_string(), &mut symbols, &mut symbol_keys);
        }
        for state in set.content.state_schemas.values() {
            for schema in &state.component_schemas {
                intern(&schema.id.to_string(), &mut symbols, &mut symbol_keys);
            }
        }
        intern("base:stone", &mut symbols, &mut symbol_keys);
        for package in &set.packages {
            intern(
                &format!("{}:module", package.manifest.id),
                &mut symbols,
                &mut symbol_keys,
            );
        }
    }

    let mut host = AuthorityHost::new(authority, catalog.clone(), symbols, symbol_keys);

    // Spawn fixture entities first so component references can point at them.
    let mut entity_ids: BTreeMap<String, EntityId> = BTreeMap::new();
    for entity in &scenario.entities {
        let position = Position::new(entity.at.0, entity.at.1, entity.at.2);
        let id = host
            .authority_mut()
            .entities_mut()
            .spawn(position)
            .map_err(|error| format!("spawn {}: {error:?}", entity.name))?;
        let id = host
            .authority()
            .entities()
            .get(id)
            .expect("spawned entity")
            .id;
        entity_ids.insert(entity.name.clone(), id);
    }
    for entity in &scenario.entities {
        let id = entity_ids[&entity.name];
        let handle = host
            .authority()
            .entities()
            .handle_of(id)
            .expect("fixture entity");
        for (schema, values) in &entity.components {
            let fields = fixture_component_fields(values, &entity_ids)?;
            host.authority_mut()
                .entities_mut()
                .components_mut(handle)
                .expect("components")
                .insert(&schemas, schema, fields)
                .map_err(|error| format!("component install: {error:?}"))?;
        }
        if let Some(inventory) = &entity.inventory {
            let mut slots =
                Inventory::new(inventory.slots).map_err(|code| format!("inventory: {code:?}"))?;
            for (index, item) in inventory.stock.iter().enumerate() {
                slots
                    .set(
                        index,
                        Some(ItemStack {
                            data: Default::default(),
                            item: fixture_key(item)?,
                            count: 1,
                        }),
                        &catalog,
                    )
                    .map_err(|code| format!("stock: {code:?}"))?;
            }
            host.authority_mut()
                .entities_mut()
                .components_mut(handle)
                .expect("components")
                .insert(&schemas, &torse_inventory::schema(), slots.fields())
                .map_err(|error| format!("inventory install: {error:?}"))?;
        }
    }

    // Register packages: granted permissions, fixture entities as targets,
    // and their declared component schemas.
    for package in &set.packages {
        let principal = package.manifest.principal();
        let granted: Vec<ContentKey> = match policy {
            GrantPolicy::AllRequested => package.manifest.permissions.clone(),
            GrantPolicy::Explicit(list) => package
                .manifest
                .permissions
                .iter()
                .filter(|permission| list.contains(permission))
                .cloned()
                .collect(),
        };
        let mut actor = ActorContext::new(principal.clone());
        for permission in &granted {
            actor = actor.permission(permission.clone());
        }
        for entity in entity_ids.values() {
            actor = actor.target(ActionTarget::Entity(*entity));
        }
        let owned = set
            .owned_schemas
            .get(&principal)
            .cloned()
            .unwrap_or_default();
        host.register_package(principal, actor, owned);
    }

    for wakeup in &scenario.wakeups {
        let package = fixture_key(&format!("{}:module", wakeup.package))
            .or_else(|_| {
                set.packages
                    .first()
                    .map(|package| package.manifest.principal())
                    .ok_or_else(|| "no packages loaded".to_string())
            })
            .map_err(|detail| format!("wakeup: {detail}"))?;
        let entity = entity_ids
            .get(&wakeup.entity)
            .ok_or_else(|| format!("wakeup references unknown entity `{}`", wakeup.entity))?;
        host.seed_wakeup(wakeup.tick, *entity, &wakeup.key, package);
    }

    let mut runtime = BehaviorRuntime::new(set, host, components, CallBudget::prototype())?;

    let mut tick = 1_u64;
    for step in &scenario.steps {
        runtime.run_ticks(tick, *step)?;
        tick += step;
    }

    // Evaluate assertions against the final state.
    let mut asserts = Vec::new();
    for assert in &scenario.asserts {
        let result = evaluate_assert(&runtime, assert, &entity_ids, &catalog);
        asserts.push(result);
    }
    let seq = runtime.host().seq().0;
    let passed = asserts.iter().all(|result| result.ok);
    let mut packages = BTreeMap::new();
    for package in &set.packages {
        let principal = package.manifest.principal();
        let stats = runtime.stats().get(&principal).cloned().unwrap_or_default();
        packages.insert(package.manifest.id.clone(), stats);
    }
    Ok(ScenarioReport {
        packages,
        asserts,
        seq,
        passed,
    })
}

fn evaluate_assert(
    runtime: &BehaviorRuntime,
    assert: &FixtureAssert,
    entities: &BTreeMap<String, EntityId>,
    catalog: &ItemCatalog,
) -> AssertResult {
    match assert {
        FixtureAssert::ComponentField {
            entity,
            schema,
            field,
            equals,
        } => {
            let Some(id) = entities.get(entity) else {
                return AssertResult {
                    detail: format!("assert references unknown entity `{entity}`"),
                    ok: false,
                };
            };
            let authority = runtime.host().authority();
            let fields = authority
                .entities()
                .handle_of(*id)
                .and_then(|handle| authority.entities().get(handle))
                .and_then(|entity| entity.components.component(schema));
            let Some(fields) = fields else {
                return AssertResult {
                    detail: format!("entity `{entity}` has no component {schema}"),
                    ok: false,
                };
            };
            let actual = fields.get(field);
            let expected = match (equals, actual) {
                (FixtureValue::U32(want), Some(ComponentValue::U32(got))) => want == got,
                (FixtureValue::I64(want), Some(ComponentValue::I64(got))) => want == got,
                (FixtureValue::Bool(want), Some(ComponentValue::Bool(got))) => want == got,
                (FixtureValue::EntityRef(name), Some(ComponentValue::Entity(got))) => {
                    entities.get(name).is_some_and(|want| want == got)
                }
                (want, got) => {
                    return AssertResult {
                        detail: format!("field {field}: expected {want:?}, found {got:?}"),
                        ok: false,
                    };
                }
            };
            AssertResult {
                detail: format!("field {field} on `{entity}`"),
                ok: expected,
            }
        }
        FixtureAssert::InventorySlot {
            entity,
            slot,
            item,
            count,
        } => {
            let Some(id) = entities.get(entity) else {
                return AssertResult {
                    detail: format!("assert references unknown entity `{entity}`"),
                    ok: false,
                };
            };
            let authority = runtime.host().authority();
            let fields = authority
                .entities()
                .handle_of(*id)
                .and_then(|handle| authority.entities().get(handle))
                .and_then(|entity| entity.components.component(&torse_inventory::schema()));
            let Some(fields) = fields else {
                return AssertResult {
                    detail: format!("entity `{entity}` has no inventory"),
                    ok: false,
                };
            };
            let Ok(inventory) = Inventory::from_fields(fields, catalog) else {
                return AssertResult {
                    detail: format!("inventory on `{entity}` does not decode"),
                    ok: false,
                };
            };
            let actual = inventory.slots().get(*slot).cloned().flatten();
            let ok = match (item, &actual) {
                (None, None) => true,
                (Some(want), Some(got)) => &got.item == want && got.count == *count,
                _ => false,
            };
            AssertResult {
                detail: format!("slot {slot} on `{entity}`: {actual:?}"),
                ok,
            }
        }
    }
}

/// Builds behavior components for every package with a build crate by
/// invoking cargo, then encoding the artifacts.
pub fn build_components(set: &ResolvedModSet) -> Result<BTreeMap<ContentKey, Vec<u8>>, String> {
    let mut components = BTreeMap::new();
    for package in &set.packages {
        let Some(build_crate) = &package.manifest.build_crate else {
            // Data-only packages contribute no behavior component.
            continue;
        };
        let workspace = package
            .manifest
            .build_workspace
            .as_ref()
            .map(|relative| package.dir.join(relative))
            .unwrap_or_else(|| package.dir.clone());
        let workspace = workspace.canonicalize().unwrap_or(workspace.clone());
        let status = std::process::Command::new("cargo")
            .arg("build")
            .arg("--release")
            .arg("--target")
            .arg("wasm32-unknown-unknown")
            .arg("-p")
            .arg(build_crate)
            .current_dir(&workspace)
            .status()
            .map_err(|error| format!("cargo spawn failed: {error}"))?;
        if !status.success() {
            return Err(format!("build for {build_crate} failed"));
        }
        let artifact = workspace
            .join("target")
            .join("wasm32-unknown-unknown")
            .join("release")
            .join(build_crate.replace('-', "_"))
            .with_extension("wasm");
        let core = std::fs::read(&artifact)
            .map_err(|error| format!("artifact {}: {error}", artifact.display()))?;
        let component = encode_component(&core)?;
        components.insert(package.manifest.principal(), component);
    }
    Ok(components)
}

/// The engine base content packages are loaded on top of: the M0 fixture
/// world, block actions, and the inventory module.
pub fn engine_base() -> torse_content::ContentPack {
    let mut pack = torse_content::m0_fixture_pack();
    torse_sim::block_actions::register_content(&mut pack);
    torse_inventory::register_content(&mut pack);
    pack
}
