//! Pre-freeze parallel planning must preserve durable serial arbitration/replay.
mod support;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use support::Directory;
use torse_codec::{StorageCodec, wire};
use torse_server::{bootstrap as base, execution::*, *};
use torse_sim::action::ActorContext;
use torse_sim::durable::*;
use torse_sim::{OrderingKey, Phase, SystemSpec, Transaction, WorldAuthority};
use torse_types::*;
use torse_world::{BlockStateId, ComponentFields};

#[derive(Default)]
struct Probe {
    calls: AtomicUsize,
    events: AtomicUsize,
    active: AtomicUsize,
    peak: AtomicUsize,
    deny: AtomicBool,
    completed: Mutex<Vec<usize>>,
    changed: Condvar,
}

struct Rule {
    index: usize,
    probe: Arc<Probe>,
    reverse: bool,
}
impl DurableRule for Rule {
    fn spec(&self) -> SystemSpec {
        SystemSpec {
            id: base::key(&format!("planning:r{}", self.index)),
            phase: Phase::Simulation,
            rule_priority: 0,
            before: vec![],
            after: vec![],
        }
    }
    fn parallel_planner(&self) -> Option<&(dyn DurableRule + Sync)> {
        Some(self)
    }
    fn plan(&self, world: &WorldAuthority, tick: Tick) -> Result<Vec<WorkPlan>, Fault> {
        assert!(
            !self.probe.deny.load(Ordering::SeqCst),
            "recovery ran a planner"
        );
        self.probe.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.probe.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.probe.peak.fetch_max(active, Ordering::SeqCst);
        if self.reverse && self.index.is_multiple_of(2) {
            let completed = self.probe.completed.lock().unwrap();
            let (completed, timeout) = self
                .probe
                .changed
                .wait_timeout_while(completed, Duration::from_secs(5), |c| {
                    !c.contains(&(self.index + 1))
                })
                .unwrap();
            assert!(!timeout.timed_out(), "independent partner did not run");
            drop(completed);
        }
        let output = if tick == Tick(0) {
            // Identical arbitration keys across rules: registration order wins.
            let pos = BlockPos::new(8, 1, 1);
            let previous = world.store().query(pos).ready().unwrap();
            assert!(
                previous.is_empty(),
                "every planner sees the phase-start world"
            );
            let mut tx = Transaction::new(
                TransactionId(7),
                self.spec().id.clone(),
                OrderingKey {
                    phase: Phase::Simulation,
                    rule_priority: 0,
                    stable_task_key: 0,
                    local_sequence: 0,
                },
            );
            tx.read_block(pos, previous).write_block(
                pos,
                world
                    .registry()
                    .default_state(&base::key("base:stone"))
                    .unwrap(),
            );
            let mut work = WorkPlan::transaction(tx);
            work.schedule.push(EventDraft {
                tick: Tick(1),
                rule: self.spec().id,
                payload: ComponentFields::new(),
            });
            vec![
                work,
                WorkPlan {
                    transaction: None,
                    schedule: vec![],
                    cancel: vec![],
                },
            ]
        } else {
            vec![]
        };
        self.probe.completed.lock().unwrap().push(self.index);
        self.probe.changed.notify_all();
        self.probe.active.fetch_sub(1, Ordering::SeqCst);
        Ok(output)
    }
    fn event(&self, _: &WorldAuthority, _: Tick, _: &TimedEvent) -> Result<WorkPlan, Fault> {
        assert!(
            !self.probe.deny.load(Ordering::SeqCst),
            "recovery ran an event callback"
        );
        self.probe.events.fetch_add(1, Ordering::SeqCst);
        Ok(WorkPlan {
            transaction: None,
            schedule: vec![],
            cancel: vec![],
        })
    }
}
fn rules(probe: &Arc<Probe>, reverse: bool) -> Vec<Box<dyn DurableRule>> {
    // Deliberately shuffled registration, canonical rule identity determines order.
    [3, 0, 2, 1]
        .into_iter()
        .map(|index| {
            Box::new(Rule {
                index,
                probe: Arc::clone(probe),
                reverse,
            }) as Box<dyn DurableRule>
        })
        .collect()
}
fn binding(world: &WorldAuthority) -> Binding {
    Binding {
        world: base::WORLD,
        epoch: WorldEpoch(1),
        compatibility: world.coordinator().compatibility(),
    }
}
fn create(dir: &Directory, rules: Vec<Box<dyn DurableRule>>) -> base::Files {
    let (world, actions) = base::world().unwrap();
    base::Files::create_file(
        dir.acquire(),
        world.coordinator().compatibility(),
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| ExecutionHost::new(binding, world, actions, rules, base::execution_limits()),
    )
    .unwrap()
}
fn reopen(dir: &Directory, rules: Vec<Box<dyn DurableRule>>) -> base::Files {
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
                rules,
                base::execution_limits(),
            ))
        },
    )
    .unwrap()
}
fn bytes(host: &ExecutionHost) -> Vec<u8> {
    let meta = SnapshotMeta {
        cursor: host.cursor(),
        compatibility: host.binding().compatibility,
        resume_at: host.resume_point(),
    };
    let snapshot = WorldSnapshot {
        meta,
        state: ExecutionSnapshot {
            parked: Default::default(),
            world: host.world().capture_snapshot(meta).unwrap(),
            runtime: host.runtime().clone(),
        },
    };
    let mut out = vec![];
    base::Codec::new(base::codec_limits())
        .unwrap()
        .encode_snapshot(&snapshot, &mut out, base::budget())
        .unwrap();
    out
}
fn until_simulation(host: &mut ExecutionHost) {
    while usize::from(host.resume_point().next_phase) != Phase::Simulation.index() {
        let prepared = host.prepare_next().unwrap().unwrap();
        host.publish(prepared).unwrap();
    }
}

