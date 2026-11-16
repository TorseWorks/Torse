//! Durable phase driver. Each bounded batch is frozen before its first task
//! commits; every task, request result, event change and execution position then
//! shares one journal record with its world mutation. Recovery never runs rules.
use crate::{HostAuthority, PreparedChange, RecoveryTarget};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;
use torse_codec::{CodecLimits, wire};
use torse_diagnostics::{SerialProfiler, Stage};
use torse_sim::action::{ActionOrigin, ActionOutput, ActionRequest, ActionService, ActorContext};
use torse_sim::durable::*;
use torse_sim::replica::{
    ReplicaLimits, ReplicaScope, ReplicaSource, WorldReplicaBatch, WorldReplicaSeed,
};
use torse_sim::{
    Phase, PrepareError, Recovery, SystemSpec, Transaction, WorldAuthority, next_resume_point,
    order_phase, persistent_changes_from_prepared,
};
use torse_types::*;
use torse_world::{BlockStateRegistry, ComponentFields, ComponentSchemas};

#[path = "planning.rs"]
mod planning;
pub use planning::{NativePlanningStats, PlanningConfig, PlanningContext};
#[path = "runtime_budget.rs"]
mod runtime_budget;
use runtime_budget::RuntimeBudget;
#[path = "prevalidation.rs"]
mod prevalidation;
pub use prevalidation::{PrevalidationConfig, PrevalidationStats};
#[path = "installs.rs"]
pub mod installs;
#[path = "execution_memory.rs"]
mod memory;
#[path = "player_control.rs"]
mod player_control;
#[path = "streaming.rs"]
mod streaming;
pub use player_control::{PlayerState, movement_action, movement_call};
pub use streaming::SectionInstallDiagnostics;

pub struct ExecutionModel;
/// Derived event tickets. They are rebuilt from committed events and current entity locations.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EventTickets {
    pub global: bool,
    pub sections: std::collections::BTreeSet<SectionPos>,
}
impl AuthorityData for ExecutionModel {
    type Snapshot = ExecutionSnapshot;
    type Changes = ExecutionChanges;
    type Input = Call;
    type Output = Reply;
    type ReplicaSnapshot = torse_sim::replica::ReplicaSnapshot;
    type ReplicaChanges = torse_sim::replica::ReplicaChanges;
}

#[derive(Clone, Copy, Debug)]
pub struct ExecutionLimits {
    pub pending_requests: usize,
    pub pending_bytes: u64,
    pub input: OutputBudget,
    pub sessions: usize,
    pub receipt_window: usize,
    /// Retained tasks per ordinary/event batch and event callbacks per admission attempt.
    pub phase_tasks: usize,
    pub events: usize,
    pub runtime: OutputBudget,
    pub replicas: ReplicaLimits,
    pub codec: CodecLimits,
}
impl ExecutionLimits {
    pub(crate) fn validate(self) -> Result<(), Fault> {
        if [
            self.pending_requests,
            self.sessions,
            self.receipt_window,
            self.phase_tasks,
            self.events,
        ]
        .contains(&0)
            || self.phase_tasks >= (1 << 31)
            || self.runtime.max_items == 0
            || self.runtime.max_bytes == 0
            || self.input.max_bytes == 0
            || self.input.max_items == 0
            || self.pending_bytes < self.input.max_bytes
        {
            return Err(Fault::invalid_data("execution capacities must be nonzero"));
        }
        Ok(())
    }
}

/// A rule is a pure planner. Persistent machine state belongs in world components;
/// callbacks have no I/O or mutable authority. Ordinary planners share the phase-start
/// world; each later event batch shares its committed batch-start world.
/// Freeze/retry may call a planner again before it has published any state.
pub trait DurableRule {
    fn spec(&self) -> SystemSpec;
    /// Opt in only when `plan` is thread-safe, pure, and bounded by the host's
    /// phase/runtime limits. Return `Some(self)` on a Sync implementation.
    /// Actions and timed events continue through their serial callbacks.
    fn parallel_planner(&self) -> Option<&(dyn DurableRule + Sync)> {
        None
    }
    /// Split one otherwise isolated opt-in rule into bounded native jobs.
    /// Called on the host thread only when no other rule overlaps this batch.
    /// Default implementations retain their ordinary planner; timed callbacks
    /// remain serial. All work must finish before returning the plan.
    fn plan_parallel(
        &self,
        world: &WorldAuthority,
        tick: Tick,
        _context: &PlanningContext,
    ) -> Result<Vec<WorkPlan>, Fault> {
        self.plan(world, tick)
    }
    /// Derive bounded notifications from a planned action. They are frozen with
    /// that action and registered only if its world transaction commits.
    /// This callback must not invoke guests or change process-local state.
    fn action_events(
        &self,
        _world: &WorldAuthority,
        _tick: Tick,
        _transaction: &Transaction,
    ) -> Result<Vec<EventDraft>, Fault> {
        Ok(Vec::new())
    }
    fn plan(&self, _world: &WorldAuthority, _tick: Tick) -> Result<Vec<WorkPlan>, Fault> {
        Ok(Vec::new())
    }
    fn event(
        &self,
        _world: &WorldAuthority,
        _tick: Tick,
        _event: &TimedEvent,
    ) -> Result<WorkPlan, Fault> {
        Err(Fault::invalid_data("rule has no event handler"))
    }
}
pub struct WorkPlan {
    pub transaction: Option<torse_sim::TransactionGroup>,
    pub schedule: Vec<EventDraft>,
    pub cancel: Vec<EventId>,
}
impl WorkPlan {
    pub fn transaction(transaction: Transaction) -> Self {
        Self {
            transaction: Some(torse_sim::TransactionGroup {
                id: transaction.id,
                ordering: transaction.ordering,
                members: vec![transaction],
            }),
            schedule: Vec::new(),
            cancel: Vec::new(),
        }
    }
}

struct LiveSession {
    viewer: Viewer,
    actor: ActorContext,
    scope: ReplicaScope,
    high: u64,
    completed: u64,
}
struct Queued {
    request: WorldRequest<Call>,
    tick: Tick,
}
pub struct ExecutionHost {
    players: BTreeMap<u128, player_control::PlayerSession>,
    parked: BTreeMap<SectionPos, ParkedSection>,
    visual_transitions:
        std::collections::VecDeque<(SectionPos, crate::visual::Stamp, crate::visual::Stamp)>,
    section_pending: Option<ExecutionPrepared>,
    section_install_limit: usize,
    section_install_diagnostics: streaming::SectionInstallDiagnostics,
    source: ReplicaSource,
    runtime: RuntimeState,
    rules: Vec<Box<dyn DurableRule>>,
    actions: ActionService,
    sessions: BTreeMap<u128, LiveSession>,
    pending: Vec<Queued>,
    frozen: Option<PhaseWork>,
    binding: Binding,
    limits: ExecutionLimits,
    stopping: Option<Tick>,
    highest_session: u128,
    profiler: SerialProfiler,
    planning: Option<PlanningConfig>,
    native_planning_stats: std::cell::Cell<NativePlanningStats>,
    compact_records: bool,
    budget_cache: RefCell<RuntimeBudget>,
    prevalidation: Option<PrevalidationConfig>,
    prevalidated: RefCell<prevalidation::PrevalidationCache>,
    memory: Option<memory::Resident>,
}
pub struct ExecutionPrepared {
    world: PreparedWorld,
    record: CommitRecord<ExecutionChanges>,
    remove_pending: Vec<RequestId>,
    runtime: RuntimeState,
    memory: Option<memory::Prepared>,
}
struct PreparedWorld {
    normal: Option<torse_sim::PreparedChange>,
    section: Option<Box<torse_sim::streaming::PreparedSection>>,
    sections: Option<Box<torse_sim::streaming::PreparedSections>>,
}
impl PreparedChange for ExecutionPrepared {
    type Model = ExecutionModel;
    fn record(&self) -> &CommitRecord<ExecutionChanges> {
        &self.record
    }
}

