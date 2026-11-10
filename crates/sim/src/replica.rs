//! A-side authorized replication. The source exclusively owns its authority;
//! opening a seed and publishing increments therefore share one serial boundary.
//! B must gate seeds/batches by their cursor against DurableSeq, and discard its
//! own outgoing queue when a publication invalidates a subscription.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use torse_diagnostics::{SerialProfiler, Stage};

use torse_types::{
    AuthorityMode, Binding, CommitCursor, ContentKey, EntityId, Fault, FaultCode, OutputBudget,
    ReplicaSeq, ReplicationVisibility, SectionPos, SubscriptionId, Viewer,
};
use torse_world::{
    ComponentFields, ComponentValue, Entity, SectionPhase, SectionStorage, SnapshotCells,
    SnapshotSection, StateEntry, WorldBounds,
};

use crate::action::{ActionTarget, ActorContext};
use crate::{CommitFault, PrepareError, PreparedChange, Publication, Transaction, WorldAuthority};

mod incremental;
mod interest;
use incremental::{Dirty, ProjectionCache};

/// Host-approved interest. Terrain and entity presence are public within this
/// range; Authorized components additionally require the actor's entity grant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplicaScope {
    pub sections: BTreeSet<SectionPos>,
}

/// No allocator watermark, private component, previous value, or runtime handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplicaEntity {
    pub id: EntityId,
    pub position_bits: [u64; 3],
    pub components: BTreeMap<ContentKey, ComponentFields>,
}

/// Canonical order: sections by coordinate, entities by persistent ID, state
/// table by symbol. State symbols are meaningful only through the included
/// stable content table; they are never interpreted as a remote runtime ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplicaSnapshot {
    pub side: u32,
    pub bounds: WorldBounds,
    pub states: Vec<StateEntry>,
    pub sections: Vec<SnapshotSection>,
    pub entities: Vec<ReplicaEntity>,
}

/// Atomic final replacements/removals for one visible transaction. The state
/// table describes the resulting projection, not the storage registry as a whole.
/// Replace entities wholesale so removed components/fields cannot linger.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplicaChanges {
    pub presentation: Vec<torse_world::PresentationEvent>,
    pub states: Vec<StateEntry>,
    pub sections: Vec<SnapshotSection>,
    pub removed_sections: Vec<SectionPos>,
    pub entities: Vec<ReplicaEntity>,
    pub removed_entities: Vec<EntityId>,
}

pub type WorldReplicaSeed = torse_types::ReplicaSeed<ReplicaSnapshot>;
pub type WorldReplicaBatch = torse_types::ReplicaBatch<ReplicaChanges>;

/// Per-source and per-subscription hard limits; there are no unbounded defaults.
#[derive(Clone, Copy, Debug)]
pub struct ReplicaLimits {
    pub max_subscriptions: usize,
    pub max_sections: usize,
    pub max_queued_batches: usize,
    /// Includes the seed envelope, nested values and dense cell arrays.
    pub snapshot: OutputBudget,
    /// Sum of batch envelopes and nested payloads for one subscription.
    pub queue: OutputBudget,
}

struct Subscription {
    viewer: Viewer,
    actor: ActorContext,
    scope: ReplicaScope,
    state: Option<ReplicaSnapshot>,
    cache: ProjectionCache,
    next_seq: u64,
    queue: VecDeque<(WorldReplicaBatch, Cost)>,
    queued: Cost,
    _memory: Option<torse_types::MemoryLease>,
}

/// Concrete source, following the A5b convention of deferring a generic trait
/// until a second implementation needs it. It is not a transport or journal.
pub struct ReplicaSource {
    memory: Option<torse_types::MemoryPool>,
    world: WorldAuthority,
    binding: Binding,
    limits: ReplicaLimits,
    next_subscription: u128,
    subscriptions: BTreeMap<u128, Subscription>,
    pending_invalidations: BTreeSet<u128>,
    profiler: SerialProfiler,
    incremental: bool,
}

impl ReplicaSource {
    /// An incremental interest cut requires a live projection with no unconsumed
    /// source batches. Hosts must reset or close if downstream output is unsafe.
    pub fn can_change_interest(&self, viewer: Viewer) -> bool {
        matches!(self.interest_ready(viewer), Ok(true))
    }

