//! Mod assembly for the durable execution host.
//!
//! Loads behavior packages from directories, spawns the demo machine world,
//! and adapts each package into one `DurableRule`. Machine edits and native
//! inventory plans form one multi-source transaction group; wakeups share its
//! durable settlement. Traps settle as empty work with a bounded
//! retry; rejections settle terminally without touching the world.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use crate::execution::WorkPlan;
use torse_inventory::{ItemCatalog, ItemStack, Transfer, plan_transfer};
use torse_mod_host::component::CallBudget;
use torse_mod_host::durable_dispatch::{
    BlockNotification, DurableGuest, GuestOutcome, SymbolTable, TransferSpec, convert_plan,
};
use torse_mod_host::package::{self, resolve};
use torse_sim::action::{ActionTarget, ActorContext};
use torse_sim::durable::{EventDraft, TimedEvent};
use torse_sim::{OrderingKey, Phase, SystemSpec, TransactionGroup, WorldAuthority};
use torse_types::{Compatibility, ContentKey, EntityId, Fault, Tick, TransactionId};
use torse_world::{ComponentFields, ComponentValue, Position, Section, WorldBounds};

use crate::bootstrap;
use crate::execution::DurableRule;

/// Package ids whose guest is built with the `spin` feature (trap tests);
/// every other package builds normally.
pub type SpinPackages = [String];

/// Shared counters exposed to tests; one set for all packages.
#[derive(Clone, Debug, Default)]
pub struct ModStats {
    pub guest_dispatches: BTreeMap<String, u64>,
    pub traps: BTreeMap<String, u64>,
    pub rejections: BTreeMap<String, u64>,
    pub transfer_rejections: BTreeMap<String, u64>,
    pub parks: BTreeMap<String, u64>,
}

fn bump(map: &mut BTreeMap<String, u64>, key: &str) {
    *map.entry(key.to_string()).or_insert(0) += 1;
}

/// Retry backoff in ticks and the bounded attempt budget for trapped guests.
const RETRY_TICKS: u64 = 8;
const MAX_ATTEMPTS: u32 = 8;

/// Mutable rule state: the guest executor. The rule trait
/// takes `&self`, so mutation lives behind a `RefCell`; this adapter keeps
/// serial callbacks on the host thread and dispatches never recurse.
struct RuleInner {
    guest: DurableGuest,
}

/// One behavior package adapted to the durable execution host.
pub struct ModRule {
    package_id: String,
    package: ContentKey,
    rule: ContentKey,
    owned_schemas: Vec<ContentKey>,
    symbols: Arc<SymbolTable>,
    catalog: ItemCatalog,
    actor: ActorContext,
    stats: Rc<RefCell<ModStats>>,
    inner: RefCell<RuleInner>,
}

fn payload_key(package: &str, field: &str) -> ContentKey {
    ContentKey::parse(&format!("{package}:event/{field}")).expect("payload key")
}

fn wakeup_draft(
    rule: &ContentKey,
    package_id: &str,
    tick: u64,
    entity: EntityId,
    key: &str,
    attempt: u32,
) -> EventDraft {
    EventDraft {
        tick: Tick(tick),
        rule: rule.clone(),
        payload: ComponentFields::from([
            (
                payload_key(package_id, "kind"),
                ComponentValue::Text("wakeup".into()),
            ),
            (
                payload_key(package_id, "entity"),
                ComponentValue::Entity(entity),
            ),
            (
                payload_key(package_id, "key"),
                ComponentValue::Text(key.into()),
            ),
            (
                payload_key(package_id, "attempt"),
                ComponentValue::U32(attempt),
            ),
        ]),
    }
}

fn empty_plan() -> WorkPlan {
    WorkPlan {
        transaction: None,
        schedule: Vec::new(),
        cancel: Vec::new(),
    }
}

