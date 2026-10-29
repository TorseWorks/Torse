//! Authority bridge for behavior mods (P2).
//!
//! This module connects the component boundary to the real authority: the
//! three import interfaces read through `WorldAuthority`, and returned plans
//! are validated, converted into transactions, and committed through the
//! coordinator. Cross-module plans cannot share one transaction because the
//! coordinator enforces component ownership per source, so a plan becomes one
//! transaction per module source that is prepared entirely before anything is
//! published; any rejection drops the whole plan.

use std::collections::{BTreeMap, BTreeSet};

use exports::torse::behavior::invoke::Event;
pub use exports::torse::behavior::invoke::{
    Event as InvokeEvent, Invocation, ObjectRef, OwnedState, Plan, WakeupTarget,
};
use torse::behavior::diagnostics::LogLevel;
use torse::behavior::query::{
    CellSample, ComponentSnapshot, ComponentTarget, EntityPage, EntitySample, PortEstimate,
    QueryError,
};
use torse::behavior::schedule::WakeupSlot;
pub use torse::behavior::types::EntityId as WitEntityId;
use torse::behavior::types::{Atom, Bbox, BlockRef, Cell, Field, Vec3i};
use torse_content::{
    ComponentField, ComponentFieldType, ComponentSchema as ContentSchema, ContentPack,
    PersistencePolicy, compile, m0_fixture_pack,
};
use torse_inventory::{
    Inventory, ItemCatalog, ItemStack, Transfer, plan_transfers,
    register_content as register_inventory,
};
use torse_sim::action::{ActionTarget, ActorContext, register_action_module};
use torse_sim::{OrderingKey, Phase, Transaction, TransactionGroup, WorldAuthority, block_actions};
use torse_types::{
    BlockPos, CommitSeq, Compatibility, ContentKey, EntityId, SectionPos, TransactionId, WorldId,
};
use torse_world::{
    BlockStateId, ComponentEdit, ComponentFields, ComponentSchemas, ComponentValue, Position,
    Section, TerrainQuery, WorldBounds,
};
use wasmtime::component::Component;
use wasmtime::component::bindgen;
use wasmtime::{Engine, Store};

bindgen!({
    path: "../mod_api/wit/behavior.wit",
    world: "mod-behavior",
});

use crate::budget::{GuestLimits, HostBudget};
use crate::component::{CallBudget, Meters};

crate::budget::boundary_sizes!();
crate::values::boundary_values!();

/// Consecutive plan rejections or traps before the handler is deactivated.
const FAILURE_THRESHOLD: u32 = 8;

/// One dispatch result as observed by the host driver.
#[derive(Clone, Debug, PartialEq)]
pub enum DispatchOutcome {
    /// Every transaction in the plan was prepared and published.
    Committed { transactions: u32, seq: u64 },
    /// The guest returned no plan.
    NoPlan,
    /// The plan failed validation or a prepare rejected it; nothing committed.
    Rejected { reason: String },
    /// The guest trapped or exhausted its budget; the instance was rebuilt.
    Trapped { reason: String },
    /// The failure threshold was reached; dispatch is refused.
    Deactivated,
}

/// Assembly configuration for the P2 machine scenario.
pub struct ScenarioConfig {
    /// Package principal; also the transaction source and component owner.
    pub package: &'static str,
    /// Component schema owned by the package.
    pub schema: &'static str,
    /// Whether the input chest starts with one stone.
    pub stock_input: bool,
    /// Host permissions granted to the package.
    pub granted_permissions: &'static [&'static str],
}

/// Result of assembling the machine scenario world.
pub struct MachineWorld {
    pub host: AuthorityHost,
    pub catalog: ItemCatalog,
    pub machine: EntityId,
    pub input: EntityId,
    pub output: EntityId,
}

fn key(value: &str) -> ContentKey {
    ContentKey::parse(value).expect("scenario key parses")
}

/// Per-package grant assembled at load time: the actor identity the host
/// vouches for and the schemas this package owns.
pub struct PackageGrant {
    pub actor: ActorContext,
    pub owned_schemas: Vec<ContentKey>,
}

/// The host side of one or more behavior packages bound to a real authority.
/// All packages share one world; symbols are global, grants are per package.
pub struct AuthorityHost {
    authority: WorldAuthority,
    catalog: ItemCatalog,
    symbols: BTreeMap<String, u32>,
    symbol_keys: Vec<String>,
    packages: BTreeMap<ContentKey, PackageGrant>,
    active_package: Option<ContentKey>,
    // Wakeup entries remember the package that scheduled them so dispatch
    // returns to the right instance.
    wakeups: Vec<(u64, EntityId, String, ContentKey)>,
    pub meters: Meters,
    read_positions: BTreeSet<BlockPos>,
    consecutive_failures: BTreeMap<ContentKey, u32>,
    deactivated: BTreeSet<ContentKey>,
    transaction_counter: u128,
    dispatch_local: u32,
    budget: HostBudget,
    guest_limits: GuestLimits,
}

impl AuthorityHost {
    /// Registers one package's grant; principal doubles as transaction source.
    pub fn register_package(
        &mut self,
        principal: ContentKey,
        actor: ActorContext,
        owned_schemas: Vec<ContentKey>,
    ) {
        self.packages.insert(
            principal,
            PackageGrant {
                actor,
                owned_schemas,
            },
        );
    }
}

impl AuthorityHost {
    /// Assembles a host around a live authority; symbols are interned by the
    /// caller from the compiled content.
    pub fn new(
        authority: WorldAuthority,
        catalog: ItemCatalog,
        symbols: BTreeMap<String, u32>,
        symbol_keys: Vec<String>,
    ) -> Self {
        Self {
            authority,
            catalog,
            symbols,
            symbol_keys,
            packages: BTreeMap::new(),
            active_package: None,
            wakeups: Vec::new(),
            meters: Meters::default(),
            read_positions: BTreeSet::new(),
            consecutive_failures: BTreeMap::new(),
            deactivated: BTreeSet::new(),
            transaction_counter: 0,
            dispatch_local: 0,
            budget: HostBudget::new(CallBudget::prototype()),
            guest_limits: GuestLimits::new(CallBudget::prototype()),
        }
    }

    /// Read-only access to the wrapped authority.
    pub fn authority(&self) -> &WorldAuthority {
        &self.authority
    }

    /// Mutable access for scenario setup such as restocking a chest.
    pub fn authority_mut(&mut self) -> &mut WorldAuthority {
        &mut self.authority
    }

    /// Current commit sequence of the wrapped authority.
    pub fn seq(&self) -> CommitSeq {
        self.authority.seq()
    }

    /// Whether the package handler has been deactivated by repeated failures.
    pub fn is_deactivated(&self, package: &ContentKey) -> bool {
        self.deactivated.contains(package)
    }

