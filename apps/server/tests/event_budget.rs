//! Real-file regression for bounded durable-event draining.
mod support;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use support::Directory;
use torse_server::{bootstrap as base, execution::*, *};
use torse_sim::durable::*;
use torse_sim::{OrderingKey, Phase, SystemSpec, Transaction, WorldAuthority};
use torse_types::*;
use torse_world::*;

fn key() -> ContentKey {
    base::key("test:event-budget")
}
#[derive(Clone, Default)]
struct Trace {
    events: Rc<RefCell<Vec<u128>>>,
    plans: Rc<Cell<u32>>,
    output_bytes: Rc<Cell<usize>>,
    invalid: Rc<Cell<bool>>,
    scoped: Rc<Cell<bool>>,
}
struct Burst {
    trace: Trace,
    count: u32,
    ordinary: bool,
    cancel: bool,
}
impl DurableRule for Burst {
    fn spec(&self) -> SystemSpec {
        SystemSpec {
            id: key(),
            phase: Phase::Simulation,
            rule_priority: 0,
            before: vec![],
            after: vec![],
        }
    }
    fn plan(&self, _: &WorldAuthority, tick: Tick) -> Result<Vec<WorkPlan>, Fault> {
        if tick == Tick(2) {
            self.trace.plans.set(self.trace.plans.get() + 1);
        }
        if tick == Tick(0) {
            return Ok(vec![WorkPlan {
                transaction: None,
                schedule: (0..self.count)
                    .map(|_| {
                        let event = EventDraft {
                            tick: Tick(2),
                            rule: key(),
                            payload: ComponentFields::new(),
                        };
                        if self.trace.scoped.get() {
                            event
                                .with_dependencies(vec![
                                    EventDependency::Section(SectionPos::ZERO),
                                    EventDependency::Entity(EntityId(1)),
                                ])
                                .unwrap()
                        } else {
                            event
                        }
                    })
                    .collect(),
                cancel: vec![],
            }]);
        }
        Ok(if self.ordinary {
            vec![WorkPlan {
                transaction: None,
                schedule: vec![],
                cancel: vec![],
            }]
        } else {
            vec![]
        })
    }
    fn event(
        &self,
        world: &WorldAuthority,
        _: Tick,
        event: &TimedEvent,
    ) -> Result<WorkPlan, Fault> {
        self.trace.events.borrow_mut().push(event.id.0);
        if self.trace.invalid.get() {
            return Err(Fault::invalid_data("invalid test event"));
        }
        let pos = BlockPos::new(8 + event.id.0 as i64, 1, 1);
        let mut tx = Transaction::new(
            TransactionId(100 + event.id.0),
            key(),
            OrderingKey {
                phase: Phase::Simulation,
                rule_priority: 0,
                stable_task_key: 0,
                local_sequence: 0,
            },
        );
        tx.read_block(pos, world.store().query(pos).ready().unwrap())
            .write_block(
                pos,
                world
                    .registry()
                    .default_state(&base::key("base:stone"))
                    .unwrap(),
            );
        let mut plan = WorkPlan::transaction(tx);
        if self.trace.output_bytes.get() != 0 && event.id.0 <= 2 {
            plan.schedule.push(EventDraft {
                tick: Tick(100),
                rule: key(),
                payload: [(
                    base::key("test:large"),
                    ComponentValue::Text("a".repeat(self.trace.output_bytes.get())),
                )]
                .into(),
            });
        }
        if self.cancel && event.id == EventId(1) {
            plan.cancel = vec![EventId(2), EventId(3)];
        }
        Ok(plan)
    }
}
fn rules(trace: &Trace, count: u32, ordinary: bool, cancel: bool) -> Vec<Box<dyn DurableRule>> {
    vec![Box::new(Burst {
        trace: trace.clone(),
        count,
        ordinary,
        cancel,
    })]
}
fn limits() -> ExecutionLimits {
    ExecutionLimits {
        phase_tasks: 2,
        events: 8,
        ..base::execution_limits()
    }
}
fn create(dir: &Directory, trace: &Trace, count: u32, ordinary: bool, cancel: bool) -> base::Files {
    create_with_limits(dir, trace, count, ordinary, cancel, limits())
}
fn create_with_limits(
    dir: &Directory,
    trace: &Trace,
    count: u32,
    ordinary: bool,
    cancel: bool,
    limits: ExecutionLimits,
) -> base::Files {
    let (world, actions) = base::world().unwrap();
    base::Files::create_file(
        dir.acquire(),
        world.coordinator().compatibility(),
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            ExecutionHost::new(
                binding,
                world,
                actions,
                rules(trace, count, ordinary, cancel),
                limits,
            )
        },
    )
    .unwrap()
}
fn reopen(dir: &Directory, trace: &Trace, count: u32, ordinary: bool, cancel: bool) -> base::Files {
    let (world, actions) = base::world().unwrap();
    base::Files::open_file(
        dir.acquire(),
        world.coordinator().compatibility(),
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                world.registry(),
                world.schemas(),
                actions,
                rules(trace, count, ordinary, cancel),
                limits(),
            ))
        },
    )
    .unwrap()
}
fn tick(server: &mut base::Files) {
    server.drive_tick(&mut NoReplicas, 1024).unwrap();
}
fn assert_blocks(server: &base::Files, present: &[u128], absent: &[u128]) {
    for (ids, filled) in [(present, true), (absent, false)] {
        for id in ids {
            let state = server
                .authority()
                .unwrap()
                .world()
                .store()
                .query(BlockPos::new(8 + *id as i64, 1, 1))
                .ready()
                .unwrap();
            assert_eq!(!state.is_empty(), filled, "event {id}");
        }
    }
}