impl ModRule {
    fn settle_wakeup(
        &self,
        world: &WorldAuthority,
        tick: Tick,
        entity: EntityId,
        key: &str,
        attempt: u32,
        transaction_id: TransactionId,
    ) -> WorkPlan {
        let outcome = {
            let mut inner = self.inner.borrow_mut();
            let outcome = inner
                .guest
                .dispatch(world, tick.0, entity, key, &self.owned_schemas);
            if outcome.is_ok() {
                bump(
                    &mut self.stats.borrow_mut().guest_dispatches,
                    &self.package_id,
                );
            }
            outcome
        };
        self.settle_outcome(world, tick, entity, key, attempt, transaction_id, outcome)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "durable event settlement context"
    )]
    fn settle_outcome(
        &self,
        world: &WorldAuthority,
        tick: Tick,
        entity: EntityId,
        key: &str,
        attempt: u32,
        transaction_id: TransactionId,
        outcome: Result<GuestOutcome, String>,
    ) -> WorkPlan {
        // A host-scheduled object may have been created after package assembly.
        // Ownership of one declared component grants this invocation its own
        // target; unrelated entities and external callers receive no grant.
        let mut actor = self.actor.clone();
        if world
            .entities()
            .handle_of(entity)
            .and_then(|h| world.entities().get(h))
            .is_some_and(|object| {
                self.owned_schemas.iter().any(|schema| {
                    object.components.component(schema).is_some()
                        && world
                            .schemas()
                            .get(schema)
                            .is_some_and(|s| s.owner_module == self.package)
                })
            })
        {
            actor = actor.target(ActionTarget::Entity(entity));
        }
        match outcome {
            Err(_setup) => {
                // Structural dispatch failures are terminal for this event;
                // they never fault the phase or touch the world.
                bump(&mut self.stats.borrow_mut().rejections, &self.package_id);
                empty_plan()
            }
            Ok(GuestOutcome::Trapped(_reason)) => {
                bump(&mut self.stats.borrow_mut().traps, &self.package_id);
                if entity == EntityId(0) || attempt + 1 >= MAX_ATTEMPTS {
                    bump(&mut self.stats.borrow_mut().parks, &self.package_id);
                    return empty_plan();
                }
                WorkPlan {
                    transaction: None,
                    schedule: vec![wakeup_draft(
                        &self.rule,
                        &self.package_id,
                        tick.0 + RETRY_TICKS,
                        entity,
                        key,
                        attempt + 1,
                    )],
                    cancel: Vec::new(),
                }
            }
            Ok(GuestOutcome::NoPlan) => empty_plan(),
            Ok(GuestOutcome::Plan(plan)) => {
                let stable = entity.0 as u64;
                // The caller derives the diagnostic identity from the durable
                // event; retries consume no process-local allocation state.
                let local = 0;
                match convert_plan(
                    world,
                    &self.symbols,
                    &self.package,
                    &actor,
                    &plan,
                    stable,
                    transaction_id,
                    local,
                ) {
                    Ok(parts) => {
                        let mut schedule = Vec::new();
                        let ordering = OrderingKey {
                            phase: Phase::Simulation,
                            rule_priority: 0,
                            stable_task_key: stable,
                            local_sequence: local,
                        };
                        let mut members: Vec<_> = parts.machine.into_iter().collect();
                        if !parts.transfers.is_empty() {
                            let requests: Vec<_> = parts
                                .transfers
                                .iter()
                                .map(|transfer| Transfer {
                                    source: transfer.source,
                                    source_slot: transfer.source_slot as usize,
                                    destination: transfer.destination,
                                    destination_slot: transfer.destination_slot as usize,
                                    count: transfer.count,
                                })
                                .collect();
                            match torse_inventory::plan_transfers(
                                &actor.view(world),
                                &self.catalog,
                                &requests,
                                transaction_id,
                                ordering,
                            ) {
                                Ok(transaction) => members.push(transaction),
                                Err(_) => {
                                    bump(
                                        &mut self.stats.borrow_mut().transfer_rejections,
                                        &self.package_id,
                                    );
                                    return empty_plan();
                                }
                            }
                        }
                        for (wakeup_tick, wakeup_entity, wakeup_key) in parts.wakeups {
                            schedule.push(wakeup_draft(
                                &self.rule,
                                &self.package_id,
                                wakeup_tick.max(tick.0 + 1),
                                wakeup_entity,
                                &wakeup_key,
                                0,
                            ));
                        }
                        WorkPlan {
                            transaction: if members.is_empty() {
                                None
                            } else {
                                match TransactionGroup::new(transaction_id, ordering, members) {
                                    Ok(group) => Some(group),
                                    Err(_) => {
                                        bump(
                                            &mut self.stats.borrow_mut().rejections,
                                            &self.package_id,
                                        );
                                        return empty_plan();
                                    }
                                }
                            },
                            schedule,
                            cancel: Vec::new(),
                        }
                    }
                    Err(_error) => {
                        // Plan-level rejection settles terminally without any
                        // world change; the event stays consumed.
                        bump(&mut self.stats.borrow_mut().rejections, &self.package_id);
                        empty_plan()
                    }
                }
            }
        }
    }

    fn settle_transfer(
        &self,
        world: &WorldAuthority,
        transfer: &TransferSpec,
        transaction_id: TransactionId,
    ) -> WorkPlan {
        let source = transfer.source;
        let request = Transfer {
            source,
            source_slot: transfer.source_slot as usize,
            destination: transfer.destination,
            destination_slot: transfer.destination_slot as usize,
            count: transfer.count,
        };
        // Retained only for pending transfer events from the B8 format.
        let local = 0;
        let ordering = OrderingKey {
            phase: Phase::Simulation,
            rule_priority: 0,
            stable_task_key: source.0 as u64,
            local_sequence: local,
        };
        let view = self.actor.view(world);
        match plan_transfer(&view, &self.catalog, request, transaction_id, ordering) {
            Ok(transaction) => WorkPlan::transaction(transaction),
            Err(_code) => {
                // Business rejection: empty barrier, no retry scheduling; the
                // machine's next cycle retries on its own.
                bump(
                    &mut self.stats.borrow_mut().transfer_rejections,
                    &self.package_id,
                );
                empty_plan()
            }
        }
    }

    fn decode_payload(
        &self,
        event: &TimedEvent,
    ) -> Option<(String, EntityId, String, u32, Option<TransferSpec>)> {
        let payload = &event.event.payload;
        let kind = match payload.get(&payload_key(&self.package_id, "kind")) {
            Some(ComponentValue::Text(kind)) => kind.clone(),
            _ => return None,
        };
        let entity = match payload.get(&payload_key(&self.package_id, "entity")) {
            Some(ComponentValue::Entity(entity)) => *entity,
            _ => EntityId(0),
        };
        let key = match payload.get(&payload_key(&self.package_id, "key")) {
            Some(ComponentValue::Text(key)) => key.clone(),
            _ => String::new(),
        };
        let attempt = match payload.get(&payload_key(&self.package_id, "attempt")) {
            Some(ComponentValue::U32(attempt)) => *attempt,
            _ => 0,
        };
        let transfer = (kind == "transfer")
            .then(|| {
                let field = |name: &str| payload.get(&payload_key(&self.package_id, name));
                Some(TransferSpec {
                    source: match field("source") {
                        Some(ComponentValue::Entity(entity)) => *entity,
                        _ => return None,
                    },
                    source_slot: match field("source-slot") {
                        Some(ComponentValue::U32(value)) => *value,
                        _ => return None,
                    },
                    destination: match field("destination") {
                        Some(ComponentValue::Entity(entity)) => *entity,
                        _ => return None,
                    },
                    destination_slot: match field("destination-slot") {
                        Some(ComponentValue::U32(value)) => *value,
                        _ => return None,
                    },
                    count: match field("count") {
                        Some(ComponentValue::U32(value)) => *value,
                        _ => return None,
                    },
                })
            })
            .flatten();
        Some((kind, entity, key, attempt, transfer))
    }
}