    /// Consecutive failure count for a package since its last committed plan.
    pub fn consecutive_failures(&self, package: &ContentKey) -> u32 {
        self.consecutive_failures.get(package).copied().unwrap_or(0)
    }

    /// Registers a wakeup directly; used to seed the first machine tick.
    pub fn seed_wakeup(&mut self, tick: u64, entity: EntityId, key: &str, package: ContentKey) {
        self.wakeups.push((tick, entity, key.to_string(), package));
    }

    fn symbol_of(&self, key: &ContentKey) -> Option<u32> {
        self.symbols.get(&key.to_string()).copied()
    }

    fn key_of(&self, symbol: u32) -> Option<ContentKey> {
        // Symbols are 1-based indices into the key table.
        let stored = self.symbol_keys.get(symbol.checked_sub(1)? as usize)?;
        ContentKey::parse(stored).ok()
    }

    pub fn begin_invocation(&mut self) {
        self.read_positions.clear();
        self.budget.reset();
    }

    /// Select a registered identity before initialization or dispatch. Only the
    /// trusted runtime can select it; it is absent from the guest interface.
    pub(crate) fn bind_package(&mut self, package: &ContentKey) -> Result<(), String> {
        self.active_package = None;
        if !self.packages.contains_key(package) {
            return Err(format!("no grant for package {package}"));
        }
        self.active_package = Some(package.clone());
        Ok(())
    }

    fn active_grant(&self) -> Option<&PackageGrant> {
        self.packages.get(self.active_package.as_ref()?)
    }

    fn allows_entity(&self, entity: EntityId) -> bool {
        self.active_grant()
            .is_some_and(|grant| grant.actor.allows(ActionTarget::Entity(entity)))
    }

    fn allows_component(&self, entity: EntityId, schema: &ContentKey) -> bool {
        self.active_grant().is_some_and(|grant| {
            grant.actor.allows(ActionTarget::Entity(entity))
                && (grant.owned_schemas.contains(schema)
                    || (*schema == torse_inventory::schema()
                        && grant.actor.permits(&key("inventory:permission/read"))))
        })
    }

    pub(crate) fn configure_budget(&mut self, budget: CallBudget) {
        self.budget = HostBudget::new(budget);
        self.guest_limits = GuestLimits::new(budget);
    }

    pub(crate) fn bounded_store(
        engine: &Engine,
        mut host: Self,
        budget: CallBudget,
    ) -> Result<Store<Self>, String> {
        host.configure_budget(budget);
        let mut store = Store::new(engine, host);
        store.limiter(|host| &mut host.guest_limits);
        store.set_hostcall_fuel(budget.copy_fuel());
        store
            .set_fuel(budget.fuel)
            .map_err(|error| format!("fuel setup failed: {error}"))?;
        Ok(store)
    }

    pub(crate) fn prepare_invocation(
        &mut self,
        tick: u64,
        event: &Event,
        package: &ContentKey,
    ) -> Option<Invocation> {
        // Reserve owned state before cloning it into the boundary value.
        if self.active_package.as_ref() != Some(package) {
            return None;
        }
        if let Event::Wakeup(target) = event {
            if !self.allows_entity(target.entity.to_entity_id()) {
                return None;
            }
            self.budget.output(target.key.len() as u64).ok()?;
            if let Some(handle) = self
                .authority
                .entities()
                .handle_of(target.entity.to_entity_id())
                && let Some(entity) = self.authority.entities().get(handle)
                && let Some(grant) = self.packages.get(package)
            {
                for schema in &grant.owned_schemas {
                    self.budget.work(1).ok()?;
                    if let Some(fields) = entity.components.component(schema)
                        && let Some(bytes) = crate::budget::flat_source_size(fields)
                    {
                        self.budget.output(bytes).ok()?;
                    }
                }
            }
        }
        let invocation = self.build_invocation(tick, event, package);
        self.budget.output(invocation_size(&invocation)).ok()?;
        Some(invocation)
    }

    pub(crate) fn budget_exceeded(&self) -> bool {
        self.budget.exceeded()
    }

    pub(crate) fn plan_fits(&mut self, plan: Option<&Plan>) -> bool {
        self.budget.input(plan.map_or(0, plan_size)).is_ok()
    }

    pub fn drain_due_wakeups(&mut self, tick: u64) -> Vec<(EntityId, String, ContentKey)> {
        let mut due = Vec::new();
        let mut remaining = Vec::new();
        for (at, entity, key, package) in std::mem::take(&mut self.wakeups) {
            if at <= tick {
                due.push((entity, key, package));
            } else {
                remaining.push((at, entity, key, package));
            }
        }
        self.wakeups = remaining;
        due
    }

    pub fn build_invocation(&self, tick: u64, event: &Event, package: &ContentKey) -> Invocation {
        let self_ = match event {
            Event::Wakeup(target) => self.build_object_ref(&target.entity, package),
            _ => None,
        };
        Invocation {
            event: event.clone(),
            tick,
            self_,
            rng_seed: tick.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x4d5a_9595,
        }
    }

    fn build_object_ref(
        &self,
        wit_entity: &WitEntityId,
        package: &ContentKey,
    ) -> Option<ObjectRef> {
        let entity = wit_entity.to_entity_id();
        let grant = self.packages.get(package)?;
        if !grant.actor.allows(ActionTarget::Entity(entity)) {
            return None;
        }
        let handle = self.authority.entities().handle_of(entity)?;
        let stored = self.authority.entities().get(handle)?;
        let position = stored.position;
        let pos = Vec3i {
            x: position.x.floor() as i64,
            y: position.y.floor() as i64,
            z: position.z.floor() as i64,
        };
        let block_pos = BlockPos::new(pos.x, pos.y, pos.z);
        let terrain = if grant
            .actor
            .allows_block(block_pos, self.authority.store().side())
        {
            self.authority.store().query(block_pos)
        } else {
            TerrainQuery::NotLoaded
        };
        let block = match terrain {
            TerrainQuery::Ready(state) => self
                .authority
                .registry()
                .definition_key(state)
                .and_then(|block| self.symbol_of(block))
                .unwrap_or(0),
            _ => 0,
        };
        let owned_schemas = self
            .packages
            .get(package)
            .map(|grant| grant.owned_schemas.clone())
            .unwrap_or_default();
        let mut owned = Vec::new();
        for schema in &owned_schemas {
            if let Some(fields) = stored.components.component(schema)
                && let Some(flat) = self.fields_to_flat(fields)
            {
                owned.push(OwnedState {
                    schema: self.symbol_of(schema).unwrap_or(0),
                    fields: Some(flat),
                });
            }
        }
        Some(ObjectRef {
            entity: *wit_entity,
            pos,
            block,
            owned,
        })
    }