    /// Distinguish drainable source backlog from a projection requiring reset.
    pub fn interest_ready(&self, viewer: Viewer) -> Result<bool, Fault> {
        let mut ready = true;
        for sub in self
            .subscriptions
            .values()
            .filter(|sub| sub.viewer == viewer)
        {
            if sub.state.is_none() {
                return Err(reset_required());
            }
            ready &= sub.queue.is_empty();
        }
        Ok(ready)
    }

    /// Preserve subscription IDs and sequence normal edits and interest cuts on
    /// the same ReplicaSeq stream. A failed multi-subscription update invalidates
    /// every projection for this viewer; hosts must clear downstream output.
    pub fn change_interest(
        &mut self,
        viewer: Viewer,
        actor: ActorContext,
        scope: ReplicaScope,
        budget: OutputBudget,
    ) -> Result<Vec<WorldReplicaBatch>, Fault> {
        if scope.sections.len() > self.limits.max_sections {
            return Err(Fault::resource_limit("replica interest capacity"));
        }
        if !self.can_change_interest(viewer) {
            return Err(reset_required());
        }
        let through = CommitCursor {
            world: self.world.world(),
            seq: self.world.seq(),
        };
        let result: Result<Vec<WorldReplicaBatch>, Fault> = self
            .subscriptions
            .iter_mut()
            .filter(|(_, sub)| sub.viewer == viewer)
            .map(|(id, sub)| {
                interest::change(
                    &self.world,
                    self.binding,
                    self.limits,
                    SubscriptionId(*id),
                    through,
                    sub,
                    actor.clone(),
                    scope.clone(),
                    budget,
                )
            })
            .collect();
        if result.is_err() {
            self.invalidate_viewer(viewer);
        }
        result
    }
    /// Aggregate existing bounded subscription ledgers, without encoding or cloning payloads.
    pub fn queue_diagnostics(
        &self,
        config: &torse_types::QueueDiagnostics,
    ) -> torse_types::Diagnostic<torse_types::ReplicaQueues> {
        use torse_types::{Diagnostic, ReplicaQueues, Unavailable};
        if let Err(reason) = config.check::<ReplicaQueues>() {
            return Diagnostic::Unavailable(reason);
        }
        let total = self.subscriptions.values().try_fold(
            ReplicaQueues {
                subscriptions: self.subscriptions.len() as u64,
                ..ReplicaQueues::default()
            },
            |total, sub| {
                Some(ReplicaQueues {
                    subscriptions: total.subscriptions,
                    batches: total.batches.checked_add(sub.queue.len() as u64)?,
                    items: total.items.checked_add(sub.queued.items)?,
                    logical_bytes: total.logical_bytes.checked_add(sub.queued.bytes)?,
                })
            },
        );
        match total {
            Some(total) => Diagnostic::Available(total),
            None => Diagnostic::Unavailable(Unavailable::Overflow),
        }
    }
    /// Connect before opening subscriptions; live payloads keep their own reservations.
    pub fn attach_memory(&mut self, memory: torse_types::MemoryPool) -> Result<(), Fault> {
        if self.memory.is_some() || !self.subscriptions.is_empty() {
            return Err(Fault::invalid_data(
                "replica memory account requires a fresh source",
            ));
        }
        self.memory = Some(memory);
        Ok(())
    }
    /// Residency invalidates affected subscriptions before any further output.
    pub fn publish_section(
        &mut self,
        prepared: crate::streaming::PreparedSection,
    ) -> Result<Publication, CommitFault> {
        let pos = prepared.position();
        let mut publication = self.world.publish_section(prepared)?;
        for (&id, sub) in &mut self.subscriptions {
            if sub.scope.sections.contains(&pos) && sub.state.is_some() {
                reset(sub);
                self.pending_invalidations.insert(id);
            }
        }
        publication.invalidated_replicas = std::mem::take(&mut self.pending_invalidations)
            .into_iter()
            .map(SubscriptionId)
            .collect();
        Ok(publication)
    }