fn specifications(rules: &[Box<dyn DurableRule>]) -> Result<Vec<SystemSpec>, Fault> {
    let mut specs: Vec<_> = rules.iter().map(|r| r.spec()).collect();
    specs.sort_by(|a, b| a.id.cmp(&b.id));
    if specs.windows(2).any(|w| w[0].id == w[1].id) {
        return Err(Fault::invalid_data("duplicate durable rule"));
    }
    for phase in Phase::ALL {
        let phase_specs: Vec<_> = specs.iter().filter(|s| s.phase == phase).cloned().collect();
        order_phase(&phase_specs).map_err(|e| Fault::invalid_data(e.to_string()))?;
    }
    Ok(specs)
}
fn checked_state(
    state: &RuntimeState,
    limits: ExecutionLimits,
    point: ResumePoint,
    world: WorldId,
) -> Result<(), Fault> {
    checked_state_cached(state, limits, point, world, None)
}
fn checked_state_cached(
    state: &RuntimeState,
    limits: ExecutionLimits,
    point: ResumePoint,
    world: WorldId,
    cache: Option<&RefCell<RuntimeBudget>>,
) -> Result<(), Fault> {
    if point.next_step != 0
        || Phase::from_index(point.next_phase).is_none()
        || state.next_event == 0
        || state.events.len() > limits.events
        || state.windows.len() > limits.sessions
        || state
            .work
            .as_ref()
            .is_some_and(|w| w.tasks.len() > limits.phase_tasks || w.next as usize > w.tasks.len())
    {
        return Err(Fault::resource_limit(
            "invalid or over-budget execution state",
        ));
    }
    let mut event_ids = std::collections::BTreeSet::new();
    for event in &state.events {
        event.event.dependencies()?;
        if event.id.0 == 0
            || event.id.0 >= state.next_event
            || !event_ids.insert(event.id.0)
            || !state
                .rules
                .iter()
                .any(|r| r.id == event.event.rule && r.phase == Phase::Simulation)
        {
            return Err(Fault::invalid_data("invalid persistent event"));
        }
    }
    if state
        .events
        .windows(2)
        .any(|w| (w[0].event.tick, w[0].id) >= (w[1].event.tick, w[1].id))
    {
        return Err(Fault::invalid_data("noncanonical persistent events"));
    }
    let mut windows = std::collections::BTreeSet::new();
    for window in &state.windows {
        if window.session.0 == 0
            || window.epoch.0 == 0
            || !windows.insert((window.epoch.0, window.session.0))
            || window.receipts.len() > limits.receipt_window
            || window
                .receipts
                .windows(2)
                .any(|r| r[0].id.seq.0 >= r[1].id.seq.0)
        {
            return Err(Fault::invalid_data("invalid request deduplication window"));
        }
        for r in &window.receipts {
            if r.id.world != world
                || r.id.epoch != window.epoch
                || r.id.session != window.session
                || r.id.seq.0 == 0
                || r.id.seq.0 > window.high
                || r.reply.at.world != world
            {
                return Err(Fault::invalid_data("invalid persistent receipt"));
            }
        }
    }
    if let Some(work) = &state.work {
        if work.event_batch
            && (Phase::from_index(point.next_phase) != Some(Phase::Simulation)
                || work.tasks.is_empty()
                || work
                    .tasks
                    .iter()
                    .any(|task| task.consume.is_none() || task.request.is_some()))
        {
            return Err(Fault::invalid_data("invalid event batch"));
        }
        let mut requests = std::collections::BTreeSet::new();
        let mut consumes = std::collections::BTreeSet::new();
        for (index, task) in work.tasks.iter().enumerate() {
            for event in &task.schedule {
                event.dependencies()?;
            }
            if task
                .transaction
                .as_ref()
                .is_some_and(|t| t.validate().is_err() || t.members[0].phase != point.next_phase)
                || task.schedule.len() > limits.events
                || task.cancel.len() > limits.events
                || task.request.as_ref().is_some_and(|r| {
                    r.id.world != world
                        || r.id.seq.0 == 0
                        || r.id.epoch.0 == 0
                        || r.id.session.0 == 0
                        || !requests.insert((r.id.epoch.0, r.id.session.0, r.id.seq.0))
                })
            {
                return Err(Fault::invalid_data("invalid frozen phase task"));
            }
            if index >= work.next as usize {
                if let Some(id) = task.consume
                    && (!consumes.insert(id.0)
                        || id.0 == 0
                        || id.0 >= state.next_event
                        || state
                            .events
                            .iter()
                            .any(|e| e.id == id && e.event.tick > point.tick))
                {
                    return Err(Fault::invalid_data("invalid event acknowledgement"));
                }
                if task.request.as_ref().is_some_and(|r| {
                    state
                        .windows
                        .iter()
                        .flat_map(|w| &w.receipts)
                        .any(|old| old.id == r.id)
                }) {
                    return Err(Fault::invalid_data(
                        "request already completed before pending task",
                    ));
                }
            }
        }
    }
    if let Some(cache) = cache {
        return cache.borrow_mut().check(state, limits);
    }
    let mut encoded = Vec::new();
    wire::encode(state, &mut encoded, limits.runtime, limits.codec)?;
    // Decode enforces owned allocation/node budgets, not merely encoded length.
    let _: RuntimeState = wire::decode(&encoded, limits.runtime, limits.codec)?;
    Ok(())
}
/// Replay validates admission without invoking a callback. Event batches must
/// freeze a nonempty canonical prefix, never skip or invent queued identities.
fn valid_batch_freeze(before: &RuntimeState, work: &PhaseWork, point: ResumePoint) -> bool {
    if work.next != 0 {
        return false;
    }
    if work.event_batch {
        Phase::from_index(point.next_phase) == Some(Phase::Simulation)
            && !work.tasks.is_empty()
            && work.tasks.len()
                <= before
                    .events
                    .iter()
                    .take_while(|event| event.event.tick <= point.tick)
                    .count()
            && work
                .tasks
                .iter()
                .zip(&before.events)
                .all(|(task, event)| task.consume == Some(event.id) && task.request.is_none())
    } else {
        // Legacy ordinary batches may also contain already frozen events.
        work.tasks.iter().filter_map(|task| task.consume).all(|id| {
            before
                .events
                .iter()
                .any(|event| event.id == id && event.event.tick <= point.tick)
        })
    }
}

fn receipt_bounds(state: &RuntimeState, cursor: CommitCursor) -> Result<(), Fault> {
    if state
        .windows
        .iter()
        .flat_map(|w| &w.receipts)
        .any(|r| r.reply.at.world != cursor.world || r.reply.at.seq > cursor.seq)
    {
        return Err(Fault::invalid_data("receipt exceeds authoritative cursor"));
    }
    Ok(())
}

impl ExecutionHost {
    pub(crate) fn proves_visual_transition(
        &self,
        pos: SectionPos,
        before: crate::visual::Stamp,
        after: crate::visual::Stamp,
    ) -> bool {
        // A visual poll may see several residency changes for the same page.
        // An intervening change to its content breaks the stamp chain.
        let mut expected = after;
        for &(page, from, to) in self.visual_transitions.iter().rev() {
            if page != pos {
                continue;
            }
            if to != expected {
                return false;
            }
            if from == before {
                return true;
            }
            expected = from;
        }
        false
    }

    /// Export a tick-boundary archive with source versions and complete runtime.
    pub fn capture_archive(
        &mut self,
        content: &torse_content::CompiledContent,
        budget: OutputBudget,
    ) -> Result<ExecutionArchive, Fault> {
        let snapshot = HostAuthority::capture_snapshot(self, budget)?;
        crate::archive::capture(snapshot.state, content, budget, self.limits.codec)
    }