    fn fields_to_flat(&self, fields: &ComponentFields) -> Option<Vec<Field>> {
        if crate::budget::flat_source_size(fields)? > self.budget.limits.max_output_bytes {
            return None;
        }
        encode_fields(fields, |key| self.symbol_of(key))
    }

    fn flat_to_fields(&self, flat: &[Field]) -> Result<ComponentFields, String> {
        decode_fields(flat, |symbol| self.key_of(symbol))
    }

    fn cell_to_state(&self, cell: &Cell) -> Result<BlockStateId, String> {
        match cell {
            Cell::Empty => Ok(BlockStateId::EMPTY),
            Cell::NotLoaded => Err("plan must not write not-loaded".to_string()),
            Cell::Occupied(block) => {
                let definition = self
                    .key_of(block.definition)
                    .ok_or("unknown block symbol")?;
                self.authority
                    .registry()
                    .state_of(&definition, &block.state_values)
                    .ok_or_else(|| "unknown block state".to_string())
            }
        }
    }

    fn next_transaction_id(&mut self) -> TransactionId {
        self.transaction_counter += 1;
        TransactionId(self.transaction_counter)
    }

    fn ordering(&mut self, stable_task_key: u64) -> OrderingKey {
        let local = self.dispatch_local;
        self.dispatch_local = self.dispatch_local.wrapping_add(1);
        OrderingKey {
            phase: Phase::Simulation,
            rule_priority: 0,
            stable_task_key,
            local_sequence: local,
        }
    }

    /// Validates a returned plan and commits it through the coordinator.
    pub fn apply_plan(
        &mut self,
        plan: &Plan,
        package: &ContentKey,
    ) -> Result<DispatchOutcome, String> {
        if !self.plan_fits(Some(plan)) {
            return Ok(DispatchOutcome::Rejected {
                reason: "host input budget exceeded".to_string(),
            });
        }
        let mut transactions: Vec<Transaction> = Vec::new();
        let mut machine_tx = Transaction::new(
            self.next_transaction_id(),
            package.clone(),
            self.ordering(0),
        );
        let mut has_machine_work = false;

        for edit in &plan.edits {
            has_machine_work = true;
            let pos = BlockPos::new(edit.pos.x, edit.pos.y, edit.pos.z);
            let expected = match &edit.expected {
                Some(cell) => self.cell_to_state(cell)?,
                None => {
                    if !self.read_positions.contains(&pos) {
                        return Ok(DispatchOutcome::Rejected {
                            reason: format!(
                                "edit at {pos:?} has no expected state and was not read this invocation"
                            ),
                        });
                    }
                    match self.authority.store().query(pos) {
                        TerrainQuery::Ready(state) => state,
                        _ => {
                            return Ok(DispatchOutcome::Rejected {
                                reason: format!("edit at {pos:?} targets unreadable terrain"),
                            });
                        }
                    }
                }
            };
            let new = self.cell_to_state(&edit.new)?;
            machine_tx.read_block(pos, expected);
            machine_tx.write_block(pos, new);
        }

        for edit in &plan.component_edits {
            has_machine_work = true;
            let schema = self.key_of(edit.schema).ok_or("unknown schema symbol")?;
            if let Some(declared) = self.authority.schemas().get(&schema) {
                if declared.owner_module != *package {
                    return Ok(DispatchOutcome::Rejected {
                        reason: format!("schema {schema} is owned by {}", declared.owner_module),
                    });
                }
            } else {
                return Ok(DispatchOutcome::Rejected {
                    reason: format!("schema {schema} is not registered"),
                });
            }
            let entity = edit.entity.to_entity_id();
            if self.authority.entities().handle_of(entity).is_none() {
                return Ok(DispatchOutcome::Rejected {
                    reason: format!("entity {entity:?} does not exist"),
                });
            }
            let previous = match &edit.previous {
                Some(flat) => Some(self.flat_to_fields(flat)?),
                None => None,
            };
            let fields = match &edit.fields {
                Some(flat) => Some(self.flat_to_fields(flat)?),
                None => None,
            };
            machine_tx.components.push(ComponentEdit {
                id: entity,
                schema,
                previous,
                fields,
            });
        }

        let actor = &self.packages.get(package).ok_or("unknown package")?.actor;
        if plan.spawns.len().saturating_add(plan.despawns.len()) > 256
            || (!plan.spawns.is_empty() && !actor.permits(&torse_sim::action::spawn_permission()))
            || (!plan.despawns.is_empty()
                && !actor.permits(&torse_sim::action::despawn_permission()))
        {
            return Ok(DispatchOutcome::Rejected {
                reason: "lifecycle permission or count rejected".into(),
            });
        }
        for spawn in &plan.spawns {
            let pos = BlockPos::new(spawn.pos.x, spawn.pos.y, spawn.pos.z);
            if !actor.allows_block(pos, self.authority.store().side()) {
                return Ok(DispatchOutcome::Rejected {
                    reason: "spawn scope denied".into(),
                });
            }
            let mut components = BTreeMap::new();
            for component in &spawn.components {
                let schema = self
                    .key_of(component.schema)
                    .ok_or("unknown spawn schema")?;
                if self
                    .authority
                    .schemas()
                    .get(&schema)
                    .is_none_or(|s| s.owner_module != *package)
                {
                    return Ok(DispatchOutcome::Rejected {
                        reason: "foreign spawn schema".into(),
                    });
                }
                let fields = self
                    .flat_to_fields(component.fields.as_deref().ok_or("missing spawn fields")?)?;
                if components.insert(schema, fields).is_some() {
                    return Ok(DispatchOutcome::Rejected {
                        reason: "duplicate spawn schema".into(),
                    });
                }
            }
            machine_tx.spawn_entity(
                Position::new(pos.x as f64, pos.y as f64, pos.z as f64),
                components,
            );
            has_machine_work = true;
        }
        for entity in &plan.despawns {
            let view = actor.view(&self.authority);
            let (handle, previous) = match view.entity(entity.to_entity_id()) {
                Ok(value) => value,
                Err(_) => {
                    return Ok(DispatchOutcome::Rejected {
                        reason: "despawn target denied".into(),
                    });
                }
            };
            machine_tx.despawn_entity(handle, previous);
            has_machine_work = true;
        }
        for edit in &plan.component_edits {
            if !actor.allows(ActionTarget::Entity(edit.entity.to_entity_id())) {
                return Ok(DispatchOutcome::Rejected {
                    reason: "component target denied".into(),
                });
            }
        }
        for edit in &plan.edits {
            if !actor.permits(&key("world:permission/edit"))
                || !actor.allows_block(
                    BlockPos::new(edit.pos.x, edit.pos.y, edit.pos.z),
                    self.authority.store().side(),
                )
            {
                return Ok(DispatchOutcome::Rejected {
                    reason: "block edit denied".into(),
                });
            }
        }
        for wakeup in &plan.wakeups {
            if !actor.allows(ActionTarget::Entity(wakeup.entity.to_entity_id())) {
                return Ok(DispatchOutcome::Rejected {
                    reason: "wakeup target denied".into(),
                });
            }
        }

        if has_machine_work {
            transactions.push(machine_tx);
        }

        if !plan.transfers.is_empty() {
            let transfers: Vec<_> = plan
                .transfers
                .iter()
                .map(|only| Transfer {
                    source: only.source.to_entity_id(),
                    source_slot: only.source_slot as usize,
                    destination: only.destination.to_entity_id(),
                    destination_slot: only.destination_slot as usize,
                    count: only.count,
                })
                .collect();
            let transaction_id = self.next_transaction_id();
            let ordering = self.ordering(0);
            let actor = &self.packages.get(package).ok_or("unknown package")?.actor;
            match plan_transfers(
                &actor.view(&self.authority),
                &self.catalog,
                &transfers,
                transaction_id,
                ordering,
            ) {
                Ok(tx) => transactions.push(tx),
                Err(code) => {
                    return Ok(DispatchOutcome::Rejected {
                        reason: format!("transfer rejected: {code:?}"),
                    });
                }
            }
        }

        let transaction_count = transactions.len() as u32;
        if transactions.is_empty() {
            for wakeup in &plan.wakeups {
                let entity = wakeup.entity.to_entity_id();
                self.wakeups
                    .push((wakeup.tick, entity, wakeup.key.clone(), package.clone()));
            }
            return Ok(DispatchOutcome::Committed {
                transactions: 0,
                seq: self.authority.seq().0,
            });
        }

        // Transactions in one plan must touch disjoint targets; the ownership
        // rule already separates component schemas, so this catches duplicate
        // work early instead of failing between two publishes.
        let mut touched = BTreeSet::new();
        for transaction in &transactions {
            for edit in &transaction.components {
                if !touched.insert((edit.id, edit.schema.clone())) {
                    return Ok(DispatchOutcome::Rejected {
                        reason: "plan edits the same component twice".to_string(),
                    });
                }
            }
        }

        let id = self.next_transaction_id();
        let ordering = transactions[0].ordering;
        let group = TransactionGroup::new(id, ordering, transactions)
            .map_err(|e| format!("group: {e:?}"))?;
        let prepared = match self.authority.prepare_group(&group) {
            Ok(change) => change,
            Err(error) => {
                return Ok(DispatchOutcome::Rejected {
                    reason: format!("prepare rejected: {error:?}"),
                });
            }
        };
        self.authority
            .publish(prepared)
            .map_err(|e| format!("publish fault: {e:?}"))?;
        self.consecutive_failures.insert(package.clone(), 0);
        for wakeup in &plan.wakeups {
            let entity = wakeup.entity.to_entity_id();
            self.wakeups
                .push((wakeup.tick, entity, wakeup.key.clone(), package.clone()));
        }
        Ok(DispatchOutcome::Committed {
            transactions: transaction_count,
            seq: self.authority.seq().0,
        })
    }