#[test]
fn three_due_events_drain_with_budget_two_and_ordinary_rules_once() {
    for ordinary in [false, true] {
        let dir = Directory::new();
        let trace = Trace::default();
        let mut server = create(&dir, &trace, 3, ordinary, false);
        tick(&mut server);
        tick(&mut server);
        while server.authority().unwrap().resume_point().tick == Tick(2) {
            let before = trace.events.borrow().len();
            server.drive_one(&mut NoReplicas).unwrap();
            assert!(trace.events.borrow().len() - before <= 2);
            if let Some(work) = &server.authority().unwrap().runtime().work {
                assert!(work.tasks.len() <= 2);
            }
        }
        assert_eq!(*trace.events.borrow(), [1, 2, 3]);
        assert_eq!(trace.plans.get(), 1);
        assert!(server.authority().unwrap().runtime().events.is_empty());
        assert_blocks(&server, &[1, 2, 3], &[]);
        server.checkpoint().unwrap();
        drop(server);
        let recovered_trace = Trace::default();
        let mut server = reopen(&dir, &recovered_trace, 3, ordinary, false);
        tick(&mut server);
        assert!(recovered_trace.events.borrow().is_empty());
        assert_blocks(&server, &[1, 2, 3], &[]);
    }
}

#[test]
fn partial_event_batches_resume_without_replanning_or_duplicate_effects() {
    for stop in 0..9 {
        let dir = Directory::new();
        let trace = Trace::default();
        let mut server = create(&dir, &trace, 5, true, false);
        tick(&mut server);
        tick(&mut server);
        loop {
            let point = server.authority().unwrap().resume_point();
            if point.tick == Tick(2) && point.next_phase == 3 {
                break;
            }
            server.drive_one(&mut NoReplicas).unwrap();
        }
        for _ in 0..stop {
            server.drive_one(&mut NoReplicas).unwrap();
        }
        let before = trace.events.borrow().clone();
        drop(server);
        let after = Trace::default();
        let mut server = reopen(&dir, &after, 5, true, false);
        assert!(
            after.events.borrow().is_empty(),
            "recovery must not invoke planners"
        );
        tick(&mut server);
        assert_blocks(&server, &[1, 2, 3, 4, 5], &[]);
        assert!(server.authority().unwrap().runtime().events.is_empty());
        assert!(
            before.len() + after.events.borrow().len() <= 5,
            "frozen callbacks must not replay"
        );
        assert_eq!(trace.plans.get() + after.plans.get(), 1);
    }
}

#[test]
fn cancellation_skips_frozen_and_unplanned_events() {
    let dir = Directory::new();
    let trace = Trace::default();
    let mut server = create(&dir, &trace, 5, false, true);
    tick(&mut server);
    tick(&mut server);
    tick(&mut server);
    assert_eq!(*trace.events.borrow(), [1, 2, 4, 5]);
    assert_blocks(&server, &[1, 4, 5], &[2, 3]);
    drop(server);
    let server = reopen(&dir, &Trace::default(), 5, false, true);
    assert_blocks(&server, &[1, 4, 5], &[2, 3]);
}