impl DurableRule for ModRule {
    fn spec(&self) -> SystemSpec {
        SystemSpec {
            id: self.rule.clone(),
            phase: Phase::Simulation,
            rule_priority: 0,
            before: Vec::new(),
            after: Vec::new(),
        }
    }

    fn action_events(
        &self,
        world: &WorldAuthority,
        tick: Tick,
        transaction: &torse_sim::Transaction,
    ) -> Result<Vec<EventDraft>, Fault> {
        let mut events = Vec::new();
        for mutation in &transaction.mutations {
            let previous = transaction
                .preconditions
                .iter()
                .find(|read| read.pos == mutation.pos)
                .map(|read| read.expected);
            let Some(previous) = previous else { continue };
            let (state, placed) = if previous.is_empty() && !mutation.state.is_empty() {
                (mutation.state, true)
            } else if !previous.is_empty() && mutation.state.is_empty() {
                (previous, false)
            } else {
                continue;
            };
            let Some(definition) = world.registry().definition(state) else {
                continue;
            };
            if !definition.behavior_bindings.contains(&self.rule) {
                continue;
            }
            let tick = tick
                .0
                .checked_add(1)
                .ok_or_else(|| Fault::resource_limit("block event tick exhausted"))?;
            let pos = mutation.pos;
            events.push(EventDraft {
                tick: Tick(tick),
                rule: self.rule.clone(),
                payload: ComponentFields::from([
                    (
                        payload_key(&self.package_id, "kind"),
                        ComponentValue::Text(
                            if placed {
                                "block-placed"
                            } else {
                                "block-broken"
                            }
                            .into(),
                        ),
                    ),
                    (
                        payload_key(&self.package_id, "block"),
                        ComponentValue::Content(definition.id.clone()),
                    ),
                    (
                        payload_key(&self.package_id, "values"),
                        ComponentValue::List(
                            world
                                .registry()
                                .state_values(state)
                                .unwrap_or_default()
                                .iter()
                                .map(|value| ComponentValue::U32(u32::from(*value)))
                                .collect(),
                        ),
                    ),
                    (
                        payload_key(&self.package_id, "x"),
                        ComponentValue::I64(pos.x),
                    ),
                    (
                        payload_key(&self.package_id, "y"),
                        ComponentValue::I64(pos.y),
                    ),
                    (
                        payload_key(&self.package_id, "z"),
                        ComponentValue::I64(pos.z),
                    ),
                ]),
            });
        }
        Ok(events)
    }