    pub fn new(
        world: WorldAuthority,
        binding: Binding,
        limits: ReplicaLimits,
    ) -> Result<Self, Fault> {
        if binding.world != world.world()
            || binding.compatibility != world.coordinator().compatibility()
            || binding.epoch.0 == 0
            || limits.max_subscriptions == 0
            || limits.max_sections == 0
            || limits.max_queued_batches == 0
            || limits.snapshot.max_items == 0
            || limits.snapshot.max_bytes == 0
            || limits.queue.max_items == 0
            || limits.queue.max_bytes == 0
        {
            return Err(Fault::invalid_data("invalid replica binding or limits"));
        }
        Ok(Self {
            memory: None,
            world,
            binding,
            limits,
            next_subscription: 1,
            subscriptions: BTreeMap::new(),
            pending_invalidations: BTreeSet::new(),
            profiler: SerialProfiler::default(),
            incremental: true,
        })
    }

    /// Read-only; no mutable store/coordinator escape can bypass publication.
    pub fn world(&self) -> &WorldAuthority {
        &self.world
    }

    pub fn set_serial_profiler(&mut self, profiler: SerialProfiler) {
        self.profiler = profiler;
    }

    /// Retain full projection as a diagnostic reference, including for live subscriptions.
    pub fn set_projection_optimization(&mut self, enabled: bool) {
        self.incremental = enabled;
    }

    pub fn prepare(&self, transaction: &Transaction) -> Result<PreparedChange, PrepareError> {
        self.world.prepare(transaction)
    }

    pub fn prepare_phase_advance(&self) -> Result<PreparedChange, CommitFault> {
        self.world.prepare_phase_advance()
    }

    pub fn prepare_barrier(&self) -> Result<PreparedChange, CommitFault> {
        self.world.prepare_barrier()
    }

    /// The host supplies an authenticated actor and approved interest, separately
    /// from the Viewer IDs. Call under the same host authentication used by actions.
    /// Failed opening consumes neither a slot nor an ID.
    pub fn open_replica(
        &mut self,
        viewer: Viewer,
        actor: ActorContext,
        scope: ReplicaScope,
        budget: OutputBudget,
    ) -> Result<WorldReplicaSeed, Fault> {
        if viewer.session.0 == 0 || viewer.actor.0 == 0 {
            return Err(Fault::invalid_data("reserved viewer identity"));
        }
        if self.subscriptions.len() >= self.limits.max_subscriptions
            || scope.sections.len() > self.limits.max_sections
            || self.next_subscription == u128::MAX
        {
            return Err(Fault::resource_limit(
                "replica subscription capacity exhausted",
            ));
        }
        let bytes = self
            .limits
            .snapshot
            .max_bytes
            .checked_mul(3)
            .and_then(|bytes| bytes.checked_add(self.limits.queue.max_bytes.checked_mul(2)?))
            .ok_or_else(|| Fault::resource_limit("replica memory reservation overflow"))?;
        let memory = self
            .memory
            .as_ref()
            .map(|pool| {
                pool.reserve(
                    torse_types::MemoryClass::Replica,
                    bytes,
                    torse_types::EvictionRank::Pinned,
                )
            })
            .transpose()?;
        // Count envelope and state before copying even one owned world value.
        let state = project(
            &self.world,
            &actor,
            &scope,
            smaller(budget, self.limits.snapshot),
        )?;
        let cache = ProjectionCache::new(&state)?;
        let subscription = SubscriptionId(self.next_subscription);
        self.next_subscription += 1;
        let seed = WorldReplicaSeed {
            binding: self.binding,
            subscription,
            baseline: CommitCursor {
                world: self.world.world(),
                seq: self.world.seq(),
            },
            state: state.clone(),
        };
        self.subscriptions.insert(
            subscription.0,
            Subscription {
                viewer,
                actor,
                scope,
                state: Some(state),
                cache,
                next_seq: 1,
                queue: VecDeque::new(),
                queued: Cost::default(),
                _memory: memory,
            },
        );
        Ok(seed)
    }