    pub fn on_failure(&mut self, tick: u64, event: &Event, package: &ContentKey) {
        let failures = self
            .consecutive_failures
            .entry(package.clone())
            .or_insert(0);
        *failures += 1;
        if *failures >= FAILURE_THRESHOLD {
            self.deactivated.insert(package.clone());
        } else if let Event::Wakeup(target) = event {
            let entity = target.entity.to_entity_id();
            self.wakeups
                .push((tick + 1, entity, target.key.clone(), package.clone()));
        }
    }
}

impl WitEntityId {
    fn to_entity_id(self) -> EntityId {
        EntityId((self.high as u128) << 64 | self.low as u128)
    }

    pub fn from_entity_id(entity: EntityId) -> Self {
        Self {
            low: entity.0 as u64,
            high: (entity.0 >> 64) as u64,
        }
    }
}

impl torse::behavior::types::Host for AuthorityHost {}

impl torse::behavior::query::Host for AuthorityHost {
    fn resolve_symbol(&mut self, key: String) -> Option<u32> {
        self.budget.enter(key.len() as u64).ok()?;
        self.meters.resolve_calls = self.meters.resolve_calls.saturating_add(1);
        self.budget.output(4).ok()?;
        self.symbols.get(&key).copied()
    }

    fn symbol_key(&mut self, symbol: u32) -> Option<String> {
        self.budget.enter(4).ok()?;
        let key = self.symbol_keys.get(symbol.checked_sub(1)? as usize)?;
        self.budget.output(key.len() as u64).ok()?;
        Some(key.clone())
    }

    fn query_blocks(&mut self, region: Bbox) -> Result<Vec<CellSample>, QueryError> {
        self.meters.query_block_calls = self.meters.query_block_calls.saturating_add(1);
        let count = self
            .budget
            .region(
                [region.min.x, region.min.y, region.min.z],
                [region.max.x, region.max.y, region.max.z],
                self.budget.limits.max_cells_per_query,
            )
            .map_err(|_| QueryError::BudgetExceeded)?;
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut samples = Vec::new();
        for y in region.min.y..region.max.y {
            for z in region.min.z..region.max.z {
                for x in region.min.x..region.max.x {
                    let pos = BlockPos::new(x, y, z);
                    let allowed = self.active_grant().is_some_and(|grant| {
                        grant.actor.allows_block(pos, self.authority.store().side())
                    });
                    let terrain = if allowed {
                        self.read_positions.insert(pos);
                        self.authority.store().query(pos)
                    } else {
                        TerrainQuery::NotLoaded
                    };
                    let cell = match terrain {
                        TerrainQuery::Ready(BlockStateId::EMPTY) => Cell::Empty,
                        TerrainQuery::Ready(state) => {
                            let definition = self
                                .authority
                                .registry()
                                .definition_key(state)
                                .and_then(|block| self.symbol_of(block))
                                .unwrap_or(0);
                            let values = self
                                .authority
                                .registry()
                                .state_values(state)
                                .unwrap_or_default();
                            self.budget
                                .output((values.len() as u64).saturating_mul(2))
                                .map_err(|_| QueryError::BudgetExceeded)?;
                            Cell::Occupied(BlockRef {
                                definition,
                                state_values: values.to_vec(),
                            })
                        }
                        TerrainQuery::Exterior => Cell::Empty,
                        TerrainQuery::NotLoaded | TerrainQuery::OutOfBounds => Cell::NotLoaded,
                    };
                    self.budget
                        .output(std::mem::size_of::<CellSample>() as u64)
                        .map_err(|_| QueryError::BudgetExceeded)?;
                    samples.push(CellSample {
                        pos: Vec3i { x, y, z },
                        cell,
                    });
                }
            }
        }
        self.meters.cells_returned = self
            .meters
            .cells_returned
            .saturating_add(samples.len() as u32);
        Ok(samples)
    }