    fn plan(&self, world: &WorldAuthority, tick: Tick) -> Result<Vec<WorkPlan>, Fault> {
        if tick != Tick(0) {
            return Ok(Vec::new());
        }
        // Arm every entity carrying this package's schemas.
        let mut schedule = Vec::new();
        for (_, entity) in world.entities().iter() {
            if self
                .owned_schemas
                .iter()
                .any(|schema| entity.components.component(schema).is_some())
            {
                schedule.push(wakeup_draft(
                    &self.rule,
                    &self.package_id,
                    1,
                    entity.id,
                    "machine:tick",
                    0,
                ));
            }
        }
        if schedule.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![WorkPlan {
            transaction: None,
            schedule,
            cancel: Vec::new(),
        }])
    }

    fn event(
        &self,
        world: &WorldAuthority,
        tick: Tick,
        event: &TimedEvent,
    ) -> Result<WorkPlan, Fault> {
        // The rule stays a pure planner over the world; mutation is confined
        // to guest-local state behind the RefCell.
        let Some((kind, entity, key, attempt, transfer)) = self.decode_payload(event) else {
            return Ok(empty_plan());
        };
        if kind == "block-placed" || kind == "block-broken" {
            let payload = &event.event.payload;
            let field = |name| payload.get(&payload_key(&self.package_id, name));
            let Some(ComponentValue::Content(definition)) = field("block") else {
                return Err(Fault::invalid_data("block notification definition"));
            };
            let coordinate = |name| match field(name) {
                Some(ComponentValue::I64(value)) => Ok(*value),
                _ => Err(Fault::invalid_data("block notification coordinate")),
            };
            let Some(ComponentValue::List(values)) = field("values") else {
                return Err(Fault::invalid_data("block notification state"));
            };
            let values = values
                .iter()
                .map(|value| match value {
                    ComponentValue::U32(value) => u16::try_from(*value)
                        .map_err(|_| Fault::invalid_data("block notification state value")),
                    _ => Err(Fault::invalid_data("block notification state value")),
                })
                .collect::<Result<Vec<_>, _>>()?;
            let notification = BlockNotification {
                pos: torse_types::BlockPos::new(
                    coordinate("x")?,
                    coordinate("y")?,
                    coordinate("z")?,
                ),
                definition: definition.clone(),
                values,
                placed: kind == "block-placed",
            };
            // Recheck the frozen package binding without consulting the current cell.
            let state = world
                .registry()
                .default_state(definition)
                .and_then(|state| world.registry().definition(state));
            if !state.is_some_and(|block| block.behavior_bindings.contains(&self.rule)) {
                return Ok(empty_plan());
            }
            let outcome = self.inner.borrow_mut().guest.dispatch_block(
                world,
                tick.0,
                &notification,
                &self.owned_schemas,
            );
            if outcome.is_ok() {
                bump(
                    &mut self.stats.borrow_mut().guest_dispatches,
                    &self.package_id,
                );
            }
            return Ok(self.settle_outcome(
                world,
                tick,
                EntityId(0),
                "",
                0,
                TransactionId(event.id.0),
                outcome,
            ));
        }
        let plan = match kind.as_str() {
            "transfer" => match transfer {
                Some(transfer) => self.settle_transfer(world, &transfer, TransactionId(event.id.0)),
                None => empty_plan(),
            },
            "wakeup" => self.settle_wakeup(
                world,
                tick,
                entity,
                &key,
                attempt,
                TransactionId(event.id.0),
            ),
            _ => empty_plan(),
        };
        Ok(plan)
    }
}