    /// Publish after B reserves the full authoritative record. Projection failure
    /// or slow consumers reset only their subscription, never reject a world commit.
    pub fn publish(&mut self, prepared: PreparedChange) -> Result<Publication, CommitFault> {
        let presentation = prepared.presentation().to_vec();
        let visible_work = !presentation.is_empty()
            || !prepared.edits().is_empty()
            || !prepared.entity_moves().is_empty()
            || !prepared.component_edits().is_empty()
            || prepared.entity_lifecycle().is_some();
        let dirty = (visible_work
            && self.incremental
            && self.subscriptions.values().any(|s| s.state.is_some()))
        .then(|| Dirty::capture(&self.world, &prepared))
        .flatten();
        let mut publication = self.world.publish(prepared)?;
        // Durable task/phase barriers change no observable world data. They must
        // still deliver pending invalidations, but need no projection or clone.
        for (&id, sub) in self.subscriptions.iter_mut().filter(|_| visible_work) {
            if sub.state.is_none() {
                continue;
            }
            let _span = self.profiler.span(Stage::Projection);
            if self.binding.compatibility != self.world.coordinator().compatibility() {
                reset(sub);
                self.pending_invalidations.insert(id);
                continue;
            }
            let result = if let Some(dirty) = &dirty {
                incremental::update(
                    &self.world,
                    self.binding,
                    self.limits,
                    SubscriptionId(id),
                    publication.cursor,
                    sub,
                    dirty,
                )
            } else {
                full_projection(
                    &self.world,
                    self.binding,
                    self.limits,
                    SubscriptionId(id),
                    publication.cursor,
                    sub,
                )
            };
            let result = result.and_then(|()| {
                append_presentation(
                    &self.world,
                    self.binding,
                    self.limits,
                    SubscriptionId(id),
                    publication.cursor,
                    sub,
                    &presentation,
                )
            });
            if result.is_err() {
                reset(sub);
                self.pending_invalidations.insert(id);
            }
        }
        publication.invalidated_replicas = std::mem::take(&mut self.pending_invalidations)
            .into_iter()
            .map(SubscriptionId)
            .collect();
        Ok(publication)
    }

    /// Returns a prefix of complete batches. A too-small first-batch budget
    /// returns ResourceLimit without draining anything; the caller may retry.
    pub fn take_replica_batches(
        &mut self,
        binding: Binding,
        subscription: SubscriptionId,
        budget: OutputBudget,
    ) -> Result<Vec<WorldReplicaBatch>, Fault> {
        self.check_binding(binding)?;
        let sub = self
            .subscriptions
            .get_mut(&subscription.0)
            .ok_or_else(reset_required)?;
        if sub.state.is_none() {
            return Err(reset_required());
        }
        let mut total = Cost {
            items: 0,
            bytes: size_of::<Vec<WorldReplicaBatch>>() as u64,
        };
        if !total.fits(budget) {
            return Err(Fault::resource_limit("replica output budget"));
        }
        let mut count = 0;
        for (_, cost) in &sub.queue {
            let next = total.add(*cost)?;
            if !next.fits(budget) {
                if count == 0 {
                    return Err(Fault::resource_limit(
                        "whole replica batch exceeds output budget",
                    ));
                }
                break;
            }
            total = next;
            count += 1;
        }
        let mut output = Vec::with_capacity(count);
        for _ in 0..count {
            let (batch, cost) = sub.queue.pop_front().expect("counted queue entry");
            sub.queued.items -= cost.items;
            sub.queued.bytes -= cost.bytes;
            output.push(batch);
        }
        Ok(output)
    }

    /// Immediately clears A's data. The returned independent control signal must
    /// clear B's already-drained data before any more sends. Also repeats on the
    /// next successful publication, never behind ordinary batches.
    pub fn invalidate_viewer(&mut self, viewer: Viewer) -> Vec<SubscriptionId> {
        let mut invalidated = Vec::new();
        for (&id, sub) in &mut self.subscriptions {
            if sub.viewer == viewer && sub.state.is_some() {
                reset(sub);
                self.pending_invalidations.insert(id);
                invalidated.push(SubscriptionId(id));
            }
        }
        invalidated
    }