    fn query_entities(
        &mut self,
        region: Bbox,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<EntityPage, QueryError> {
        self.budget
            .enter(52 + cursor.as_ref().map_or(0, |value| value.len() as u64))
            .map_err(|_| QueryError::BudgetExceeded)?;
        if limit > self.budget.limits.max_cells_per_query {
            return self.budget.reject().map_err(|_| QueryError::BudgetExceeded);
        }
        let skip = match &cursor {
            None => 0_usize,
            Some(raw) => raw
                .parse::<usize>()
                .map_err(|_| QueryError::InvalidCursor)?,
        };
        let mut items = Vec::new();
        let mut index = 0_usize;
        self.budget
            .work(self.authority.entities().allocated_slots() as u64)
            .map_err(|_| QueryError::BudgetExceeded)?;
        for (_, entity) in self.authority.entities().iter() {
            if !self.allows_entity(entity.id) {
                continue;
            }
            let position = entity.position;
            let x = position.x.floor() as i64;
            let y = position.y.floor() as i64;
            let z = position.z.floor() as i64;
            if x < region.min.x
                || x >= region.max.x
                || y < region.min.y
                || y >= region.max.y
                || z < region.min.z
                || z >= region.max.z
            {
                continue;
            }
            if index >= skip && items.len() < limit as usize {
                self.budget
                    .output(40)
                    .map_err(|_| QueryError::BudgetExceeded)?;
                items.push(EntitySample {
                    id: WitEntityId::from_entity_id(entity.id),
                    position: Vec3i { x, y, z },
                });
            }
            index += 1;
        }
        let next = if index > skip.saturating_add(items.len()) {
            Some(skip.saturating_add(items.len()).to_string())
        } else {
            None
        };
        Ok(EntityPage { items, next })
    }

    fn read_components(
        &mut self,
        targets: Vec<ComponentTarget>,
    ) -> Result<Vec<ComponentSnapshot>, QueryError> {
        self.budget
            .enter((targets.len() as u64).saturating_mul(24))
            .and_then(|_| self.budget.work(targets.len() as u64))
            .map_err(|_| QueryError::BudgetExceeded)?;
        let mut snapshots = Vec::with_capacity(targets.len());
        for target in targets {
            self.budget
                .output(40)
                .map_err(|_| QueryError::BudgetExceeded)?;
            let schema = self
                .key_of(target.schema)
                .ok_or(QueryError::InvalidCursor)?; // unknown symbol
            let entity = target.entity.to_entity_id();
            let fields = self
                .allows_component(entity, &schema)
                .then(|| {
                    self.authority
                        .entities()
                        .handle_of(entity)
                        .and_then(|handle| self.authority.entities().get(handle))
                        .and_then(|entity| entity.components.component(&schema))
                })
                .flatten();
            if let Some(fields) = fields {
                self.budget
                    .work(fields.len() as u64)
                    .map_err(|_| QueryError::BudgetExceeded)?;
                self.budget
                    .output(crate::budget::flat_source_size(fields).ok_or(QueryError::Unsupported)?)
                    .map_err(|_| QueryError::BudgetExceeded)?;
            }
            let flat = match fields {
                Some(fields) => Some(self.fields_to_flat(fields).ok_or(QueryError::Unsupported)?),
                None => None,
            };
            snapshots.push(ComponentSnapshot {
                entity: target.entity,
                fields: flat,
            });
        }
        Ok(snapshots)
    }

    fn probe_item_port(&mut self, entity: WitEntityId, port: String) -> Option<PortEstimate> {
        self.budget.enter(16 + port.len() as u64).ok()?;
        self.budget.work(torse_inventory::MAX_SLOTS as u64).ok()?;
        let entity = entity.to_entity_id();
        if !self.allows_entity(entity)
            || !self
                .active_grant()
                .is_some_and(|grant| grant.actor.permits(&key("inventory:permission/read")))
        {
            return None;
        }
        let fields = self
            .authority
            .entities()
            .handle_of(entity)
            .and_then(|handle| self.authority.entities().get(handle))
            .and_then(|entity| entity.components.component(&torse_inventory::schema()))?;
        let inventory = Inventory::from_fields(fields, &self.catalog).ok()?;
        let mut accepts = BTreeSet::new();
        let mut free = 0_u32;
        for slot in inventory.slots() {
            match slot {
                Some(stack) => {
                    accepts.insert(stack.item.to_string());
                }
                None => free += 1,
            }
        }
        let accepts = accepts
            .into_iter()
            .filter_map(|key| self.symbols.get(&key).copied())
            .collect::<Vec<_>>();
        self.budget.output(16 + accepts.len() as u64 * 4).ok()?;
        Some(PortEstimate {
            accepts,
            free_estimate: free,
        })
    }
}

impl torse::behavior::diagnostics::Host for AuthorityHost {
    fn log(&mut self, level: LogLevel, message: String) {
        if self.budget.enter(4 + message.len() as u64).is_err()
            || self.budget.logs >= self.budget.limits.max_log_entries
        {
            self.meters.log_dropped = self.meters.log_dropped.saturating_add(1);
            return;
        }
        self.budget.logs += 1;
        self.meters.log_entries = self.meters.log_entries.saturating_add(1);
        let _ = (level, message);
    }
}

impl torse::behavior::schedule::Host for AuthorityHost {
    fn pending_wakeups(&mut self, key_prefix: Option<String>) -> Vec<WakeupSlot> {
        if self
            .budget
            .enter(key_prefix.as_ref().map_or(0, |key| key.len() as u64))
            .is_err()
            || self.budget.work(self.wakeups.len() as u64).is_err()
        {
            return Vec::new();
        }
        let mut slots = Vec::new();
        for (tick, entity, key, package) in &self.wakeups {
            if self.active_package.as_ref() != Some(package) || !self.allows_entity(*entity) {
                continue;
            }
            if key_prefix
                .as_ref()
                .is_some_and(|prefix| !key.starts_with(prefix))
            {
                continue;
            }
            if self.budget.output(48 + key.len() as u64).is_err() {
                return Vec::new();
            }
            slots.push(WakeupSlot {
                entity: WitEntityId::from_entity_id(*entity),
                tick: *tick,
                key: key.clone(),
            });
        }
        slots
    }
}

/// One component instance bound to an authority host.
pub struct ModInstance {
    package: ContentKey,
    engine: Engine,
    component: Component,
    bindings: ModBehavior,
    // Held as an option so a trapped store can be consumed for its host data
    // and replaced by a rebuilt instance.
    store: Option<Store<AuthorityHost>>,
    budget: CallBudget,
}

impl ModInstance {
    pub fn new(
        package: ContentKey,
        engine: &Engine,
        component: Component,
        mut host: AuthorityHost,
        budget: CallBudget,
    ) -> Result<Self, String> {
        host.bind_package(&package)?;
        let (bindings, store) = Self::build(engine, &component, host, budget)?;
        Ok(Self {
            package,
            engine: engine.clone(),
            component,
            bindings,
            store: Some(store),
            budget,
        })
    }