#[test]
fn cancelled_frozen_event_stays_inert_after_reopening() {
    let dir = Directory::new();
    let trace = Trace::default();
    let mut server = create(&dir, &trace, 5, false, true);
    tick(&mut server);
    tick(&mut server);
    while server.authority().unwrap().runtime().events.len() == 5 {
        server.drive_one(&mut NoReplicas).unwrap();
    }
    drop(server);
    let trace = Trace::default();
    let mut server = reopen(&dir, &trace, 5, false, true);
    tick(&mut server);
    assert_eq!(*trace.events.borrow(), [4, 5]);
    assert_blocks(&server, &[1, 4, 5], &[2, 3]);
}

#[test]
fn request_deduplication_survives_event_batch_reopening() {
    use torse_sim::action::{ActionTarget, ActorContext};
    let dir = Directory::new();
    let trace = Trace::default();
    let mut server = create(&dir, &trace, 3, false, false);
    tick(&mut server);
    tick(&mut server);
    server
        .register_session(
            Viewer {
                session: SessionId(1),
                actor: ActorContextId(1),
            },
            ActorContext::new(base::key("test:player"))
                .permission(base::key("inventory:permission/transfer"))
                .target(ActionTarget::Entity(EntityId(1)))
                .target(ActionTarget::Entity(EntityId(2))),
            base::scope(),
        )
        .unwrap();
    let id = RequestId {
        world: base::WORLD,
        epoch: server.binding().epoch,
        session: SessionId(1),
        seq: RequestSeq(1),
    };
    let input = Call {
        action: base::key("inventory:action/transfer"),
        version: 2,
        target: ActionTarget::Entity(EntityId(1)),
        parameters: [
            (
                base::key("inventory:field/destination"),
                ComponentValue::Entity(EntityId(2)),
            ),
            (
                base::key("inventory:field/source_slot"),
                ComponentValue::U32(0),
            ),
            (
                base::key("inventory:field/destination_slot"),
                ComponentValue::U32(0),
            ),
            (base::key("inventory:field/count"), ComponentValue::U32(1)),
        ]
        .into(),
    };
    let request = || WorldRequest {
        id,
        actor: ActorContextId(1),
        input: input.clone(),
    };
    assert!(matches!(server.submit(request()), Admission::Queued { .. }));
    while server.authority().unwrap().runtime().events.len() == 3 {
        server.drive_one(&mut NoReplicas).unwrap();
    }
    assert_eq!(server.submit(request()), Admission::Duplicate);
    assert_eq!(
        server.receipt(id).unwrap().unwrap().reply.status,
        ReplyStatus::Committed
    );
    drop(server);
    let mut server = reopen(&dir, &Trace::default(), 3, false, false);
    assert_eq!(
        server.receipt(id).unwrap().unwrap().reply.status,
        ReplyStatus::Committed
    );
    tick(&mut server);
    let world = server.authority().unwrap().world();
    let fields = world
        .entities()
        .get(world.entities().handle_of(EntityId(2)).unwrap())
        .unwrap()
        .components
        .component(&torse_inventory::schema())
        .unwrap();
    let catalog = torse_inventory::ItemCatalog::new([(base::key("base:stone"), 64)]).unwrap();
    assert_eq!(
        torse_inventory::Inventory::from_fields(fields, &catalog)
            .unwrap()
            .slots()[0]
            .as_ref()
            .unwrap()
            .count,
        1
    );
}

#[test]
fn individually_invalid_or_oversized_events_are_explicit_errors() {
    for oversized in [false, true] {
        let dir = Directory::new();
        let trace = Trace::default();
        let mut server = create(&dir, &trace, 1, false, false);
        tick(&mut server);
        tick(&mut server);
        trace.invalid.set(!oversized);
        trace
            .output_bytes
            .set(if oversized { 3_000_000 } else { 0 });
        let error = server.drive_tick(&mut NoReplicas, 1024).unwrap_err();
        assert_eq!(
            error.code,
            if oversized {
                FaultCode::ResourceLimit
            } else {
                FaultCode::InvalidData
            }
        );
        assert_eq!(*trace.events.borrow(), [1]);
        drop(server);
        let mut server = reopen(&dir, &Trace::default(), 1, false, false);
        assert_eq!(server.authority().unwrap().runtime().events.len(), 1);
        tick(&mut server);
        assert_blocks(&server, &[1], &[]);
    }
}