/// The assembled mod world: authority, actions, and behavior rules.
pub struct ModWorld {
    pub world: WorldAuthority,
    pub actions: torse_sim::action::ActionService,
    pub rules: Vec<Box<dyn DurableRule>>,
    pub stats: Rc<RefCell<ModStats>>,
    /// Machine entities in package and copy order.
    pub machines: Vec<(String, EntityId)>,
    pub input: EntityId,
    pub output: EntityId,
}

/// Bounded assembly for real multi-machine workloads. The executable keeps one
/// machine per package and shared ports through `Default`.
#[derive(Clone, Copy, Debug)]
pub struct ModLayout {
    pub machines_per_package: usize,
    pub shared_ports: bool,
}
impl Default for ModLayout {
    fn default() -> Self {
        Self {
            machines_per_package: 1,
            shared_ports: true,
        }
    }
}

fn additional_ports(
    world: &mut WorldAuthority,
    catalog: &ItemCatalog,
    position: Position,
    stock: bool,
) -> Result<[EntityId; 2], Fault> {
    let schemas = world.schemas().clone();
    let mut ids = [EntityId(0); 2];
    for (index, id) in ids.iter_mut().enumerate() {
        let handle = world
            .entities_mut()
            .spawn(Position::new(
                position.x - 0.5,
                position.y,
                position.z + index as f64 * 0.25,
            ))
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        *id = world.entities().get(handle).expect("port entity").id;
        let mut inventory = torse_inventory::Inventory::new(2)
            .map_err(|e| Fault::invalid_data(format!("{e:?}")))?;
        if stock && index == 0 {
            inventory
                .set(
                    0,
                    Some(ItemStack {
                        data: Default::default(),
                        item: bootstrap::key("base:stone"),
                        count: 1,
                    }),
                    catalog,
                )
                .map_err(|e| Fault::invalid_data(format!("{e:?}")))?;
        }
        world
            .entities_mut()
            .components_mut(handle)
            .expect("port components")
            .insert(&schemas, &torse_inventory::schema(), inventory.fields())
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
    }
    Ok(ids)
}

fn build_guest_bytes(package: &package::LoadedPackage, spin: bool) -> Result<Vec<u8>, Fault> {
    let Some(build_crate) = &package.manifest.build_crate else {
        return Err(Fault::invalid_data("behavior package without build crate"));
    };
    let workspace = package
        .manifest
        .build_workspace
        .as_ref()
        .map(|relative| package.dir.join(relative))
        .unwrap_or_else(|| package.dir.clone());
    let mut command = std::process::Command::new("cargo");
    // Feature variants must not race over the same output file when several
    // package tests build and read artifacts concurrently.
    let target = workspace
        .join(".work/mod-guests")
        .join(if spin { "spin" } else { "normal" });
    command
        .arg("build")
        .arg("--locked")
        .arg("--target-dir")
        .arg(&target)
        .arg("--release")
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .arg("-p")
        .arg(build_crate)
        .current_dir(&workspace);
    if spin {
        command.arg("--features").arg("spin");
    }
    let status = command
        .status()
        .map_err(|error| Fault::invalid_data(format!("cargo spawn: {error}")))?;
    if !status.success() {
        return Err(Fault::invalid_data("guest build failed"));
    }
    let artifact = target
        .join("wasm32-unknown-unknown")
        .join("release")
        .join(build_crate.replace('-', "_"))
        .with_extension("wasm");
    let core = std::fs::read(&artifact)
        .map_err(|error| Fault::invalid_data(format!("guest artifact: {error}")))?;
    torse_mod_host::durable_dispatch::encode_guest(&core).map_err(Fault::invalid_data)
}