#[test]
fn one_and_four_workers_preserve_journal_bytes_events_replies_and_replicas() {
    let mut reference = None;
    for workers in [0, 1, 4] {
        let dir = Directory::new();
        let probe = Arc::new(Probe::default());
        let mut server = create(&dir, rules(&probe, workers == 4));
        if workers != 0 {
            server
                .set_parallel_planning(Some(PlanningConfig {
                    workers,
                    max_in_flight: 2,
                }))
                .unwrap();
        }
        if workers == 4 {
            server.set_commit_batch_limit(16).unwrap();
        }
        server
            .register_session(
                Viewer {
                    session: SessionId(1),
                    actor: ActorContextId(1),
                },
                ActorContext::new(base::key("planning:viewer")),
                base::scope(),
            )
            .unwrap();
        let subscription = server
            .open_replica(SessionId(1), base::scope(), base::budget())
            .unwrap()
            .subscription;
        let request = WorldRequest {
            id: RequestId {
                world: base::WORLD,
                epoch: server.binding().epoch,
                session: SessionId(1),
                seq: RequestSeq(1),
            },
            actor: ActorContextId(1),
            input: Call {
                action: base::key("planning:unknown"),
                version: 1,
                target: torse_sim::action::ActionTarget::Block(BlockPos::new(0, 0, 0)),
                parameters: ComponentFields::new(),
            },
        };
        let request_id = request.id;
        assert!(matches!(server.submit(request), Admission::Queued { .. }));
        server.drive_tick(&mut NoReplicas, 256).unwrap();
        assert_eq!(
            server.authority().unwrap().runtime().events.len(),
            1,
            "only the first competing plan schedules an event"
        );
        server.drive_tick(&mut NoReplicas, 256).unwrap();
        assert_eq!(probe.events.load(Ordering::SeqCst), 1);
        assert_eq!(probe.calls.load(Ordering::SeqCst), 8);
        assert_eq!(probe.active.load(Ordering::SeqCst), 0);
        assert!(probe.peak.load(Ordering::SeqCst) <= if workers == 4 { 2 } else { 1 });
        if workers == 4 {
            assert_eq!(probe.peak.load(Ordering::SeqCst), 2);
            let completed = probe.completed.lock().unwrap();
            assert_eq!(&completed[..4], &[1, 0, 3, 2]);
        }
        let mut replica = vec![];
        wire::encode(
            &server
                .take_replica(subscription, base::budget())
                .unwrap()
                .into_iter()
                .map(|b| b.changes)
                .collect::<Vec<_>>(),
            &mut replica,
            base::budget(),
            base::codec_limits(),
        )
        .unwrap();
        let receipt = server.receipt(request_id).unwrap().unwrap().clone();
        let state = bytes(server.authority().unwrap());
        drop(server);
        let journal = std::fs::read(dir.0.join("journal.log")).unwrap();
        let result = (state.clone(), journal, replica, receipt);
        if let Some(expected) = &reference {
            assert_eq!(&result, expected);
        } else {
            reference = Some(result);
        }
        probe.deny.store(true, Ordering::SeqCst);
        let recovered = reopen(&dir, rules(&probe, false));
        assert_eq!(state, bytes(recovered.authority().unwrap()));
    }
}