    pub fn close_replica(
        &mut self,
        binding: Binding,
        subscription: SubscriptionId,
    ) -> Result<(), Fault> {
        self.check_binding(binding)?;
        self.subscriptions.remove(&subscription.0);
        self.pending_invalidations.remove(&subscription.0);
        Ok(())
    }

    fn check_binding(&self, binding: Binding) -> Result<(), Fault> {
        if binding != self.binding {
            Err(reset_required())
        } else {
            Ok(())
        }
    }
}

fn reset(sub: &mut Subscription) {
    sub.state = None;
    sub.cache = ProjectionCache::default();
    sub.queue.clear();
    sub.queued = Cost::default();
    sub._memory = None;
}

fn reset_required() -> Fault {
    Fault {
        code: FaultCode::ResetRequired,
        context: "replica binding or subscription is no longer valid".into(),
    }
}

fn smaller(a: OutputBudget, b: OutputBudget) -> OutputBudget {
    OutputBudget {
        max_items: a.max_items.min(b.max_items),
        max_bytes: a.max_bytes.min(b.max_bytes),
    }
}

/// Conservative owned-payload accounting, independent of B's eventual encoding.
/// Includes collection elements, nested values and strings; excludes allocator
/// bookkeeping. Every selected component is charged BEFORE it is cloned.
#[derive(Clone, Copy, Default)]
struct Cost {
    items: u64,
    bytes: u64,
}

impl Cost {
    fn subtract(self, other: Self) -> Result<Self, Fault> {
        Ok(Self {
            items: self
                .items
                .checked_sub(other.items)
                .ok_or_else(|| Fault::invalid_data("replica cost underflow"))?,
            bytes: self
                .bytes
                .checked_sub(other.bytes)
                .ok_or_else(|| Fault::invalid_data("replica cost underflow"))?,
        })
    }
    fn add(self, other: Self) -> Result<Self, Fault> {
        Ok(Self {
            items: self
                .items
                .checked_add(other.items)
                .ok_or_else(|| Fault::resource_limit("replica items overflow"))?,
            bytes: self
                .bytes
                .checked_add(other.bytes)
                .ok_or_else(|| Fault::resource_limit("replica bytes overflow"))?,
        })
    }
    fn fits(self, budget: OutputBudget) -> bool {
        self.items <= u64::from(budget.max_items) && self.bytes <= budget.max_bytes
    }
    fn entries<T>(&mut self, n: usize) -> Result<(), Fault> {
        *self = self.add(Self {
            items: n as u64,
            bytes: (n as u64)
                .checked_mul(size_of::<T>() as u64)
                .ok_or_else(|| Fault::resource_limit("replica bytes overflow"))?,
        })?;
        Ok(())
    }
    fn key(&mut self, key: &ContentKey) -> Result<(), Fault> {
        self.bytes(key.namespace().len() + key.path().len())
    }
    fn bytes(&mut self, n: usize) -> Result<(), Fault> {
        *self = self.add(Self {
            items: 0,
            bytes: n as u64,
        })?;
        Ok(())
    }
    fn value(&mut self, value: &ComponentValue) -> Result<(), Fault> {
        match value {
            ComponentValue::Content(key) => self.key(key)?,
            ComponentValue::Text(text) => self.bytes(text.len())?,
            ComponentValue::List(values) => {
                self.entries::<ComponentValue>(values.len())?;
                for value in values {
                    self.value(value)?;
                }
            }
            ComponentValue::Record(fields) => self.fields(fields)?,
            _ => {}
        }
        Ok(())
    }
    fn fields(&mut self, fields: &ComponentFields) -> Result<(), Fault> {
        self.entries::<(ContentKey, ComponentValue)>(fields.len())?;
        for (key, value) in fields {
            self.key(key)?;
            self.value(value)?;
        }
        Ok(())
    }
    fn entity(&mut self, entity: &ReplicaEntity) -> Result<(), Fault> {
        self.entries::<ReplicaEntity>(1)?;
        for (key, fields) in &entity.components {
            self.entries::<(ContentKey, ComponentFields)>(1)?;
            self.key(key)?;
            self.fields(fields)?;
        }
        Ok(())
    }
    fn states(&mut self, states: &[StateEntry]) -> Result<(), Fault> {
        self.entries::<StateEntry>(states.len())?;
        for state in states {
            self.key(&state.block)?;
            self.bytes(state.values.len() * size_of::<u16>())?;
        }
        Ok(())
    }
    fn sections(&mut self, sections: &[SnapshotSection]) -> Result<(), Fault> {
        self.entries::<SnapshotSection>(sections.len())?;
        for section in sections {
            if let SnapshotCells::Dense(cells) = &section.cells {
                self.bytes(cells.len() * size_of::<u16>())?;
            }
        }
        Ok(())
    }
}