/// Loads the packages, assembles the machine demo world, and adapts every
/// behavior package into a durable rule. `stock_input` controls whether the
/// input chest starts with one stone; `build` selects the guest variant.
pub fn mod_world(
    dirs: &[PathBuf],
    stock_input: bool,
    spin: &SpinPackages,
) -> Result<ModWorld, Fault> {
    mod_world_with_layout(dirs, stock_input, spin, ModLayout::default())
}

pub fn mod_world_with_layout(
    dirs: &[PathBuf],
    stock_input: bool,
    spin: &SpinPackages,
    layout: ModLayout,
) -> Result<ModWorld, Fault> {
    if !(1..=64).contains(&layout.machines_per_package) {
        return Err(Fault::invalid_data(
            "machine count must be 1..64 per package",
        ));
    }
    let mut base = torse_content::m0_fixture_pack();
    torse_sim::block_actions::register_content(&mut base);
    torse_inventory::register_content(&mut base);
    let set = resolve(dirs, &base).map_err(|errors| Fault::invalid_data(format!("{errors:?}")))?;

    let catalog = ItemCatalog::new([(bootstrap::key("base:stone"), 64)])
        .map_err(|e| Fault::invalid_data(format!("{e:?}")))?;
    let compatibility = catalog.bind_compatibility(Compatibility {
        authority_schema: 4,
        rules_digest: set.content.authority_digest,
        content_digest: set.content.presentation_digest,
    });
    let mut schemas = torse_world::ComponentSchemas::build(&set.content);
    catalog
        .bind_schemas(&mut schemas)
        .map_err(|error| Fault::invalid_data(error.to_string()))?;
    schemas
        .validate_block_defaults()
        .map_err(|error| Fault::invalid_data(error.to_string()))?;
    let (schemas, compatibility) = schemas
        .bind_compatibility(compatibility)
        .map_err(|error| Fault::invalid_data(error.to_string()))?;
    let mut handlers = torse_sim::block_actions::handlers();
    handlers.extend(torse_inventory::handlers(catalog.clone()));
    let mut descriptors = torse_sim::block_actions::descriptors();
    descriptors.extend(torse_inventory::descriptors());
    let actions = torse_sim::action::ActionService::bind(descriptors, handlers, &schemas)
        .map_err(|e| Fault::invalid_data(e.to_string()))?;

    let mut world = WorldAuthority::new(
        bootstrap::WORLD,
        torse_world::BlockStateRegistry::build(&set.content)
            .map_err(|e| Fault::invalid_data(format!("{e:?}")))?,
        schemas.clone(),
        16,
        WorldBounds::new(0, 32).expect("bounds"),
        compatibility,
    )
    .map_err(|e| Fault::invalid_data(format!("{e:?}")))?;
    for pos in bootstrap::scope().sections {
        world
            .store_mut()
            .request(pos)
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        world
            .store_mut()
            .install(pos, Section::empty(16).expect("side"))
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        world
            .store_mut()
            .finish_load(pos)
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
    }

    // Chests first so machine components can reference their identities.
    let mut ids = [EntityId(0); 2];
    for (index, id) in ids.iter_mut().enumerate() {
        let position = Position::new(2.0 + index as f64, 2.0, 2.0);
        let handle = world
            .entities_mut()
            .spawn(position)
            .map_err(|e| Fault::invalid_data(format!("{e:?}")))?;
        *id = world.entities().get(handle).expect("entity").id;
    }
    let [input, output] = ids;
    for (entity, stock) in [(input, stock_input), (output, false)] {
        let handle = world.entities().handle_of(entity).expect("entity");
        let mut inventory = torse_inventory::Inventory::new(2)
            .map_err(|e| Fault::invalid_data(format!("{e:?}")))?;
        if stock {
            inventory
                .set(
                    0,
                    Some(ItemStack {
                        data: Default::default(),
                        item: bootstrap::key("base:stone"),
                        count: 1,
                    }),
                    &catalog,
                )
                .map_err(|e| Fault::invalid_data(format!("{e:?}")))?;
        }
        world
            .entities_mut()
            .components_mut(handle)
            .expect("components")
            .insert(&schemas, &torse_inventory::schema(), inventory.fields())
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
    }

    let stats = Rc::new(RefCell::new(ModStats::default()));
    let mut rules: Vec<Box<dyn DurableRule>> = Vec::new();
    let mut machines: Vec<(String, EntityId)> = Vec::new();
    for package in &set.packages {
        let principal = package.manifest.principal();
        let owned = set
            .owned_schemas
            .get(&principal)
            .cloned()
            .unwrap_or_default();
        if package.manifest.server_entry.is_none() {
            continue;
        }
        let mut extra_targets = Vec::new();
        if !owned.is_empty() {
            for copy in 0..layout.machines_per_package {
                let index = machines.len();
                let position = if layout.machines_per_package == 1 {
                    Position::new(4.0 + index as f64, 2.0, 2.0)
                } else {
                    Position::new(
                        4.0 + (index % 7) as f64 * 4.0,
                        2.0,
                        2.0 + (index / 7) as f64 * 1.5,
                    )
                };
                let ports = if layout.shared_ports || (copy == 0 && machines.is_empty()) {
                    ids
                } else {
                    additional_ports(&mut world, &catalog, position, stock_input)?
                };
                extra_targets.extend(ports);
                let handle = world
                    .entities_mut()
                    .spawn(position)
                    .map_err(|e| Fault::invalid_data(format!("{e:?}")))?;
                let machine = world.entities().get(handle).expect("entity").id;
                let fields = ComponentFields::from([
                    (
                        ContentKey::parse(&format!("{}:field/progress", package.manifest.id))
                            .expect("field"),
                        ComponentValue::U32(0),
                    ),
                    (
                        ContentKey::parse(&format!("{}:field/input", package.manifest.id))
                            .expect("field"),
                        ComponentValue::Entity(ports[0]),
                    ),
                    (
                        ContentKey::parse(&format!("{}:field/output", package.manifest.id))
                            .expect("field"),
                        ComponentValue::Entity(ports[1]),
                    ),
                ]);
                world
                    .entities_mut()
                    .components_mut(handle)
                    .expect("components")
                    .insert(&schemas, &owned[0], fields)
                    .map_err(|e| Fault::invalid_data(e.to_string()))?;
                machines.push((package.manifest.id.clone(), machine));
                extra_targets.push(machine);
            }
        }
        let bytes = build_guest_bytes(package, spin.contains(&package.manifest.id))?;
        let extras: Vec<String> = package
            .manifest
            .permissions
            .iter()
            .map(|permission| permission.to_string())
            .chain([format!("{}:module", package.manifest.id)])
            .chain([bootstrap::key("base:stone").to_string()])
            .collect();
        let symbols = Arc::new(SymbolTable::build(&set.content, &extras));
        let guest = DurableGuest::new(&bytes, Arc::clone(&symbols), CallBudget::prototype())
            .map_err(Fault::invalid_data)?;
        let mut actor = ActorContext::new(principal.clone());
        for permission in &package.manifest.permissions {
            actor = actor.permission(permission.clone());
        }
        for entity in ids.iter().chain(&extra_targets) {
            actor = actor.target(ActionTarget::Entity(*entity));
        }
        rules.push(Box::new(ModRule {
            package_id: package.manifest.id.clone(),
            package: principal,
            rule: ContentKey::parse(&format!("{}:rule/behavior", package.manifest.id))
                .expect("rule key"),
            owned_schemas: owned,
            symbols,
            catalog: catalog.clone(),
            actor,
            stats: Rc::clone(&stats),
            inner: RefCell::new(RuleInner { guest }),
        }));
    }

    Ok(ModWorld {
        world,
        actions,
        rules,
        stats,
        machines,
        input,
        output,
    })
}