#[test]
fn partial_frozen_phase_recovers_without_running_callbacks() {
    for completed in [0, 1, 5] {
        let dir = Directory::new();
        let probe = Arc::new(Probe::default());
        let mut server = create(&dir, rules(&probe, false));
        server
            .set_parallel_planning(Some(PlanningConfig {
                workers: 4,
                max_in_flight: 2,
            }))
            .unwrap();
        while server.authority().unwrap().runtime().work.is_none()
            || usize::from(server.authority().unwrap().resume_point().next_phase)
                != Phase::Simulation.index()
        {
            server.drive_one(&mut NoReplicas).unwrap();
        }
        for _ in 0..completed {
            server.drive_one(&mut NoReplicas).unwrap();
        }
        let cursor = server.authority().unwrap().cursor();
        assert_eq!(probe.calls.load(Ordering::SeqCst), 4);
        drop(server);
        probe.deny.store(true, Ordering::SeqCst);
        let mut recovered = reopen(&dir, rules(&probe, false));
        recovered
            .set_parallel_planning(Some(PlanningConfig {
                workers: 1,
                max_in_flight: 1,
            }))
            .unwrap();
        assert_eq!(recovered.authority().unwrap().cursor(), cursor);
        assert_eq!(
            recovered
                .authority()
                .unwrap()
                .runtime()
                .work
                .as_ref()
                .unwrap()
                .next,
            completed
        );
        recovered.drive_tick(&mut NoReplicas, 128).unwrap();
        assert_eq!(probe.calls.load(Ordering::SeqCst), 4);
        assert_eq!(probe.events.load(Ordering::SeqCst), 0);
        assert_eq!(recovered.authority().unwrap().runtime().events.len(), 1);
        assert_eq!(
            recovered
                .authority()
                .unwrap()
                .world()
                .store()
                .query(BlockPos::new(8, 1, 1))
                .ready()
                .unwrap(),
            recovered
                .authority()
                .unwrap()
                .world()
                .registry()
                .default_state(&base::key("base:stone"))
                .unwrap()
        );
    }
}