    pub fn new(
        binding: Binding,
        world: WorldAuthority,
        actions: ActionService,
        rules: Vec<Box<dyn DurableRule>>,
        limits: ExecutionLimits,
    ) -> Result<Self, Fault> {
        let runtime = RuntimeState {
            rules: specifications(&rules)?.into(),
            next_event: 1,
            events: Vec::new(),
            windows: Vec::new(),
            work: None,
        };
        Self::from_state(binding, world, actions, rules, limits, runtime)
    }
    fn from_state(
        binding: Binding,
        world: WorldAuthority,
        actions: ActionService,
        rules: Vec<Box<dyn DurableRule>>,
        limits: ExecutionLimits,
        runtime: RuntimeState,
    ) -> Result<Self, Fault> {
        limits.validate()?;
        if specifications(&rules)? != *runtime.rules {
            return Err(Fault::incompatible("durable rule declarations changed"));
        }
        checked_state(
            &runtime,
            limits,
            world.coordinator().resume_point(),
            binding.world,
        )?;
        receipt_bounds(
            &runtime,
            CommitCursor {
                world: binding.world,
                seq: world.seq(),
            },
        )?;
        Ok(Self {
            memory: None,
            players: BTreeMap::new(),
            source: ReplicaSource::new(world, binding, limits.replicas)?,
            parked: BTreeMap::new(),
            visual_transitions: std::collections::VecDeque::new(),
            section_pending: None,
            section_install_limit: 24,
            section_install_diagnostics: streaming::SectionInstallDiagnostics::default(),
            runtime,
            rules,
            actions,
            sessions: BTreeMap::new(),
            pending: Vec::new(),
            frozen: None,
            binding,
            limits,
            stopping: None,
            highest_session: 0,
            profiler: SerialProfiler::default(),
            planning: None,
            native_planning_stats: Default::default(),
            compact_records: true,
            budget_cache: RefCell::default(),
            prevalidation: None,
            prevalidated: RefCell::default(),
        })
    }
    pub fn world(&self) -> &WorldAuthority {
        self.source.world()
    }
    pub(crate) fn replica_section_limit(&self) -> usize {
        self.limits.replicas.max_sections
    }
    pub fn runtime(&self) -> &RuntimeState {
        &self.runtime
    }
    /// Disabling this uses full records and full wire validation for comparison.
    /// Immutable table sharing remains enabled. Readers also accept old logs.
    pub fn set_runtime_optimization(&mut self, enabled: bool) {
        self.compact_records = enabled;
    }

    pub fn set_projection_optimization(&mut self, enabled: bool) {
        self.source.set_projection_optimization(enabled);
    }
    fn validation_cache(&self) -> Option<&RefCell<RuntimeBudget>> {
        self.compact_records.then_some(&self.budget_cache)
    }

    pub(crate) fn set_serial_profiler(&mut self, profiler: SerialProfiler) {
        self.source.set_serial_profiler(profiler.clone());
        self.profiler = profiler;
    }
    pub fn register_session(
        &mut self,
        viewer: Viewer,
        actor: ActorContext,
        scope: ReplicaScope,
    ) -> Result<(), Fault> {
        if self.stopping.is_some()
            || viewer.session.0 == 0
            || viewer.actor.0 == 0
            || self.sessions.len() >= self.limits.sessions
            || viewer.session.0 <= self.highest_session
        {
            return Err(Fault::resource_limit("session unavailable"));
        }
        if scope.sections.len() > self.limits.replicas.max_sections {
            return Err(Fault::resource_limit("session scope too large"));
        }
        if actor.section_count() > torse_sim::action::MAX_ACTOR_SECTIONS {
            return Err(Fault::resource_limit("actor section grants too large"));
        }
        self.highest_session = viewer.session.0;
        self.sessions.insert(
            viewer.session.0,
            LiveSession {
                viewer,
                actor,
                scope,
                high: 0,
                completed: 0,
            },
        );
        Ok(())
    }
    /// Replace trusted grants without resetting the session request sequence.
    /// Invalidates every old projection immediately. The caller must consume
    /// the returned control signal before sending any further replica data.
    pub fn replace_session_scope(
        &mut self,
        session: SessionId,
        actor: ActorContext,
        scope: ReplicaScope,
    ) -> Result<Vec<SubscriptionId>, Fault> {
        if scope.sections.len() > self.limits.replicas.max_sections
            || actor.section_count() > torse_sim::action::MAX_ACTOR_SECTIONS
        {
            return Err(Fault::resource_limit("session scope too large"));
        }
        let live = self
            .sessions
            .get_mut(&session.0)
            .ok_or_else(|| Fault::invalid_data("unknown session"))?;
        let invalidated = self.source.invalidate_viewer(live.viewer);
        live.actor = actor;
        live.scope = scope;
        Ok(invalidated)
    }

    pub fn can_change_interest(&self, session: SessionId) -> bool {
        self.sessions
            .get(&session.0)
            .is_some_and(|live| self.source.can_change_interest(live.viewer))
    }

    pub fn interest_ready(&self, session: SessionId) -> Result<bool, Fault> {
        let live = self
            .sessions
            .get(&session.0)
            .ok_or_else(|| Fault::invalid_data("unknown session"))?;
        self.source.interest_ready(live.viewer)
    }

    pub fn change_session_interest(
        &mut self,
        session: SessionId,
        actor: ActorContext,
        scope: ReplicaScope,
        budget: OutputBudget,
    ) -> Result<Vec<WorldReplicaBatch>, Fault> {
        if scope.sections.len() > self.limits.replicas.max_sections
            || actor.section_count() > torse_sim::action::MAX_ACTOR_SECTIONS
        {
            return Err(Fault::resource_limit("session scope too large"));
        }
        let live = self
            .sessions
            .get(&session.0)
            .ok_or_else(|| Fault::invalid_data("unknown session"))?;
        let batches =
            self.source
                .change_interest(live.viewer, actor.clone(), scope.clone(), budget)?;
        let live = self.sessions.get_mut(&session.0).expect("checked session");
        live.actor = actor;
        live.scope = scope;
        Ok(batches)
    }