fn visible(
    world: &WorldAuthority,
    actor: &ActorContext,
    entity: &Entity,
    schema: &ContentKey,
) -> bool {
    world.schemas().get(schema).is_some_and(|schema| {
        schema.replication.authority == AuthorityMode::Server
            && match schema.replication.visibility {
                ReplicationVisibility::Private => false,
                ReplicationVisibility::Public => true,
                ReplicationVisibility::Authorized => actor.allows(ActionTarget::Entity(entity.id)),
            }
    })
}

fn in_scope(world: &WorldAuthority, scope: &ReplicaScope, entity: &Entity) -> bool {
    entity
        .position
        .section(world.store().side())
        .is_some_and(|pos| scope.sections.contains(&pos))
}

fn readable(world: &WorldAuthority, pos: SectionPos) -> bool {
    matches!(
        world.store().phase(pos),
        Some(SectionPhase::Ready | SectionPhase::Active)
    )
}

fn project(
    world: &WorldAuthority,
    actor: &ActorContext,
    scope: &ReplicaScope,
    budget: OutputBudget,
) -> Result<ReplicaSnapshot, Fault> {
    project_sections(world, actor, scope, &scope.sections, budget)
}

/// The entity range remains complete; only selected terrain is captured.
fn project_sections(
    world: &WorldAuthority,
    actor: &ActorContext,
    scope: &ReplicaScope,
    terrain: &BTreeSet<SectionPos>,
    budget: OutputBudget,
) -> Result<ReplicaSnapshot, Fault> {
    let mut cost = Cost::default();
    cost.entries::<WorldReplicaSeed>(1)?;
    // Scratch follows the actual registry; enumerate only observed symbols,
    // avoiding two full identifier-space scans for every subscriber/commit.
    let mut seen = vec![false; world.registry().state_count() as usize];
    let mut used = Vec::new();
    let mut mark = |state: torse_world::BlockStateId| -> Result<(), Fault> {
        let slot = seen
            .get_mut(state.0 as usize)
            .ok_or_else(|| Fault::invalid_data("unregistered replica state"))?;
        if !*slot && !state.is_empty() {
            used.push(state);
        }
        *slot = true;
        Ok(())
    };
    // Borrow-only preflight. Inaccessible sections stay absent, never become air.
    for &pos in terrain {
        if !readable(world, pos) {
            continue;
        }
        let section = world.store().section(pos).expect("readable section");
        cost.entries::<SnapshotSection>(1)?;
        match section.storage() {
            SectionStorage::Uniform(state) => mark(*state)?,
            SectionStorage::Dense(cells) => {
                cost.bytes(cells.len() * size_of::<u16>())?;
                for state in cells {
                    mark(*state)?;
                }
            }
            SectionStorage::Palette(cells) => {
                cost.bytes(cells.len() * size_of::<u16>())?;
                for state in cells.states() {
                    mark(*state)?;
                }
            }
        }
        if !cost.fits(budget) {
            return Err(Fault::resource_limit("replica snapshot budget"));
        }
    }
    for (_, entity) in world
        .entities()
        .iter()
        .filter(|(_, e)| in_scope(world, scope, e))
    {
        cost.entries::<ReplicaEntity>(1)?;
        for key in entity
            .components
            .ids()
            .filter(|key| visible(world, actor, entity, key))
        {
            cost.entries::<(ContentKey, ComponentFields)>(1)?;
            cost.key(key)?;
            cost.fields(entity.components.component(key).expect("component exists"))?;
        }
        if !cost.fits(budget) {
            return Err(Fault::resource_limit("replica snapshot budget"));
        }
    }
    used.sort_unstable();
    for &id in &used {
        cost.entries::<StateEntry>(1)?;
        cost.key(
            world
                .registry()
                .definition_key(id)
                .ok_or_else(|| Fault::invalid_data("unregistered replica state"))?,
        )?;
        cost.bytes(std::mem::size_of_val(
            world.registry().state_values(id).unwrap_or(&[]),
        ))?;
    }
    if !cost.fits(budget) {
        return Err(Fault::resource_limit("replica snapshot budget"));
    }
    let states = used
        .iter()
        .map(|&id| StateEntry {
            id,
            block: world
                .registry()
                .definition_key(id)
                .expect("preflight resolved")
                .clone(),
            values: world.registry().state_values(id).unwrap_or(&[]).to_vec(),
        })
        .collect();
    let sections = terrain
        .iter()
        .copied()
        .filter(|pos| readable(world, *pos))
        .map(|pos| {
            let cells =
                SnapshotCells::capture(world.store().section(pos).expect("readable section"));
            SnapshotSection { pos, cells }
        })
        .collect();
    let mut entities: Vec<_> = world
        .entities()
        .iter()
        .filter(|(_, e)| in_scope(world, scope, e))
        .map(|(_, entity)| ReplicaEntity {
            id: entity.id,
            position_bits: entity.position.to_bits(),
            components: entity
                .components
                .ids()
                .filter(|key| visible(world, actor, entity, key))
                .map(|key| {
                    (
                        key.clone(),
                        entity
                            .components
                            .component(key)
                            .expect("component exists")
                            .clone(),
                    )
                })
                .collect(),
        })
        .collect();
    entities.sort_by_key(|entity| entity.id);
    Ok(ReplicaSnapshot {
        side: world.store().side(),
        bounds: world.store().bounds(),
        states,
        sections,
        entities,
    })
}