#[test]
fn event_batches_shrink_to_fit_runtime_bytes() {
    let dir = Directory::new();
    let trace = Trace::default();
    let mut bound = limits();
    bound.runtime.max_bytes = 36_000;
    let mut server = create_with_limits(&dir, &trace, 3, false, false, bound);
    tick(&mut server);
    tick(&mut server);
    trace.output_bytes.set(10_000);
    let mut batches = vec![];
    while server.authority().unwrap().resume_point().tick == Tick(2) {
        let before = server.authority().unwrap().runtime().work.clone();
        server.drive_one(&mut NoReplicas).unwrap();
        let after = server.authority().unwrap().runtime().work.as_ref();
        if let Some(work) = after
            && work.event_batch
            && work.next == 0
            && before
                .as_ref()
                .is_none_or(|old| old.next as usize == old.tasks.len())
        {
            batches.push(work.tasks.len());
        }
    }
    assert_eq!(batches, [1, 2]);
    assert_blocks(&server, &[1, 2, 3], &[]);
    assert_eq!(server.authority().unwrap().runtime().events.len(), 2);
}

#[test]
fn scoped_dependencies_and_checkpoint_boundaries_survive_partial_drain() {
    let dir = Directory::new();
    let trace = Trace::default();
    trace.scoped.set(true);
    let mut server = create(&dir, &trace, 3, false, false);
    tick(&mut server);
    tick(&mut server);
    assert!(
        server
            .authority()
            .unwrap()
            .section_is_event_pinned(SectionPos::ZERO)
            .unwrap()
    );
    while server.authority().unwrap().runtime().events.len() == 3 {
        server.drive_one(&mut NoReplicas).unwrap();
    }
    let point = server.authority().unwrap().resume_point();
    let queued = server.authority().unwrap().runtime().events.clone();
    assert_eq!(
        server.checkpoint().unwrap_err().code,
        FaultCode::InvalidState
    );
    assert_eq!(server.authority().unwrap().resume_point(), point);
    assert_eq!(server.authority().unwrap().runtime().events, queued);
    assert!(!server.authority().unwrap().residency_ready(true));
    drop(server);
    let mut server = reopen(&dir, &Trace::default(), 3, false, false);
    let tickets = server
        .authority()
        .unwrap()
        .event_tickets(false, 64)
        .unwrap();
    assert!(!tickets.global);
    assert!(tickets.sections.contains(&SectionPos::ZERO));
    tick(&mut server);
    assert!(
        !server
            .authority()
            .unwrap()
            .section_is_event_pinned(SectionPos::ZERO)
            .unwrap()
    );
    assert_blocks(&server, &[1, 2, 3], &[]);
}

#[test]
fn overdue_events_follow_deadline_then_identity_and_replay_rejects_bad_batches() {
    let (world, actions) = base::world().unwrap();
    let registry = world.registry().clone();
    let schemas = world.schemas().clone();
    let binding = Binding {
        world: base::WORLD,
        epoch: WorldEpoch(1),
        compatibility: world.coordinator().compatibility(),
    };
    let trace = Trace::default();
    let mut host = ExecutionHost::new(
        binding,
        world,
        actions,
        rules(&trace, 3, false, false),
        limits(),
    )
    .unwrap();
    let mut snapshot = host.capture_snapshot(base::budget()).unwrap();
    snapshot.meta.resume_at.tick = Tick(2);
    snapshot.state.world.meta.resume_at.tick = Tick(2);
    snapshot.state.runtime.next_event = 4;
    snapshot.state.runtime.events = [(2, 0), (1, 1), (3, 1)]
        .into_iter()
        .map(|(id, tick)| TimedEvent {
            id: EventId(id),
            event: EventDraft {
                tick: Tick(tick),
                rule: key(),
                payload: ComponentFields::new(),
            },
        })
        .collect();
    let (_, actions) = base::world().unwrap();
    let mut recovery = ExecutionRecovery::new(
        binding,
        &registry,
        &schemas,
        actions,
        rules(&trace, 3, false, false),
        limits(),
    );
    recovery
        .restore_snapshot(WorldSnapshot {
            meta: snapshot.meta,
            state: snapshot.state.clone(),
        })
        .unwrap();
    let mut host = recovery.finish(snapshot.meta.cursor).unwrap();
    let (_, actions) = base::world().unwrap();
    let mut recovery = ExecutionRecovery::new(
        binding,
        &registry,
        &schemas,
        actions,
        rules(&trace, 3, false, false),
        limits(),
    );
    recovery.restore_snapshot(snapshot).unwrap();
    let mut checked = false;
    while host.resume_point().tick == Tick(2) {
        let prepared = host.prepare_next().unwrap().unwrap();
        let record = CommitRecord {
            meta: prepared.record().meta,
            changes: prepared.record().changes.clone(),
        };
        if let RuntimeUpdate::Full(state) = &record.changes.runtime
            && let Some(work) = &state.work
            && work.event_batch
            && !checked
        {
            let mut bad = CommitRecord {
                meta: record.meta,
                changes: record.changes.clone(),
            };
            let RuntimeUpdate::Full(state) = &mut bad.changes.runtime else {
                unreachable!()
            };
            std::sync::Arc::make_mut(&mut state.work.as_mut().unwrap().tasks).swap(0, 1);
            assert!(recovery.replay(bad).is_err());
            checked = true;
        }
        recovery.replay(record).unwrap();
        host.publish(prepared).unwrap();
    }
    assert!(checked);
    assert_eq!(*trace.events.borrow(), [2, 1, 3]);
    assert_eq!(trace.plans.get(), 1);
    let host = recovery.finish(host.cursor()).unwrap();
    assert!(host.runtime().events.is_empty());
}