    pub fn revoke_session(&mut self, session: SessionId) -> Vec<SubscriptionId> {
        let children: Vec<_> = self
            .players
            .iter()
            .filter(|(id, player)| **id == session.0 || player.parent == session)
            .map(|(id, _)| *id)
            .collect();
        for id in children {
            self.players.remove(&id);
            if id != session.0 {
                self.sessions.remove(&id);
            }
        }
        self.sessions
            .remove(&session.0)
            .map_or_else(Vec::new, |s| self.source.invalidate_viewer(s.viewer))
    }
    pub fn open_replica(
        &mut self,
        session: SessionId,
        scope: ReplicaScope,
        budget: OutputBudget,
    ) -> Result<WorldReplicaSeed, Fault> {
        let live = self
            .sessions
            .get(&session.0)
            .ok_or_else(|| Fault::invalid_data("unknown session"))?;
        if !scope.sections.is_subset(&live.scope.sections) {
            return Err(Fault::invalid_data("unauthorized replica scope"));
        }
        self.source
            .open_replica(live.viewer, live.actor.clone(), scope, budget)
    }
    pub fn take_replica(
        &mut self,
        id: SubscriptionId,
        budget: OutputBudget,
    ) -> Result<Vec<WorldReplicaBatch>, Fault> {
        self.source.take_replica_batches(self.binding, id, budget)
    }
    pub fn close_replica(&mut self, id: SubscriptionId) -> Result<(), Fault> {
        self.source.close_replica(self.binding, id)
    }
    pub fn receipt(&self, id: RequestId) -> Option<&Receipt> {
        self.runtime
            .windows
            .iter()
            .flat_map(|w| &w.receipts)
            .find(|r| r.id == id)
    }
    pub fn expired(&self, id: RequestId) -> bool {
        self.receipt(id).is_none()
            && self.pending_input(id).is_none()
            && self
                .sessions
                .get(&id.session.0)
                .is_some_and(|s| s.completed >= id.seq.0)
    }
    fn pending_input(&self, id: RequestId) -> Option<&Call> {
        self.pending
            .iter()
            .find(|q| q.request.id == id)
            .map(|q| &q.request.input)
            .or_else(|| {
                self.runtime
                    .work
                    .iter()
                    .flat_map(|w| w.tasks.iter().skip(w.next as usize))
                    .filter_map(|t| t.request.as_ref())
                    .find(|r| r.id == id)
                    .map(|r| &r.input)
            })
    }
    fn freeze_plan(
        &self,
        plan: WorkPlan,
        phase: Phase,
        consume: Option<EventId>,
    ) -> Result<FrozenTask, Fault> {
        planning::freeze_rule_plan(self.world(), plan, phase, consume)
    }
    fn collect_phase(&self) -> Result<PhaseWork, Fault> {
        let _span = self.profiler.span(Stage::Planning);
        let point = self.resume_point();
        let phase = Phase::from_index(point.next_phase).expect("validated phase");
        let mut ordinary = Vec::new();
        self.collect_rules(phase, point.tick, &mut ordinary)?;
        let mut tasks = Vec::new();
        if !self.pending.is_empty() {
            for q in self
                .pending
                .iter()
                .filter(|q| {
                    q.tick <= point.tick
                        && if q.request.input.action == movement_action() {
                            phase == Phase::Simulation
                        } else {
                            self.actions
                                .phase(&q.request.input.action)
                                .unwrap_or(Phase::Interaction)
                                == phase
                        }
                })
                .take(self.limits.phase_tasks.saturating_sub(ordinary.len()))
            {
                let id = q.request.id;
                let mut task = FrozenTask {
                    transaction: None,
                    request: Some(Accepted {
                        id,
                        input: q.request.input.clone(),
                    }),
                    consume: None,
                    schedule: Vec::new(),
                    cancel: Vec::new(),
                    status: ReplyStatus::Committed,
                    output: ComponentFields::new(),
                };
                let live = self
                    .sessions
                    .get(&id.session.0)
                    .filter(|s| s.viewer.actor == q.request.actor);
                let output = if self.stopping.is_some_and(|end| q.tick > end) {
                    Err(RejectCode::Stopping)
                } else {
                    live.ok_or(RejectCode::StaleSession).and_then(|s| {
                        if q.request.input.action == movement_action() {
                            return self
                                .plan_movement(&q.request)
                                .map(|tx| ActionOutput::Mutation(Box::new(tx)));
                        }
                        let actor = crate::item_drops::pickup_actor(
                            self.world(),
                            &s.actor,
                            &s.scope,
                            &q.request.input,
                        )?;
                        self.actions.plan(
                            self.world(),
                            &actor,
                            phase,
                            &ActionRequest {
                                action: q.request.input.action.clone(),
                                api_version: q.request.input.version,
                                origin: ActionOrigin::Network,
                                target: q.request.input.target,
                                parameters: q.request.input.parameters.clone(),
                                transaction: TransactionId(id.seq.0 as u128),
                                ordering: torse_sim::OrderingKey {
                                    phase,
                                    rule_priority: 0,
                                    stable_task_key: id.session.0 as u64,
                                    local_sequence: id.seq.0 as u32,
                                },
                            },
                        )
                    })
                };
                match output {
                    Ok(ActionOutput::Mutation(tx)) => {
                        for rule in &self.rules {
                            task.schedule.extend(rule.action_events(
                                self.world(),
                                point.tick,
                                &tx,
                            )?);
                            if task.schedule.len() > self.limits.events {
                                return Err(Fault::resource_limit("action event budget"));
                            }
                        }
                        task.transaction = Some(StoredTransactionGroup::single(self.world(), &tx)?)
                    }
                    Ok(ActionOutput::Query(fields)) => {
                        task.status = ReplyStatus::ReadOnly;
                        task.output = fields;
                    }
                    Ok(ActionOutput::Atomic(group)) => {
                        for member in &group.members {
                            for rule in &self.rules {
                                task.schedule.extend(rule.action_events(
                                    self.world(),
                                    point.tick,
                                    member,
                                )?);
                                if task.schedule.len() > self.limits.events {
                                    return Err(Fault::resource_limit("action event budget"));
                                }
                            }
                        }
                        task.transaction =
                            Some(StoredTransactionGroup::capture(self.world(), &group)?);
                    }
                    Ok(ActionOutput::Invoke(_)) => {
                        task.status = ReplyStatus::Rejected(RejectCode::InvalidData);
                    }
                    Err(reason) => task.status = ReplyStatus::Rejected(reason),
                }
                tasks.push(task);
            }
        }
        tasks.extend(ordinary);
        if tasks.is_empty() && self.has_due_events() {
            return self.collect_event_batch();
        }
        if tasks.len() > self.limits.phase_tasks {
            return Err(Fault::resource_limit("phase task budget"));
        }
        // Stable transaction arbitration, retaining deterministic registration/event order
        // for equal keys and tasks without mutations.
        tasks.sort_by(|a, b| {
            let key = |t: &FrozenTask| {
                t.transaction.as_ref().map(|p| {
                    let m = &p.members[0];
                    (m.priority, m.task_key, m.local, p.id.0)
                })
            };
            key(a).cmp(&key(b))
        });
        Ok(PhaseWork {
            tasks: tasks.into(),
            next: 0,
            event_batch: false,
        })
    }
    fn has_due_events(&self) -> bool {
        Phase::from_index(self.resume_point().next_phase) == Some(Phase::Simulation)
            && self
                .runtime
                .events
                .first()
                .is_some_and(|e| e.event.tick <= self.resume_point().tick)
    }

    /// Invoke at most one bounded prefix of due callbacks. Only frozen work is
    /// published here; unselected events retain their durable queue entries.
    fn collect_event_batch(&self) -> Result<PhaseWork, Fault> {
        let point = self.resume_point();
        let mut work = PhaseWork {
            tasks: Arc::default(),
            next: 0,
            event_batch: true,
        };
        let budget = RefCell::new(RuntimeBudget::default());
        for event in self
            .runtime
            .events
            .iter()
            .take_while(|e| e.event.tick <= point.tick)
            .take(self.limits.phase_tasks)
        {
            let rule = self
                .rules
                .iter()
                .find(|r| r.spec().id == event.event.rule)
                .ok_or_else(|| Fault::incompatible("missing event rule"))?;
            let task = self.freeze_plan(
                rule.event(self.world(), point.tick, event)?,
                Phase::Simulation,
                Some(event.id),
            )?;
            Arc::make_mut(&mut work.tasks).push(task);
            let mut candidate = self.runtime.clone();
            candidate.work = Some(work.clone());
            let admission: Result<(), Fault> = (|| {
                fit_state_cached(
                    &mut candidate,
                    self.limits,
                    point,
                    self.binding.world,
                    Some(&budget),
                )?;
                // Include the retained table and successful output at every
                // settlement, so aggregate growth defers work instead of
                // rejecting events which would fit in smaller batches.
                for task in work.tasks.iter() {
                    let status = if task
                        .consume
                        .is_some_and(|id| !candidate.events.iter().any(|event| event.id == id))
                    {
                        ReplyStatus::Rejected(RejectCode::Conflict)
                    } else {
                        task.status.clone()
                    };
                    candidate = settle(
                        &candidate,
                        task,
                        &status,
                        self.cursor(),
                        point,
                        self.limits,
                        Some(&budget),
                    )?;
                }
                Ok(())
            })();
            if let Err(error) = admission {
                Arc::make_mut(&mut work.tasks).pop();
                if work.tasks.is_empty() || error.code != FaultCode::ResourceLimit {
                    return Err(error);
                }
                break;
            }
        }
        Ok(work)
    }