struct TraceRule {
    spec: SystemSpec,
    trace: Arc<Mutex<Vec<(String, bool)>>>,
}
impl DurableRule for TraceRule {
    fn spec(&self) -> SystemSpec {
        self.spec.clone()
    }
    fn parallel_planner(&self) -> Option<&(dyn DurableRule + Sync)> {
        Some(self)
    }
    fn plan(&self, _: &WorldAuthority, _: Tick) -> Result<Vec<WorkPlan>, Fault> {
        self.trace
            .lock()
            .unwrap()
            .push((self.spec.id.to_string(), true));
        std::thread::sleep(Duration::from_millis(1));
        self.trace
            .lock()
            .unwrap()
            .push((self.spec.id.to_string(), false));
        Ok(vec![])
    }
}
struct Legacy {
    spec: SystemSpec,
    calls: Rc<Cell<usize>>,
    trace: Arc<Mutex<Vec<(String, bool)>>>,
    owner: std::thread::ThreadId,
}
impl DurableRule for Legacy {
    fn spec(&self) -> SystemSpec {
        self.spec.clone()
    }
    fn plan(&self, _: &WorldAuthority, _: Tick) -> Result<Vec<WorkPlan>, Fault> {
        assert_eq!(std::thread::current().id(), self.owner);
        let trace = self.trace.lock().unwrap();
        assert_eq!(
            trace.iter().filter(|(_, start)| *start).count(),
            trace.iter().filter(|(_, start)| !start).count()
        );
        self.calls.set(self.calls.get() + 1);
        Ok(vec![])
    }
}
#[test]
fn dependency_edges_and_non_sync_rules_are_join_boundaries() {
    let trace = Arc::new(Mutex::new(vec![]));
    let calls = Rc::new(Cell::new(0));
    let specs: Vec<_> = (0..5)
        .map(|i| SystemSpec {
            id: base::key(&format!("trace:r{i}")),
            phase: Phase::Simulation,
            rule_priority: 0,
            before: if i == 0 {
                vec![base::key("trace:r1")]
            } else {
                vec![]
            },
            after: if i == 2 {
                vec![base::key("trace:r1")]
            } else {
                vec![]
            },
        })
        .collect();
    let rules = specs
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            if i == 3 {
                Box::new(Legacy {
                    spec: spec.clone(),
                    calls: Rc::clone(&calls),
                    trace: Arc::clone(&trace),
                    owner: std::thread::current().id(),
                }) as Box<dyn DurableRule>
            } else {
                Box::new(TraceRule {
                    spec: spec.clone(),
                    trace: Arc::clone(&trace),
                }) as Box<dyn DurableRule>
            }
        })
        .collect();
    let (world, actions) = base::world().unwrap();
    let mut host = ExecutionHost::new(
        binding(&world),
        world,
        actions,
        rules,
        base::execution_limits(),
    )
    .unwrap();
    host.set_parallel_planning(Some(PlanningConfig {
        workers: 4,
        max_in_flight: 4,
    }))
    .unwrap();
    until_simulation(&mut host);
    host.prepare_next().unwrap();
    assert_eq!(calls.get(), 1);
    let trace = trace.lock().unwrap();
    for (first, second) in [(0, 1), (1, 2)] {
        let first = trace
            .iter()
            .position(|(id, start)| id == &format!("trace:r{first}") && !start)
            .unwrap();
        let second = trace
            .iter()
            .position(|(id, start)| id == &format!("trace:r{second}") && *start)
            .unwrap();
        assert!(first < second);
    }
}