#[test]
fn legacy_phase_encoding_is_unchanged_and_event_marker_roundtrips() {
    use torse_codec::wire;
    let budget = base::budget();
    let codec = base::codec_limits();
    let work = PhaseWork {
        tasks: vec![FrozenTask {
            transaction: None,
            request: None,
            consume: Some(EventId(1)),
            schedule: vec![],
            cancel: vec![],
            status: ReplyStatus::Committed,
            output: ComponentFields::new(),
        }]
        .into(),
        next: 0,
        event_batch: false,
    };
    let mut ordinary = vec![];
    wire::encode(&work, &mut ordinary, budget, codec).unwrap();
    assert_eq!(&ordinary[ordinary.len() - 4..], &0u32.to_le_bytes());
    let mut event = work.clone();
    event.event_batch = true;
    let mut encoded = vec![];
    wire::encode(&event, &mut encoded, budget, codec).unwrap();
    assert_eq!(
        &encoded[..encoded.len() - 4],
        &ordinary[..ordinary.len() - 4]
    );
    assert_eq!(&encoded[encoded.len() - 4..], &(1u32 << 31).to_le_bytes());
    assert_eq!(
        wire::decode::<PhaseWork>(&ordinary, budget, codec).unwrap(),
        work
    );
    assert_eq!(
        wire::decode::<PhaseWork>(&encoded, budget, codec).unwrap(),
        event
    );
    event.next = 1 << 31;
    assert!(wire::encode(&event, &mut vec![], budget, codec).is_err());
}

#[test]
fn event_batch_abort_child() {
    let Some(path) = std::env::var_os("TORSE_EVENT_BATCH_ABORT") else {
        return;
    };
    let dir = Directory(path.into());
    let trace = Trace::default();
    let mut server = create(&dir, &trace, 3, true, false);
    tick(&mut server);
    tick(&mut server);
    let remaining = std::env::var("TORSE_EVENT_BATCH_REMAINING")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    loop {
        let host = server.authority().unwrap();
        let frozen = host
            .runtime()
            .work
            .as_ref()
            .is_some_and(|work| work.event_batch);
        if frozen && host.runtime().events.len() == remaining {
            break;
        }
        server.drive_one(&mut NoReplicas).unwrap();
    }
    // Terminate without Rust destructors immediately after a durable drive step.
    std::process::exit(53);
}

#[test]
fn process_exit_between_freeze_settlement_and_batches_keeps_exact_effects() {
    for remaining in [3, 2, 1] {
        let dir = Directory::new();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "event_batch_abort_child", "--nocapture"])
            .env("TORSE_EVENT_BATCH_ABORT", &dir.0)
            .env("TORSE_EVENT_BATCH_REMAINING", remaining.to_string())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(53));
        let trace = Trace::default();
        let mut server = reopen(&dir, &trace, 3, true, false);
        assert!(trace.events.borrow().is_empty());
        assert_eq!(
            server.authority().unwrap().runtime().events.len(),
            remaining
        );
        tick(&mut server);
        assert_blocks(&server, &[1, 2, 3], &[]);
        assert!(server.authority().unwrap().runtime().events.is_empty());
        assert_eq!(trace.plans.get(), 0);
        assert_eq!(*trace.events.borrow(), [3]);
    }
}