    fn build(
        engine: &Engine,
        component: &Component,
        host: AuthorityHost,
        budget: CallBudget,
    ) -> Result<(ModBehavior, Store<AuthorityHost>), String> {
        let mut linker: wasmtime::component::Linker<AuthorityHost> =
            wasmtime::component::Linker::new(engine);
        ModBehavior::add_to_linker::<AuthorityHost, wasmtime::component::HasSelf<AuthorityHost>>(
            &mut linker,
            |state| state,
        )
        .map_err(|error| format!("linker setup failed: {error}"))?;
        let mut store = AuthorityHost::bounded_store(engine, host, budget)?;
        let bindings = ModBehavior::instantiate(&mut store, component, &linker)
            .map_err(|error| format!("instantiation failed: {error}"))?;
        if store.data().budget_exceeded() {
            return Err("component initialization exceeded host budget".to_string());
        }
        Ok((bindings, store))
    }

    /// Discards the trapped instance and rebuilds it around the same host so
    /// guest state restarts from the last committed components.
    pub fn rebuild(&mut self) -> Result<(), String> {
        let old = self.store.take().expect("store present while rebuilding");
        let mut host = old.into_data();
        host.bind_package(&self.package)?;
        let (bindings, store) = Self::build(&self.engine, &self.component, host, self.budget)?;
        self.bindings = bindings;
        self.store = Some(store);
        Ok(())
    }

    fn store(&self) -> &Store<AuthorityHost> {
        self.store.as_ref().expect("store present")
    }

    fn store_mut(&mut self) -> &mut Store<AuthorityHost> {
        self.store.as_mut().expect("store present")
    }

    pub fn host(&self) -> &AuthorityHost {
        self.store().data()
    }

    pub fn host_mut(&mut self) -> &mut AuthorityHost {
        self.store_mut().data_mut()
    }

    /// Dispatches one event and applies the returned plan.
    pub fn dispatch(&mut self, tick: u64, event: Event) -> Result<DispatchOutcome, String> {
        if matches!(event, Event::Interaction(_)) {
            return Ok(DispatchOutcome::Rejected {
                reason: "interactions require a scoped durable route".into(),
            });
        }
        let package = self.package.clone();
        if self.host().is_deactivated(&package) {
            return Ok(DispatchOutcome::Deactivated);
        }
        let fuel = self.budget.fuel;
        {
            let store = self.store.as_mut().expect("store present");
            store
                .set_fuel(fuel)
                .map_err(|error| format!("fuel reset failed: {error}"))?;
            store.data_mut().begin_invocation();
            store.data_mut().bind_package(&package)?;
        }
        let Some(invocation) = self.host_mut().prepare_invocation(tick, &event, &package) else {
            self.host_mut().on_failure(tick, &event, &package);
            return Ok(DispatchOutcome::Rejected {
                reason: "host output budget exceeded".to_string(),
            });
        };
        let result = {
            let store = self.store.as_mut().expect("store present");
            self.bindings
                .torse_behavior_invoke()
                .call_handle_event(store, &invocation)
        };
        match result {
            Err(trap) => {
                let reason = trap
                    .chain()
                    .map(|cause| cause.to_string())
                    .collect::<Vec<_>>()
                    .join("; ");
                self.host_mut().on_failure(tick, &event, &package);
                self.rebuild()?;
                Ok(DispatchOutcome::Trapped { reason })
            }
            Ok(None) if self.host_mut().plan_fits(None) => Ok(DispatchOutcome::NoPlan),
            Ok(None) => {
                self.host_mut().on_failure(tick, &event, &package);
                self.rebuild()?;
                Ok(DispatchOutcome::Rejected {
                    reason: "host budget exceeded".to_string(),
                })
            }
            Ok(Some(plan)) => {
                let outcome = self.host_mut().apply_plan(&plan, &package)?;
                if matches!(outcome, DispatchOutcome::Rejected { .. }) {
                    self.host_mut().on_failure(tick, &event, &package);
                    if self.host().budget_exceeded() {
                        self.rebuild()?;
                    }
                }
                Ok(outcome)
            }
        }
    }