struct FailureRule {
    index: usize,
    mode: Arc<AtomicUsize>,
    finished: Arc<AtomicUsize>,
}
impl DurableRule for FailureRule {
    fn spec(&self) -> SystemSpec {
        SystemSpec {
            id: base::key(&format!("failure:r{}", self.index)),
            phase: Phase::Simulation,
            rule_priority: 0,
            before: vec![],
            after: vec![],
        }
    }
    fn parallel_planner(&self) -> Option<&(dyn DurableRule + Sync)> {
        Some(self)
    }
    fn plan(&self, _: &WorldAuthority, _: Tick) -> Result<Vec<WorkPlan>, Fault> {
        self.finished.fetch_add(1, Ordering::SeqCst);
        match self.mode.load(Ordering::SeqCst) {
            1 => Err(Fault::invalid_data(format!("failure {}", self.index))),
            2 if self.index == 0 => panic!("injected native planning panic"),
            3 => Ok((0..129)
                .map(|_| WorkPlan {
                    transaction: None,
                    schedule: vec![],
                    cancel: vec![],
                })
                .collect()),
            4 => Ok(vec![WorkPlan {
                transaction: None,
                schedule: vec![EventDraft {
                    tick: Tick(9),
                    rule: self.spec().id,
                    payload: [(
                        base::key("failure:large"),
                        torse_world::ComponentValue::Text("x".repeat(262_145)),
                    )]
                    .into(),
                }],
                cancel: vec![],
            }]),
            _ => Ok(vec![]),
        }
    }
}
#[test]
fn failure_panic_and_output_limits_leave_phase_retryable_and_join_all_workers() {
    for count in [1, 2] {
        for failure in 1..=4 {
            let mode = Arc::new(AtomicUsize::new(failure));
            let finished = Arc::new(AtomicUsize::new(0));
            let rules = (0..count)
                .map(|index| {
                    Box::new(FailureRule {
                        index,
                        mode: Arc::clone(&mode),
                        finished: Arc::clone(&finished),
                    }) as Box<dyn DurableRule>
                })
                .collect();
            let (world, actions) = base::world().unwrap();
            let mut host = ExecutionHost::new(
                binding(&world),
                world,
                actions,
                rules,
                base::execution_limits(),
            )
            .unwrap();
            host.set_parallel_planning(Some(PlanningConfig {
                workers: 4,
                max_in_flight: 2,
            }))
            .unwrap();
            until_simulation(&mut host);
            let before = bytes(&host);
            let error = host.prepare_next().err().expect("planning must fail");
            if failure == 1 {
                assert_eq!(error.context, "failure 0");
            }
            if failure >= 3 {
                assert_eq!(error.code, FaultCode::ResourceLimit);
            }
            assert_eq!(finished.load(Ordering::SeqCst), count);
            assert_eq!(before, bytes(&host));
            mode.store(0, Ordering::SeqCst);
            let prepared = host.prepare_next().unwrap().unwrap();
            assert_eq!(finished.load(Ordering::SeqCst), count * 2);
            host.publish(prepared).unwrap();
            assert_eq!(host.runtime().work.as_ref().unwrap().tasks.len(), 0);
        }
    }
}

#[test]
fn journal_block_reuses_collected_parallel_plan_and_config_validation_is_atomic() {
    let probe = Arc::new(Probe::default());
    let (world, actions) = base::world().unwrap();
    let mut host = ExecutionHost::new(
        binding(&world),
        world,
        actions,
        rules(&probe, false),
        base::execution_limits(),
    )
    .unwrap();
    host.set_parallel_planning(Some(PlanningConfig {
        workers: 4,
        max_in_flight: 2,
    }))
    .unwrap();
    for (workers, max_in_flight) in [(0, 2), (65, 2), (2, 0), (2, 65)] {
        assert!(
            host.set_parallel_planning(Some(PlanningConfig {
                workers,
                max_in_flight
            }))
            .is_err()
        );
    }
    until_simulation(&mut host);
    let first = host.prepare_next().unwrap().unwrap();
    let codec = base::Codec::new(base::codec_limits()).unwrap();
    let mut record = vec![];
    codec
        .encode_commit(first.record(), &mut record, base::budget())
        .unwrap();
    drop(first); // A refused journal reservation never calls publish.
    let before = bytes(&host);
    host.set_parallel_planning(None).unwrap();
    let retry = host.prepare_next().unwrap().unwrap();
    let mut retried = vec![];
    codec
        .encode_commit(retry.record(), &mut retried, base::budget())
        .unwrap();
    assert_eq!(retried, record);
    assert_eq!(probe.calls.load(Ordering::SeqCst), 4);
    assert_eq!(before, bytes(&host));
    host.publish(retry).unwrap();
    assert_eq!(host.runtime().work.as_ref().unwrap().tasks.len(), 8);
    assert_eq!(
        host.world()
            .store()
            .query(BlockPos::new(8, 1, 1))
            .ready()
            .unwrap(),
        BlockStateId::EMPTY
    );
}