    fn record(
        &self,
        prepared: torse_sim::PreparedChange,
        runtime: RuntimeState,
        remove_pending: Vec<RequestId>,
    ) -> Result<ExecutionPrepared, Fault> {
        self.record_outcome(prepared, runtime, remove_pending, None)
    }
    fn record_outcome(
        &self,
        prepared: torse_sim::PreparedChange,
        runtime: RuntimeState,
        remove_pending: Vec<RequestId>,
        outcome: Option<ReplyStatus>,
    ) -> Result<ExecutionPrepared, Fault> {
        checked_state_cached(
            &runtime,
            self.limits,
            prepared.meta().resume_at,
            self.binding.world,
            self.validation_cache(),
        )?;
        receipt_bounds(&runtime, prepared.meta().cursor)?;
        let changes = persistent_changes_from_prepared(
            self.world().registry(),
            self.world().schemas(),
            &prepared,
        )
        .map_err(|e| Fault::invalid_data(e.to_string()))?;
        let update = if self.compact_records && outcome.is_some() {
            RuntimeUpdate::Progress(RuntimeProgress {
                next: runtime.work.as_ref().expect("settled work").next,
                next_event: runtime.next_event,
                events: runtime.events.clone(),
                windows: runtime.windows.clone(),
            })
        } else {
            RuntimeUpdate::Full(runtime.clone())
        };
        Ok(ExecutionPrepared {
            memory: None,
            record: CommitRecord {
                meta: changes.meta,
                changes: ExecutionChanges {
                    world: changes,
                    runtime: update,
                    outcome,
                    cancelled: None,
                    section: None,
                    sections: Vec::new(),
                },
            },
            world: PreparedWorld {
                normal: Some(prepared),
                section: None,
                sections: None,
            },
            remove_pending,
            runtime,
        })
    }
    fn prepare_task(&self, task: &FrozenTask) -> Result<ExecutionPrepared, Fault> {
        let mut status = task.status.clone();
        let movement_rejection = if task
            .consume
            .is_some_and(|id| !self.runtime.events.iter().any(|e| e.id == id))
        {
            Some(RejectCode::Conflict)
        } else {
            self.check_frozen_movement(task).err()
        };
        let mut prepared = if let Some(reason) = movement_rejection {
            status = ReplyStatus::Rejected(reason);
            self.source.prepare_barrier().map_err(commit_fault)?
        } else {
            match &task.transaction {
                Some(stored) => match self.cached_prevalidation().map(Ok).unwrap_or_else(|| {
                    stored.restore(self.world()).and_then(|tx| {
                        self.source.world().prepare_group(&tx).map_err(|e| match e {
                            PrepareError::Rejected(c) => c,
                            PrepareError::SequenceExhausted => RejectCode::QuotaExceeded,
                        })
                    })
                }) {
                    Ok(prepared) => prepared,
                    Err(reason) => {
                        status = ReplyStatus::Rejected(reason);
                        self.source.prepare_barrier().map_err(commit_fault)?
                    }
                },
                None => self.source.prepare_barrier().map_err(commit_fault)?,
            }
        };
        let state = match settle(
            &self.runtime,
            task,
            &status,
            prepared.meta().cursor,
            self.resume_point(),
            self.limits,
            self.validation_cache(),
        ) {
            Ok(state) => state,
            Err(error) if !matches!(status, ReplyStatus::Rejected(_)) => {
                status = ReplyStatus::Rejected(if error.code == FaultCode::ResourceLimit {
                    RejectCode::QuotaExceeded
                } else {
                    RejectCode::InvalidData
                });
                prepared = self.source.prepare_barrier().map_err(commit_fault)?;
                settle(
                    &self.runtime,
                    task,
                    &status,
                    prepared.meta().cursor,
                    self.resume_point(),
                    self.limits,
                    self.validation_cache(),
                )?
            }
            Err(error) => return Err(error),
        };
        self.record_outcome(prepared, state, Vec::new(), Some(status))
    }
}
fn matches_plan(
    task: &FrozenTask,
    status: &ReplyStatus,
    world: &torse_world::ChangeSet,
    recovery: &Recovery<'_>,
) -> bool {
    let schemas = recovery.schemas();
    let Ok(projected) = world.persistent_view(schemas) else {
        return false;
    };
    let world = projected.as_ref();
    let empty = world.edits.is_empty()
        && world.entity_moves.is_empty()
        && world.component_edits.is_empty()
        && world.entity_lifecycle.is_none();
    if *status != ReplyStatus::Committed {
        return empty;
    }
    let Some(plan) = &task.transaction else {
        return empty;
    };
    let mut merged = plan.members[0].clone();
    for member in &plan.members[1..] {
        merged.writes.extend(member.writes.clone());
        merged.moves.extend(member.moves.clone());
        merged.components.extend(member.components.clone());
        merged.spawns.extend(member.spawns.clone());
        merged.despawns.extend(member.despawns.clone());
    }
    for component in &merged.components {
        if schemas.persists(&component.schema).is_err() {
            return false;
        }
    }
    merged
        .components
        .retain(|e| schemas.persists(&e.schema).expect("checked schema"));
    for spawn in &mut merged.spawns {
        if spawn
            .components
            .keys()
            .any(|key| schemas.persists(key).is_err())
        {
            return false;
        }
        spawn
            .components
            .retain(|key, _| schemas.persists(key).expect("checked schema"));
    }
    for despawn in &mut merged.despawns {
        if despawn.retain_persistent(schemas).is_err() {
            return false;
        }
    }
    let plan = &merged;
    if world.edits.len() != plan.writes.len()
        || !world
            .edits
            .iter()
            .zip(&plan.writes)
            .all(|(a, b)| a.pos == b.pos && a.content == b.content)
        || world.component_edits != plan.components
        || world.entity_moves.len() != plan.moves.len()
        || !world
            .entity_moves
            .iter()
            .zip(&plan.moves)
            .all(|(a, b)| a.id == b.id && a.previous_bits == b.previous)
    {
        return false;
    }
    let explicit_matches = match &world.entity_lifecycle {
        None => plan.spawns.is_empty() && plan.despawns.is_empty(),
        Some(life) => {
            life.despawned.get(..plan.despawns.len()) == Some(plan.despawns.as_slice())
                && life.spawned.len() >= plan.spawns.len()
                && life
                    .spawned
                    .iter()
                    .zip(&plan.spawns)
                    .all(|(a, b)| a.position_bits == b.position && a.components == b.components)
        }
    };
    explicit_matches
        && recovery
            .validate_derived_lifecycle(world, plan.spawns.len(), plan.despawns.len())
            .is_ok()
}
fn commit_fault(e: torse_sim::CommitFault) -> Fault {
    Fault::new(FaultCode::InvalidState, e.to_string())
}