    /// Runs all wakeups due at each tick in `[from, from + count)`.
    pub fn run_ticks(&mut self, from: u64, count: u64) -> Result<Vec<DispatchOutcome>, String> {
        let mut outcomes = Vec::new();
        for tick in from..from + count {
            let due = self.host_mut().drain_due_wakeups(tick);
            for (entity, key, package) in due {
                if package != self.package {
                    return Err(format!(
                        "wakeup scheduled for package {package} but this instance serves {}",
                        self.package
                    ));
                }
                let event = Event::Wakeup(WakeupTarget {
                    entity: WitEntityId::from_entity_id(entity),
                    key,
                });
                outcomes.push(self.dispatch(tick, event)?);
            }
        }
        Ok(outcomes)
    }
}

/// Assembles the P2 machine scenario world with a real authority.
pub fn machine_scenario(config: &ScenarioConfig) -> Result<MachineWorld, String> {
    let package = key(config.package);
    let schema_key = key(config.schema);

    let machine_schema = ContentSchema {
        id: schema_key.clone(),
        schema_version: 1,
        owner_module: package.clone(),
        fields: vec![
            ComponentField {
                id: key("machine:field/history"),
                field_type: ComponentFieldType::List { max_items: 64 },
                required: false,
            },
            ComponentField {
                id: key("machine:field/progress"),
                field_type: ComponentFieldType::Unsigned32,
                required: true,
            },
            ComponentField {
                id: key("machine:field/input"),
                field_type: ComponentFieldType::EntityReference,
                required: true,
            },
            ComponentField {
                id: key("machine:field/output"),
                field_type: ComponentFieldType::EntityReference,
                required: true,
            },
        ],
        persistence: PersistencePolicy::Always,
        replication: torse_types::ReplicationDeclaration::SERVER_PRIVATE,
        migration_binding: None,
    };

    let mut pack: ContentPack = m0_fixture_pack();
    block_actions::register_content(&mut pack);
    register_inventory(&mut pack);
    register_action_module(
        &mut pack,
        package.clone(),
        key("machine:state/machine"),
        vec![machine_schema],
        Vec::new(),
        Vec::new(),
    );
    let content = compile(pack).map_err(|errors| format!("content compile failed: {errors:?}"))?;
    let registry = torse_world::BlockStateRegistry::build(&content)
        .map_err(|error| format!("registry build failed: {error:?}"))?;
    let schemas = ComponentSchemas::build(&content);
    let catalog = ItemCatalog::new([(key("base:stone"), 64)]).map_err(|e| format!("{e:?}"))?;

    let mut authority = WorldAuthority::new(
        WorldId(41),
        registry,
        schemas.clone(),
        16,
        WorldBounds::new(0, 32).map_err(|e| format!("{e:?}"))?,
        catalog.bind_compatibility(Compatibility {
            authority_schema: 3,
            rules_digest: content.authority_digest,
            content_digest: content.presentation_digest,
        }),
    )
    .map_err(|error| format!("authority assembly failed: {error:?}"))?;

    authority
        .store_mut()
        .request(SectionPos::ZERO)
        .map_err(|e| format!("{e:?}"))?;
    authority
        .store_mut()
        .install(
            SectionPos::ZERO,
            Section::empty(16).map_err(|e| format!("{e:?}"))?,
        )
        .map_err(|e| format!("{e:?}"))?;
    authority
        .store_mut()
        .finish_load(SectionPos::ZERO)
        .map_err(|e| format!("{e:?}"))?;

    let mut ids = [EntityId(0); 3];
    for (index, id) in ids.iter_mut().enumerate() {
        let position = Position::new(2.0 + index as f64, 2.0, 2.0);
        let handle = authority
            .entities_mut()
            .spawn(position)
            .map_err(|e| format!("{e:?}"))?;
        *id = authority.entities().get(handle).expect("entity").id;
    }
    let [machine, input, output] = ids;

    let mut actor = ActorContext::new(package.clone());
    for permission in config.granted_permissions {
        actor = actor.permission(key(permission));
    }
    for entity in ids {
        actor = actor.target(ActionTarget::Entity(entity));
    }

    let machine_fields = ComponentFields::from([
        (key("machine:field/progress"), ComponentValue::U32(0)),
        (key("machine:field/input"), ComponentValue::Entity(input)),
        (key("machine:field/output"), ComponentValue::Entity(output)),
    ]);
    let mut input_inventory = Inventory::new(2).map_err(|e| format!("{e:?}"))?;
    if config.stock_input {
        input_inventory
            .set(
                0,
                Some(ItemStack {
                    item: key("base:stone"),
                    count: 1,
                }),
                &catalog,
            )
            .map_err(|e| format!("{e:?}"))?;
    }
    let output_inventory = Inventory::new(2).map_err(|e| format!("{e:?}"))?;

    for (entity, schema, fields) in [
        (machine, schema_key.clone(), machine_fields),
        (input, torse_inventory::schema(), input_inventory.fields()),
        (output, torse_inventory::schema(), output_inventory.fields()),
    ] {
        let handle = authority
            .entities()
            .handle_of(entity)
            .expect("entity exists");
        authority
            .entities_mut()
            .components_mut(handle)
            .ok_or("components missing")?
            .insert(&schemas, &schema, fields)
            .map_err(|e| format!("component install failed: {e:?}"))?;
    }

    let mut symbols = BTreeMap::new();
    let mut symbol_keys = Vec::new();
    let intern = |value: &str, symbols: &mut BTreeMap<String, u32>, keys: &mut Vec<String>| {
        let symbol = keys.len() as u32 + 1;
        symbols.insert(value.to_string(), symbol);
        keys.push(value.to_string());
    };
    for block in content.blocks.keys() {
        intern(&block.to_string(), &mut symbols, &mut symbol_keys);
    }
    for state in content.state_schemas.values() {
        for schema in &state.component_schemas {
            intern(&schema.id.to_string(), &mut symbols, &mut symbol_keys);
        }
    }
    for item in ["base:stone", config.package, config.schema] {
        if !symbols.contains_key(item) {
            intern(item, &mut symbols, &mut symbol_keys);
        }
    }

    let mut host = AuthorityHost::new(authority, catalog.clone(), symbols, symbol_keys);
    host.register_package(package, actor, vec![schema_key.clone()]);
    Ok(MachineWorld {
        host,
        catalog,
        machine,
        input,
        output,
    })
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use torse::behavior::query::Host;

    fn host() -> AuthorityHost {
        let mut host = machine_scenario(&ScenarioConfig {
            package: "machine:module",
            schema: "machine:schema/state",
            stock_input: true,
            granted_permissions: &["inventory:permission/transfer", "inventory:permission/read"],
        })
        .unwrap()
        .host;
        host.bind_package(&key("machine:module")).unwrap();
        host
    }

    fn region(min: [i64; 3], max: [i64; 3]) -> Bbox {
        Bbox {
            min: Vec3i {
                x: min[0],
                y: min[1],
                z: min[2],
            },
            max: Vec3i {
                x: max[0],
                y: max[1],
                z: max[2],
            },
        }
    }

    #[test]
    fn imports_filter_targets_schemas_and_package_wakeups() {
        use torse::behavior::schedule::Host as _;

        let mut host = host();
        let all = || region([i64::MIN; 3], [i64::MAX; 3]);
        let machine = host.authority.entities().iter().next().unwrap().1.id;
        let schema = host.symbol_of(&key("machine:schema/state")).unwrap();
        let inventory = host.symbol_of(&torse_inventory::schema()).unwrap();
        let input = host.authority.entities().iter().nth(1).unwrap().1.id;
        let stranger = host
            .authority_mut()
            .entities_mut()
            .spawn(Position::new(2.0, 2.0, 2.0))
            .unwrap();
        let stranger = host.authority.entities().get(stranger).unwrap().id;
        let package = key("machine:module");
        let other = key("other:module");
        host.seed_wakeup(1, machine, "own", package.clone());
        host.seed_wakeup(2, machine, "foreign", other.clone());
        host.seed_wakeup(3, stranger, "denied", package.clone());
        let page = host.query_entities(all(), None, 16).unwrap();
        assert_eq!(page.items.len(), 3);
        assert!(!page.items.iter().any(|e| e.id.to_entity_id() == stranger));
        let first = host.query_entities(all(), None, 2).unwrap();
        let last = host.query_entities(all(), first.next, 2).unwrap();
        assert_eq!(last.items.len(), 1);
        assert!(last.next.is_none());
        let target = |entity, schema| ComponentTarget {
            entity: WitEntityId::from_entity_id(entity),
            schema,
        };
        assert!(
            host.read_components(vec![target(machine, schema)]).unwrap()[0]
                .fields
                .is_some()
        );
        assert!(
            host.read_components(vec![target(input, inventory)])
                .unwrap()[0]
                .fields
                .is_some()
        );
        assert!(
            host.probe_item_port(WitEntityId::from_entity_id(input), "main".into())
                .is_some()
        );
        assert_eq!(host.pending_wakeups(None).len(), 1);
        assert_eq!(host.pending_wakeups(None)[0].key, "own");
        assert!(matches!(
            host.query_blocks(region([0; 3], [1; 3])).unwrap()[0].cell,
            Cell::NotLoaded
        ));
        assert!(host.read_positions.is_empty());

        host.register_package(
            other.clone(),
            ActorContext::new(other.clone())
                .target(ActionTarget::Entity(machine))
                .target(ActionTarget::Entity(input)),
            Vec::new(),
        );
        host.bind_package(&other).unwrap();
        host.begin_invocation();
        assert!(
            host.read_components(vec![target(machine, schema), target(input, inventory)])
                .unwrap()
                .iter()
                .all(|s| s.fields.is_none())
        );
        assert!(
            host.probe_item_port(WitEntityId::from_entity_id(input), "main".into())
                .is_none()
        );
        assert_eq!(host.pending_wakeups(None)[0].key, "foreign");
        assert!(
            host.build_object_ref(&WitEntityId::from_entity_id(stranger), &other)
                .is_none()
        );
        let denied = Event::Wakeup(WakeupTarget {
            entity: WitEntityId::from_entity_id(stranger),
            key: "denied".into(),
        });
        assert!(host.prepare_invocation(0, &denied, &other).is_none());

        assert!(host.bind_package(&key("unknown:module")).is_err());
        host.begin_invocation();
        assert!(
            host.query_entities(all(), None, 16)
                .unwrap()
                .items
                .is_empty()
        );
        assert!(host.pending_wakeups(None).is_empty());
        assert!(
            host.read_components(vec![target(machine, schema)]).unwrap()[0]
                .fields
                .is_none()
        );

        // A later trusted binding may grant terrain without granting entities.
        host.register_package(
            other.clone(),
            ActorContext::new(other.clone()).section(SectionPos::ZERO),
            Vec::new(),
        );
        host.bind_package(&other).unwrap();
        assert!(matches!(
            host.query_blocks(region([0; 3], [1; 3])).unwrap()[0].cell,
            Cell::Empty
        ));
        assert_eq!(host.read_positions.len(), 1);
        assert!(
            host.query_entities(all(), None, 16)
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[test]
    fn authority_rejects_overflow_without_reads_or_commits() {
        let mut host = host();
        let seq = host.seq();
        for (min, max) in [
            ([0; 3], [1_i64 << 32, 1_i64 << 32, 1]),
            ([i64::MIN, 0, 0], [i64::MAX, 1, 1]),
        ] {
            host.begin_invocation();
            assert!(matches!(
                host.query_blocks(region(min, max)),
                Err(QueryError::BudgetExceeded)
            ));
            assert!(host.read_positions.is_empty());
        }
        host.begin_invocation();
        assert!(
            host.query_blocks(region([0; 3], [0, i64::MAX, i64::MAX]))
                .unwrap()
                .is_empty()
        );
        assert_eq!(host.seq(), seq);
    }

    #[test]
    fn actual_authority_budget_is_cumulative_and_fail_closed() {
        let mut host = host();
        host.configure_budget(CallBudget {
            max_cells_per_query: 1,
            max_host_work: 1,
            ..CallBudget::prototype()
        });
        assert_eq!(host.query_blocks(region([0; 3], [1; 3])).unwrap().len(), 1);
        assert!(matches!(
            host.query_blocks(region([0; 3], [1; 3])),
            Err(QueryError::BudgetExceeded)
        ));
        let plan = Plan {
            spawns: Vec::new(),
            despawns: Vec::new(),
            edits: Vec::new(),
            component_edits: Vec::new(),
            transfers: Vec::new(),
            wakeups: Vec::new(),
        };
        let seq = host.seq();
        assert!(matches!(
            host.apply_plan(&plan, &key("machine:module")).unwrap(),
            DispatchOutcome::Rejected { .. }
        ));
        assert_eq!(host.seq(), seq);
        host.begin_invocation();
        assert_eq!(host.query_blocks(region([0; 3], [1; 3])).unwrap().len(), 1);
        host.begin_invocation();
        assert!(matches!(
            host.query_blocks(region([0; 3], [2, 1, 1])),
            Err(QueryError::BudgetExceeded)
        ));
    }

    #[test]
    fn oversized_inputs_results_and_cursor_are_safe() {
        let mut host = host();
        host.configure_budget(CallBudget {
            max_input_bytes: 2,
            ..CallBudget::prototype()
        });
        assert!(host.resolve_symbol("machine:module".to_string()).is_none());
        assert!(host.budget_exceeded());
        host.configure_budget(CallBudget {
            max_output_bytes: 1,
            ..CallBudget::prototype()
        });
        assert!(matches!(
            host.query_blocks(region([0; 3], [1; 3])),
            Err(QueryError::BudgetExceeded)
        ));
        host.configure_budget(CallBudget::prototype());
        let page = host
            .query_entities(
                region([i64::MIN; 3], [i64::MAX; 3]),
                Some(usize::MAX.to_string()),
                1,
            )
            .unwrap();
        assert!(page.items.is_empty());
        assert!(page.next.is_none());
        host.configure_budget(CallBudget {
            max_output_bytes: 1,
            ..CallBudget::prototype()
        });
        let target = ComponentTarget {
            entity: WitEntityId {
                low: u64::MAX,
                high: u64::MAX,
            },
            schema: host.symbol_of(&key("machine:schema/state")).unwrap(),
        };
        assert!(matches!(
            host.read_components(vec![target]),
            Err(QueryError::BudgetExceeded)
        ));
    }

    #[test]
    fn vacant_entity_slots_are_charged_before_scanning() {
        let mut host = host();
        let mut handles = Vec::new();
        for _ in 0..32 {
            handles.push(
                host.authority_mut()
                    .entities_mut()
                    .spawn(Position::new(0.0, 64.0, 0.0))
                    .unwrap(),
            );
        }
        for handle in handles {
            host.authority_mut().entities_mut().despawn(handle).unwrap();
        }
        assert_eq!(host.authority().entities().len(), 3);
        host.configure_budget(CallBudget {
            max_host_work: 10,
            ..CallBudget::prototype()
        });
        assert!(matches!(
            host.query_entities(region([i64::MIN; 3], [i64::MAX; 3]), None, 1),
            Err(QueryError::BudgetExceeded)
        ));
    }
}