fn difference(previous: &ReplicaSnapshot, next: &ReplicaSnapshot) -> ReplicaChanges {
    ReplicaChanges {
        presentation: Vec::new(),
        states: next.states.clone(),
        sections: next
            .sections
            .iter()
            .filter(|section| {
                previous
                    .sections
                    .binary_search_by_key(&section.pos, |s| s.pos)
                    .map_or(true, |i| previous.sections[i] != **section)
            })
            .cloned()
            .collect(),
        removed_sections: previous
            .sections
            .iter()
            .filter(|section| {
                next.sections
                    .binary_search_by_key(&section.pos, |s| s.pos)
                    .is_err()
            })
            .map(|s| s.pos)
            .collect(),
        entities: next
            .entities
            .iter()
            .filter(|entity| {
                previous
                    .entities
                    .binary_search_by_key(&entity.id, |e| e.id)
                    .map_or(true, |i| previous.entities[i] != **entity)
            })
            .cloned()
            .collect(),
        removed_entities: previous
            .entities
            .iter()
            .filter(|entity| {
                next.entities
                    .binary_search_by_key(&entity.id, |e| e.id)
                    .is_err()
            })
            .map(|e| e.id)
            .collect(),
    }
}

fn batch_cost(batch: &WorldReplicaBatch) -> Result<Cost, Fault> {
    let mut cost = Cost::default();
    cost.entries::<WorldReplicaBatch>(1)?;
    cost.states(&batch.changes.states)?;
    cost.entries::<torse_world::PresentationEvent>(batch.changes.presentation.len())?;
    for event in &batch.changes.presentation {
        cost.key(&event.draft.kind)?;
        cost.fields(&event.draft.fields)?;
    }
    cost.sections(&batch.changes.sections)?;
    cost.entries::<SectionPos>(batch.changes.removed_sections.len())?;
    cost.entries::<EntityId>(batch.changes.removed_entities.len())?;
    for entity in &batch.changes.entities {
        cost.entity(entity)?;
    }
    Ok(cost)
}