fn prune_one(state: &mut RuntimeState) -> bool {
    let oldest = state
        .windows
        .iter()
        .enumerate()
        .flat_map(|(w, window)| {
            window
                .receipts
                .iter()
                .enumerate()
                .map(move |(r, receipt)| (receipt.reply.at.seq, w, r))
        })
        .min();
    if let Some((_, w, r)) = oldest {
        state.windows[w].receipts.remove(r);
        true
    } else {
        false
    }
}
fn fit_state(
    state: &mut RuntimeState,
    limits: ExecutionLimits,
    point: ResumePoint,
    world: WorldId,
) -> Result<(), Fault> {
    fit_state_cached(state, limits, point, world, None)
}
fn fit_state_cached(
    state: &mut RuntimeState,
    limits: ExecutionLimits,
    point: ResumePoint,
    world: WorldId,
    cache: Option<&RefCell<RuntimeBudget>>,
) -> Result<(), Fault> {
    loop {
        match checked_state_cached(state, limits, point, world, cache) {
            Ok(()) => return Ok(()),
            Err(e) if e.code == FaultCode::ResourceLimit && prune_one(state) => {}
            Err(e) => return Err(e),
        }
    }
}
fn settle(
    before: &RuntimeState,
    task: &FrozenTask,
    status: &ReplyStatus,
    at: CommitCursor,
    point: ResumePoint,
    limits: ExecutionLimits,
    cache: Option<&RefCell<RuntimeBudget>>,
) -> Result<RuntimeState, Fault> {
    if task
        .consume
        .is_some_and(|id| !before.events.iter().any(|e| e.id == id))
        && status != &ReplyStatus::Rejected(RejectCode::Conflict)
    {
        return Err(Fault::invalid_data(
            "cancelled frozen event must be skipped",
        ));
    }
    if status != &task.status
        && (matches!(task.status, ReplyStatus::Rejected(_))
            || !matches!(status, ReplyStatus::Rejected(_)))
    {
        return Err(Fault::invalid_data("illegal task outcome"));
    }
    let mut state = before.clone();
    if let Some(id) = task.consume {
        state.events.retain(|e| e.id != id);
    }
    if !matches!(status, ReplyStatus::Rejected(_)) {
        for id in &task.cancel {
            state.events.retain(|e| e.id != *id);
        }
        for draft in &task.schedule {
            if draft.tick <= point.tick || state.next_event == u128::MAX {
                return Err(Fault::invalid_data("invalid future event allocation"));
            }
            let id = EventId(state.next_event);
            state.next_event += 1;
            state.events.push(TimedEvent {
                id,
                event: draft.clone(),
            });
        }
        state.events.sort_by_key(|e| (e.event.tick, e.id));
    }
    if let Some(request) = &task.request {
        add_receipt(
            &mut state,
            request,
            Reply {
                status: status.clone(),
                fields: if matches!(status, ReplyStatus::Rejected(_)) {
                    ComponentFields::new()
                } else {
                    task.output.clone()
                },
                at,
            },
            limits,
        );
    }
    state
        .work
        .as_mut()
        .ok_or_else(|| Fault::invalid_data("task without work"))?
        .next += 1;
    fit_state_cached(&mut state, limits, point, at.world, cache)?;
    Ok(state)
}
fn add_receipt(
    state: &mut RuntimeState,
    request: &Accepted,
    reply: Reply,
    limits: ExecutionLimits,
) {
    let epoch = request.id.epoch;
    let session = request.id.session;
    if !state
        .windows
        .iter()
        .any(|w| w.epoch == epoch && w.session == session)
    {
        if state.windows.len() >= limits.sessions {
            state.windows.remove(0);
        }
        state.windows.push(Window {
            epoch,
            session,
            high: 0,
            receipts: Vec::new(),
        });
    }
    let window = state
        .windows
        .iter_mut()
        .find(|w| w.epoch == epoch && w.session == session)
        .expect("window exists");
    window.high = window.high.max(request.id.seq.0);
    window.receipts.push(Receipt {
        id: request.id,
        input: request.input.clone(),
        reply,
    });
    window.receipts.sort_by_key(|r| r.id.seq.0);
    while window.receipts.len() > limits.receipt_window {
        window.receipts.remove(0);
    }
}
fn pruned_windows(before: &[Window], after: &[Window]) -> bool {
    before.len() == after.len()
        && before.iter().zip(after).all(|(b, a)| {
            b.epoch == a.epoch
                && b.session == a.session
                && b.high == a.high
                && a.receipts.iter().all(|r| b.receipts.contains(r))
        })
}

impl HostAuthority for ExecutionHost {
    fn queue_diagnostics(&self, config: &QueueDiagnostics) -> Diagnostic<AuthorityQueues> {
        if let Err(reason) = config.check::<AuthorityQueues>() {
            return Diagnostic::Unavailable(reason);
        }
        Diagnostic::Available(AuthorityQueues {
            requests: RequestQueue {
                records: self.pending.len() as u64,
                bytes: Diagnostic::Unavailable(Unavailable::NotCollected),
            },
            frozen_tasks: self.runtime.work.as_ref().map_or(0, |work| {
                work.tasks.len().saturating_sub(work.next as usize) as u64
            }),
            scheduled_events: self.runtime.events.len() as u64,
            replicas: self.source.queue_diagnostics(config),
        })
    }
    type Model = ExecutionModel;
    type Prepared = ExecutionPrepared;
    fn binding(&self) -> Binding {
        self.binding
    }
    fn cursor(&self) -> CommitCursor {
        CommitCursor {
            world: self.binding.world,
            seq: self.world().seq(),
        }
    }
    fn resume_point(&self) -> ResumePoint {
        self.world().coordinator().resume_point()
    }
    fn is_tick_start(&self, p: ResumePoint) -> bool {
        p.next_phase == 0 && p.next_step == 0
    }
    fn at_tick_boundary(&self) -> bool {
        self.is_tick_start(self.resume_point()) && self.runtime.work.is_none()
    }
    fn submit(&mut self, request: WorldRequest<Call>) -> Admission {
        let id = request.id;
        if self.stopping.is_some() {
            return Admission::Rejected(RejectCode::Stopping);
        }
        if id.world != self.binding.world
            || id.epoch != self.binding.epoch
            || id.seq.0 == 0
            || !self
                .sessions
                .get(&id.session.0)
                .is_some_and(|s| s.viewer.actor == request.actor)
        {
            return Admission::Rejected(RejectCode::StaleSession);
        }
        if let Some(old) = self
            .receipt(id)
            .map(|r| &r.input)
            .or_else(|| self.pending_input(id))
        {
            return if old == &request.input {
                Admission::Duplicate
            } else {
                Admission::Rejected(RejectCode::DuplicateMismatch)
            };
        }
        let high = self
            .sessions
            .get(&id.session.0)
            .expect("authenticated session")
            .high;
        if id.seq.0 <= high {
            return Admission::Rejected(RejectCode::SequenceExpired);
        }
        if self.pending.len() >= self.limits.pending_requests
            || self.pending.len() as u64 >= self.limits.pending_bytes / self.limits.input.max_bytes
        {
            return Admission::Rejected(RejectCode::QuotaExceeded);
        }
        let mut encoded = Vec::new();
        if wire::encode(
            &request.input,
            &mut encoded,
            self.limits.input,
            self.limits.codec,
        )
        .is_err()
            || wire::decode::<Call>(&encoded, self.limits.input, self.limits.codec).is_err()
        {
            return Admission::Rejected(RejectCode::QuotaExceeded);
        }
        let point = self.resume_point();
        let target =
            if point.next_phase == 0 && self.runtime.work.is_none() && self.frozen.is_none() {
                point.tick.0
            } else {
                match point.tick.0.checked_add(1) {
                    Some(t) => t,
                    None => return Admission::Rejected(RejectCode::QuotaExceeded),
                }
            };
        if request.input.action == movement_action() {
            let Some(player) = self.players.get_mut(&id.session.0) else {
                return Admission::Rejected(RejectCode::PermissionDenied);
            };
            if player.last_tick == Some(Tick(target)) {
                return Admission::Rejected(RejectCode::QuotaExceeded);
            }
            player.last_tick = Some(Tick(target));
        } else if self.players.contains_key(&id.session.0) {
            return Admission::Rejected(RejectCode::PermissionDenied);
        }
        self.sessions
            .get_mut(&id.session.0)
            .expect("authenticated session")
            .high = id.seq.0;
        self.pending.push(Queued {
            request,
            tick: Tick(target),
        });
        Admission::Queued {
            target_tick: Tick(target),
        }
    }
    fn prepare_next(&mut self) -> Result<Option<ExecutionPrepared>, Fault> {
        self.prepare_with_memory()
    }
    fn publish(&mut self, mut prepared: ExecutionPrepared) -> Result<Publication, Fault> {
        if self.memory.is_some() && prepared.memory.is_none() {
            return Err(Fault::new(
                FaultCode::InvalidState,
                "publication lacks memory reservation",
            ));
        }
        let accounting = prepared.memory.take();
        let transition = prepared
            .record
            .changes
            .section
            .as_ref()
            .filter(|change| !change.fresh)
            .map(|change| {
                let pos = change.page.sections[0].pos;
                (pos, crate::visual::stamp(self, pos))
            });
        let publication = if let Some(sections) = prepared.world.sections {
            self.source.publish_sections(*sections)
        } else if let Some(section) = prepared.world.section {
            self.source.publish_section(*section)
        } else {
            self.source
                .publish(prepared.world.normal.expect("normal preparation"))
        }
        .map_err(commit_fault)?;
        if let Some(section) = &prepared.record.changes.section {
            let pos = section.page.sections[0].pos;
            if section.load {
                self.parked.remove(&pos);
            } else {
                self.parked.insert(pos, section.reference.clone());
            }
        }
        if let Some((pos, before)) = transition {
            if self.visual_transitions.len() == crate::visual::TRANSITION_LIMIT {
                self.visual_transitions.pop_front();
            }
            let after = crate::visual::stamp(self, pos);
            self.visual_transitions.push_back((pos, before, after));
        }
        if let Some(request) = &prepared.record.changes.cancelled
            && let Some(live) = self.sessions.get_mut(&request.id.session.0)
        {
            live.completed = live.completed.max(request.id.seq.0);
        }
        if prepared.record.changes.outcome.is_some()
            && let Some(request) = self
                .runtime
                .work
                .as_ref()
                .and_then(|w| w.tasks.get(w.next as usize))
                .and_then(|t| t.request.as_ref())
            && request.id.epoch == self.binding.epoch
            && let Some(live) = self.sessions.get_mut(&request.id.session.0)
        {
            live.completed = live.completed.max(request.id.seq.0);
        }
        self.runtime = prepared.runtime;
        self.publish_memory(accounting)?;
        self.release_prevalidation();
        for r in self
            .runtime
            .windows
            .iter()
            .flat_map(|w| &w.receipts)
            .filter(|r| r.id.epoch == self.binding.epoch)
        {
            if let Some(live) = self.sessions.get_mut(&r.id.session.0) {
                live.completed = live.completed.max(r.id.seq.0);
            }
        }
        self.pending
            .retain(|q| !prepared.remove_pending.contains(&q.request.id));
        self.frozen = None;
        Ok(Publication {
            at: publication.cursor,
            invalidated_replicas: publication.invalidated_replicas,
        })
    }
    fn capture_snapshot(
        &mut self,
        budget: OutputBudget,
    ) -> Result<WorldSnapshot<ExecutionSnapshot>, Fault> {
        if !self.at_tick_boundary() {
            return Err(Fault::invalid_data(
                "execution checkpoint requires tick start",
            ));
        }
        let parked_cost = wire::measure_inner(&self.parked, budget, self.limits.codec)?;
        let world_budget = OutputBudget {
            max_items: budget
                .max_items
                .checked_sub(self.limits.runtime.max_items)
                .and_then(|items| items.checked_sub(parked_cost.items))
                .ok_or_else(|| Fault::resource_limit("runtime snapshot reservation"))?,
            max_bytes: budget
                .max_bytes
                .checked_sub(
                    self.limits.runtime.max_bytes
                        + parked_cost.owned_bytes.max(parked_cost.encoded_bytes)
                        + size_of::<WorldSnapshot<ExecutionSnapshot>>() as u64,
                )
                .ok_or_else(|| Fault::resource_limit("runtime snapshot reservation"))?,
        };
        let meta = SnapshotMeta {
            cursor: self.cursor(),
            compatibility: self.binding.compatibility,
            resume_at: self.resume_point(),
        };
        let world = self
            .world()
            .capture_snapshot_with_budget(meta, world_budget)?;
        Ok(WorldSnapshot {
            meta,
            state: ExecutionSnapshot {
                world,
                runtime: self.runtime.clone(),
                parked: self.parked.clone(),
            },
        })
    }
    fn stop_admission(&mut self, deadline: Tick) -> Result<(), Fault> {
        self.stopping = Some(deadline);
        // Pending input is not durable admission. Work already frozen is settled
        // through the bounded deadline; future timed events stay in the snapshot.
        Ok(())
    }
    fn shutdown_ready(&self) -> bool {
        self.stopping.is_some()
            && self.pending.is_empty()
            && self.runtime.work.is_none()
            && self.resume_point().next_phase == 0
    }
}

pub struct ExecutionRecovery<'a> {
    parked: BTreeMap<SectionPos, ParkedSection>,
    section_pages: Option<crate::section_pages::SectionPages>,
    binding: Binding,
    recovery: Recovery<'a>,
    runtime: Option<RuntimeState>,
    point: Option<ResumePoint>,
    actions: ActionService,
    rules: Vec<Box<dyn DurableRule>>,
    limits: ExecutionLimits,
    budget_cache: RefCell<RuntimeBudget>,
}
impl<'a> ExecutionRecovery<'a> {
    pub fn new(
        binding: Binding,
        registry: &'a BlockStateRegistry,
        schemas: &'a ComponentSchemas,
        actions: ActionService,
        rules: Vec<Box<dyn DurableRule>>,
        limits: ExecutionLimits,
    ) -> Self {
        Self {
            parked: BTreeMap::new(),
            section_pages: None,
            binding,
            recovery: Recovery::new(binding.world, registry, schemas),
            runtime: None,
            point: None,
            actions,
            rules,
            limits,
            budget_cache: RefCell::default(),
        }
    }
    fn validate(&self, state: &RuntimeState, point: ResumePoint) -> Result<(), Fault> {
        if *state.rules != specifications(&self.rules)? {
            return Err(Fault::incompatible("execution rules mismatch"));
        }
        for member in state
            .work
            .iter()
            .flat_map(|w| w.tasks.iter())
            .filter_map(|t| t.transaction.as_ref())
            .flat_map(|g| &g.members)
        {
            for draft in &member.presentation {
                self.recovery
                    .schemas()
                    .validate_presentation(&member.source, draft)?;
            }
        }
        checked_state_cached(
            state,
            self.limits,
            point,
            self.binding.world,
            Some(&self.budget_cache),
        )
    }
}
impl RecoveryTarget for ExecutionRecovery<'_> {
    type Model = ExecutionModel;
    type Authority = ExecutionHost;
    fn restore_snapshot(
        &mut self,
        snapshot: WorldSnapshot<ExecutionSnapshot>,
    ) -> Result<(), Fault> {
        if snapshot.meta != snapshot.state.world.meta
            || snapshot.meta.compatibility != self.binding.compatibility
            || snapshot.meta.resume_at.next_phase != 0
            || snapshot.state.runtime.work.is_some()
        {
            return Err(Fault::invalid_data(
                "execution snapshot boundary or compatibility",
            ));
        }
        self.validate(&snapshot.state.runtime, snapshot.meta.resume_at)?;
        receipt_bounds(&snapshot.state.runtime, snapshot.meta.cursor)?;
        self.recovery
            .restore_snapshot(&snapshot.state.world)
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        self.point = Some(snapshot.meta.resume_at);
        self.runtime = Some(snapshot.state.runtime);
        self.parked = snapshot.state.parked;
        Ok(())
    }
    fn replay(&mut self, record: CommitRecord<ExecutionChanges>) -> Result<(), Fault> {
        if !record.changes.sections.is_empty() {
            return self.replay_section_batch(record);
        }
        if record.changes.section.is_some() {
            return self.replay_section(record);
        }
        let previous = self
            .point
            .ok_or_else(|| Fault::invalid_data("missing execution snapshot"))?;
        if record.meta != record.changes.world.meta
            || record.meta.compatibility != self.binding.compatibility
        {
            return Err(Fault::invalid_data("execution metadata mismatch"));
        }
        let before = self.runtime.as_ref().expect("snapshot applied");
        if matches!(record.changes.runtime, RuntimeUpdate::Progress(_))
            && (record.changes.outcome.is_none()
                || record.changes.cancelled.is_some()
                || record.meta.resume_at != previous)
        {
            return Err(Fault::invalid_data("progress outside task settlement"));
        }
        let resolved = record.changes.runtime.resolve(before)?;
        self.validate(&resolved, record.meta.resume_at)?;
        receipt_bounds(&resolved, record.meta.cursor)?;
        let after = &resolved;
        let world = &record.changes.world;
        let empty = world.edits.is_empty()
            && world.entity_moves.is_empty()
            && world.component_edits.is_empty()
            && world.entity_lifecycle.is_none();
        // Only freeze, settle exactly one task, or finish exactly one phase.
        let valid = if let Some(request) = &record.changes.cancelled {
            if !empty
                || record.changes.outcome.is_some()
                || record.meta.resume_at != previous
                || before
                    .windows
                    .iter()
                    .flat_map(|w| &w.receipts)
                    .any(|r| r.id == request.id)
                || before
                    .work
                    .iter()
                    .flat_map(|w| w.tasks.iter().skip(w.next as usize))
                    .any(|t| t.request.as_ref().is_some_and(|r| r.id == request.id))
            {
                false
            } else {
                let mut expected = before.clone();
                add_receipt(
                    &mut expected,
                    request,
                    Reply {
                        status: ReplyStatus::Rejected(RejectCode::Stopping),
                        fields: ComponentFields::new(),
                        at: record.meta.cursor,
                    },
                    self.limits,
                );
                fit_state(
                    &mut expected,
                    self.limits,
                    previous,
                    record.meta.cursor.world,
                )?;
                expected == *after
            }
        } else {
            match (&before.work, &after.work) {
                (None, Some(work)) => {
                    empty
                        && record.changes.outcome.is_none()
                        && record.meta.resume_at == previous
                        && work.next == 0
                        && before.events == after.events
                        && pruned_windows(&before.windows, &after.windows)
                        && before.next_event == after.next_event
                        && valid_batch_freeze(before, work, previous)
                }
                (Some(old), Some(new)) => {
                    if old.next as usize == old.tasks.len() {
                        empty
                            && record.changes.outcome.is_none()
                            && record.meta.resume_at == previous
                            && new.event_batch
                            && before.events == after.events
                            && before.next_event == after.next_event
                            && pruned_windows(&before.windows, &after.windows)
                            && valid_batch_freeze(before, new, previous)
                    } else if record.meta.resume_at != previous
                        || old.tasks != new.tasks
                        || old.event_batch != new.event_batch
                        || (old.next as usize) >= old.tasks.len()
                    {
                        false
                    } else {
                        let status = record
                            .changes
                            .outcome
                            .as_ref()
                            .ok_or_else(|| Fault::invalid_data("missing task outcome"))?;
                        let task = &old.tasks[old.next as usize];
                        matches_plan(task, status, world, &self.recovery)
                            && settle(
                                before,
                                task,
                                status,
                                record.meta.cursor,
                                previous,
                                self.limits,
                                Some(&self.budget_cache),
                            )? == *after
                    }
                }
                (Some(old), None) => {
                    empty
                        && record.changes.outcome.is_none()
                        && old.next as usize == old.tasks.len()
                        && next_resume_point(previous) == Some(record.meta.resume_at)
                        && before.events == after.events
                        && before.windows == after.windows
                        && before.next_event == after.next_event
                        && !(Phase::from_index(previous.next_phase) == Some(Phase::Simulation)
                            && before
                                .events
                                .first()
                                .is_some_and(|event| event.event.tick <= previous.tick))
                }
                _ => false,
            }
        };
        if !valid {
            return Err(Fault::invalid_data("illegal execution transition"));
        }
        self.recovery
            .replay(world)
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        self.point = Some(record.meta.resume_at);
        self.runtime = Some(resolved);
        Ok(())
    }
    fn finish(self, expected: CommitCursor) -> Result<ExecutionHost, Fault> {
        self.validate_parked()?;
        if self.recovery.cursor() != expected {
            return Err(Fault::invalid_data("execution recovery cursor"));
        }
        let world = self
            .recovery
            .finish()
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        let mut host = ExecutionHost::from_state(
            self.binding,
            world,
            self.actions,
            self.rules,
            self.limits,
            self.runtime
                .ok_or_else(|| Fault::invalid_data("missing runtime"))?,
        )?;
        host.parked = self.parked;
        Ok(host)
    }
}