/// Extend the current commit's batch, or create an event-only batch. Authorization
/// and the full queue charge are checked before copying any selected payload.
fn append_presentation(
    world: &WorldAuthority,
    binding: Binding,
    limits: ReplicaLimits,
    id: SubscriptionId,
    through: CommitCursor,
    sub: &mut Subscription,
    drafts: &[torse_world::PresentationDraft],
) -> Result<(), Fault> {
    use torse_world::{PresentationEvent, PresentationId, PresentationVisibility};
    let selected: Vec<_> = drafts
        .iter()
        .enumerate()
        .filter(|(_, draft)| {
            sub.scope.sections.contains(&draft.section)
                && readable(world, draft.section)
                && world
                    .schemas()
                    .presentation(&draft.kind)
                    .is_some_and(|schema| match &schema.visibility {
                        PresentationVisibility::Public => draft.recipient.is_none(),
                        PresentationVisibility::Authorized { permission } => {
                            sub.actor.permits(permission)
                                && draft.recipient.is_some_and(|entity| {
                                    sub.actor.allows(ActionTarget::Entity(entity))
                                })
                        }
                    })
        })
        .collect();
    if selected.is_empty() {
        return Ok(());
    }
    let same_commit = sub
        .queue
        .back()
        .is_some_and(|(batch, _)| batch.through == through);
    let mut extra = Cost::default();
    extra.entries::<PresentationEvent>(selected.len())?;
    for (_, draft) in &selected {
        extra.key(&draft.kind)?;
        extra.fields(&draft.fields)?;
    }
    if !same_commit {
        extra.entries::<WorldReplicaBatch>(1)?;
        extra.states(&sub.state.as_ref().expect("active subscription").states)?;
        if sub.queue.len() >= limits.max_queued_batches || sub.next_seq == u64::MAX {
            return Err(Fault::resource_limit(
                "presentation replica queue exhausted",
            ));
        }
    }
    let total = sub.queued.add(extra)?;
    if !total.fits(limits.queue) {
        return Err(Fault::resource_limit("presentation replica budget"));
    }
    let events = selected
        .into_iter()
        .map(|(index, draft)| PresentationEvent {
            id: PresentationId {
                epoch: binding.epoch,
                cursor: through,
                index: index as u32,
            },
            draft: draft.clone(),
        })
        .collect();
    if same_commit {
        let (batch, cost) = sub.queue.back_mut().expect("same commit batch");
        batch.changes.presentation = events;
        *cost = cost.add(extra)?;
    } else {
        sub.queue.push_back((
            WorldReplicaBatch {
                binding,
                subscription: id,
                seq: ReplicaSeq(sub.next_seq),
                through,
                changes: ReplicaChanges {
                    presentation: events,
                    states: sub
                        .state
                        .as_ref()
                        .expect("active subscription")
                        .states
                        .clone(),
                    sections: Vec::new(),
                    removed_sections: Vec::new(),
                    entities: Vec::new(),
                    removed_entities: Vec::new(),
                },
            },
            extra,
        ));
        sub.next_seq += 1;
    }
    sub.queued = total;
    Ok(())
}

fn full_projection(
    world: &WorldAuthority,
    binding: Binding,
    limits: ReplicaLimits,
    id: SubscriptionId,
    through: CommitCursor,
    sub: &mut Subscription,
) -> Result<(), Fault> {
    let next = project(world, &sub.actor, &sub.scope, limits.snapshot)?;
    let previous = sub.state.as_ref().expect("active subscription");
    if &next == previous {
        return Ok(());
    }
    let batch = WorldReplicaBatch {
        binding,
        subscription: id,
        seq: ReplicaSeq(sub.next_seq),
        through,
        changes: difference(previous, &next),
    };
    let cost = batch_cost(&batch)?;
    let total = sub.queued.add(cost)?;
    if sub.queue.len() >= limits.max_queued_batches
        || !total.fits(limits.queue)
        || sub.next_seq == u64::MAX
    {
        return Err(Fault::resource_limit("replica queue exhausted"));
    }
    // Rebuild only if diagnostics switches this subscription back to incremental.
    sub.cache = ProjectionCache::default();
    sub.queue.push_back((batch, cost));
    sub.queued = total;
    sub.next_seq += 1;
    sub.state = Some(next);
    Ok(())
}