impl ExecutionHost {
    fn prepare_unbudgeted(&mut self) -> Result<Option<ExecutionPrepared>, Fault> {
        if let Some(prepared) = self.section_pending.take() {
            return Ok(Some(prepared));
        }
        if self.at_tick_boundary() && self.waiting_for_event_sections()? {
            return Ok(None);
        }
        if self.shutdown_ready() {
            return Ok(None);
        }
        // Reject future admissions immediately, even if shutdown begins after
        // their execution phase. This control record never advances the phase.
        if let Some(q) = self
            .pending
            .iter()
            .find(|q| self.stopping.is_some_and(|end| q.tick > end))
        {
            let request = Accepted {
                id: q.request.id,
                input: q.request.input.clone(),
            };
            let world = self.source.prepare_barrier().map_err(commit_fault)?;
            let mut state = self.runtime.clone();
            add_receipt(
                &mut state,
                &request,
                Reply {
                    status: ReplyStatus::Rejected(RejectCode::Stopping),
                    fields: ComponentFields::new(),
                    at: world.meta().cursor,
                },
                self.limits,
            );
            fit_state(
                &mut state,
                self.limits,
                self.resume_point(),
                self.binding.world,
            )?;
            let mut prepared = self.record(world, state, vec![request.id])?;
            prepared.record.changes.cancelled = Some(request);
            return Ok(Some(prepared));
        }
        if let Some(work) = &self.runtime.work {
            if let Some(task) = work.tasks.get(work.next as usize) {
                self.fill_prevalidation();
                return self.prepare_task(task).map(Some);
            }
            if self.has_due_events() {
                if self.frozen.is_none() {
                    self.frozen = Some(self.collect_event_batch()?);
                }
                let mut state = self.runtime.clone();
                state.work = self.frozen.clone();
                fit_state(
                    &mut state,
                    self.limits,
                    self.resume_point(),
                    self.binding.world,
                )?;
                return self
                    .record(
                        self.source.prepare_barrier().map_err(commit_fault)?,
                        state,
                        Vec::new(),
                    )
                    .map(Some);
            }
            let mut state = self.runtime.clone();
            state.work = None;
            let advance = self.source.prepare_phase_advance().map_err(commit_fault)?;
            return self.record(advance, state, Vec::new()).map(Some);
        }
        // Retain a pure phase plan while journal reservation is blocked; no input,
        // event, result or identity is consumed until publication succeeds.
        if self.frozen.is_none() {
            self.frozen = Some(self.collect_phase()?);
        }
        let work = self.frozen.as_ref().expect("phase was collected").clone();
        let mut state = self.runtime.clone();
        state.work = Some(work);
        // Untrusted request load becomes bounded deferral, never a world fault.
        // Native rules are trusted planners and must fit their declared phase budget.
        loop {
            if fit_state(
                &mut state,
                self.limits,
                self.resume_point(),
                self.binding.world,
            )
            .is_ok()
            {
                break;
            }
            let work = state.work.as_mut().expect("phase work");
            let Some(index) = work.tasks.iter().rposition(|t| t.request.is_some()) else {
                return Err(Fault::resource_limit(
                    "native phase output exceeds runtime capacity",
                ));
            };
            if work.tasks.len() == 1 {
                let t = &mut Arc::make_mut(&mut work.tasks)[index];
                t.transaction = None;
                t.output.clear();
                t.status = ReplyStatus::Rejected(RejectCode::QuotaExceeded);
                fit_state(
                    &mut state,
                    self.limits,
                    self.resume_point(),
                    self.binding.world,
                )?;
                break;
            }
            Arc::make_mut(&mut work.tasks).remove(index);
        }
        let remove_pending = state
            .work
            .as_ref()
            .expect("phase work")
            .tasks
            .iter()
            .filter_map(|t| t.request.as_ref().map(|r| r.id))
            .collect();
        self.record(
            self.source.prepare_barrier().map_err(commit_fault)?,
            state,
            remove_pending,
        )
        .map(Some)
    }
}
