//! Event-local tickets, isolated page I/O, deterministic generation, and recovery.
mod support;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
pub(crate) use support::Directory;
use torse_server::{bootstrap as base, execution::*, residency::*, section_pages::SectionPages, *};
use torse_sim::durable::*;
use torse_sim::{OrderingKey, Phase, SystemSpec, Transaction, WorldAuthority};
use torse_types::*;
use torse_world::*;

pub(crate) fn pages(dir: &Directory) -> SectionPages {
    let root = dir.0.join("sections");
    std::fs::create_dir_all(&root).unwrap();
    SectionPages::new(root, base::budget(), base::codec_limits(), 64).unwrap()
}
pub(crate) fn create(dir: &Directory, rules: Vec<Box<dyn DurableRule>>) -> base::Files {
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
pub(crate) fn open(
    dir: &Directory,
    pages: SectionPages,
    rules: Vec<Box<dyn DurableRule>>,
) -> base::Files {
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
            )
            .with_section_pages(pages))
        },
    )
    .unwrap()
}
fn config() -> ResidencyConfig {
    ResidencyConfig {
        max_resident: 4,
        max_io_bytes: 32 * 1024 * 1024,
    }
}
fn settle(server: &mut base::Files) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match server.poll_residency(&mut NoReplicas).unwrap() {
            ResidencyProgress::Idle => return,
            ResidencyProgress::Pending | ResidencyProgress::Published(_) => {}
            other => panic!("unexpected residency result: {other:?}"),
        }
        assert!(Instant::now() < deadline, "residency did not settle");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn global_io_pressure_preserves_interest_and_releases_completed_page_buffers() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let pool = MemoryPool::new(MemoryBudget::new(128 * 1024 * 1024));
    let mut server = create(&dir, Vec::new());
    server
        .attach_memory(pool.clone(), 48 * 1024 * 1024)
        .unwrap();
    let fresh = SectionPos::new(2, 0, 0);
    server
        .enable_residency(
            config(),
            pages,
            Some(Arc::new(EmptyGenerator {
                identity: identity(),
                gate: None,
            })),
            &[SectionPos::ZERO, SectionPos::new(1, 0, 0), fresh],
        )
        .unwrap();
    let held = pool
        .reserve(
            MemoryClass::Resource,
            128 * 1024 * 1024 - pool.usage().total,
            EvictionRank::Pinned,
        )
        .unwrap();
    assert!(matches!(
        server.poll_residency(&mut NoReplicas).unwrap(),
        ResidencyProgress::BudgetBlocked
    ));
    assert!(
        server
            .authority()
            .unwrap()
            .world()
            .store()
            .section(fresh)
            .is_none()
    );
    drop(held);
    settle(&mut server);
    assert!(
        server
            .authority()
            .unwrap()
            .world()
            .store()
            .section(fresh)
            .is_some()
    );
    assert_eq!(
        pool.usage().current[MemoryClass::Codec.index()],
        48 * 1024 * 1024
    );
    assert!(pool.usage().peak[MemoryClass::Codec.index()] > 48 * 1024 * 1024);
    drop(server);
    assert_eq!(pool.usage().total, 0);
}

struct Rule {
    dependencies: Vec<EventDependency>,
    move_entity: bool,
    event_tick: u64,
}
impl DurableRule for Rule {
    fn spec(&self) -> SystemSpec {
        SystemSpec {
            id: base::key("test:event-residency"),
            phase: Phase::Simulation,
            rule_priority: 0,
            before: Vec::new(),
            after: Vec::new(),
        }
    }
    fn plan(&self, world: &WorldAuthority, tick: Tick) -> Result<Vec<WorkPlan>, Fault> {
        if tick != Tick(0) {
            return Ok(Vec::new());
        }
        let transaction = if self.move_entity {
            let handle = world.entities().handle_of(EntityId(1)).unwrap();
            let entity = world.entities().get(handle).unwrap();
            let ordering = OrderingKey {
                phase: Phase::Simulation,
                rule_priority: 0,
                stable_task_key: 1,
                local_sequence: 0,
            };
            let mut transaction =
                Transaction::new(TransactionId(1), base::key("test:move"), ordering);
            transaction.move_entity(
                handle,
                entity.id,
                entity.position,
                [16.0, 0.0, 0.0],
                [0.5; 3],
            );
            Some(
                torse_sim::TransactionGroup::new(TransactionId(1), ordering, vec![transaction])
                    .map_err(|_| Fault::invalid_data("test movement group"))?,
            )
        } else {
            None
        };
        Ok(vec![WorkPlan {
            transaction,
            schedule: vec![
                EventDraft {
                    tick: Tick(self.event_tick),
                    rule: self.spec().id,
                    payload: Default::default(),
                }
                .with_dependencies(self.dependencies.clone())?,
            ],
            cancel: Vec::new(),
        }])
    }
    fn event(&self, _: &WorldAuthority, _: Tick, _: &TimedEvent) -> Result<WorkPlan, Fault> {
        Ok(WorkPlan {
            transaction: None,
            schedule: Vec::new(),
            cancel: Vec::new(),
        })
    }
}
fn rule(dependencies: Vec<EventDependency>) -> Box<dyn DurableRule> {
    Box::new(Rule {
        dependencies,
        move_entity: false,
        event_tick: 10,
    })
}

#[test]
fn future_events_pin_only_declared_sections_and_replay_uses_the_same_rule() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let dependencies = vec![EventDependency::Section(SectionPos::ZERO)];
    let mut server = create(&dir, vec![rule(dependencies.clone())]);
    server.drive_tick(&mut NoReplicas, 1024).unwrap();
    let events = server.authority().unwrap().runtime().events.clone();
    assert!(
        server
            .unload_section(&pages, SectionPos::ZERO, &mut NoReplicas)
            .is_err()
    );
    assert!(matches!(
        server
            .unload_section(&pages, SectionPos::new(1, 0, 0), &mut NoReplicas)
            .unwrap(),
        CommitStep::Durable(_)
    ));
    drop(server);
    let mut server = open(&dir, pages.clone(), vec![rule(dependencies)]);
    assert_eq!(server.authority().unwrap().runtime().events, events);
    server.checkpoint().unwrap();
    drop(server);
    let server = open(
        &dir,
        pages,
        vec![rule(vec![EventDependency::Section(SectionPos::ZERO)])],
    );
    assert!(
        server
            .authority()
            .unwrap()
            .section_is_event_pinned(SectionPos::ZERO)
            .unwrap()
    );
}

#[test]
fn entity_tickets_follow_committed_movement_and_cross_section_scopes_hold_both_sides() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(
        &dir,
        vec![Box::new(Rule {
            dependencies: vec![EventDependency::Entity(EntityId(1))],
            move_entity: true,
            event_tick: 10,
        })],
    );
    server.drive_tick(&mut NoReplicas, 1024).unwrap();
    assert!(
        !server
            .authority()
            .unwrap()
            .section_is_event_pinned(SectionPos::ZERO)
            .unwrap()
    );
    assert!(
        server
            .authority()
            .unwrap()
            .section_is_event_pinned(SectionPos::new(1, 0, 0))
            .unwrap()
    );
    assert!(matches!(
        server
            .unload_section(&pages, SectionPos::ZERO, &mut NoReplicas)
            .unwrap(),
        CommitStep::Durable(_)
    ));
    assert!(
        server
            .unload_section(&pages, SectionPos::new(1, 0, 0), &mut NoReplicas)
            .is_err()
    );
    drop(server);
    let dir = Directory::new();
    let pages = SectionPages::new(
        {
            let p = dir.0.join("sections");
            std::fs::create_dir(&p).unwrap();
            p
        },
        base::budget(),
        base::codec_limits(),
        64,
    )
    .unwrap();
    let mut server = create(
        &dir,
        vec![rule(vec![
            EventDependency::Section(SectionPos::ZERO),
            EventDependency::Section(SectionPos::new(1, 0, 0)),
        ])],
    );
    server.drive_tick(&mut NoReplicas, 1024).unwrap();
    for pos in [SectionPos::ZERO, SectionPos::new(1, 0, 0)] {
        assert!(server.unload_section(&pages, pos, &mut NoReplicas).is_err());
    }
}

#[test]
fn automatic_load_unload_budget_backpressure_and_page_io_retry_preserve_authority() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(&dir, Vec::new());
    let position = SectionPos::new(1, 0, 0);
    server
        .enable_residency(
            ResidencyConfig {
                max_io_bytes: 1,
                ..config()
            },
            pages.clone(),
            None,
            &[SectionPos::ZERO],
        )
        .unwrap();
    let before = server.authority().unwrap().cursor();
    assert!(matches!(
        server.poll_residency(&mut NoReplicas).unwrap(),
        ResidencyProgress::BudgetBlocked
    ));
    assert_eq!(server.authority().unwrap().cursor(), before);
    server
        .enable_residency(config(), pages.clone(), None, &[SectionPos::ZERO])
        .unwrap();
    settle(&mut server);
    let digest = server.authority().unwrap().parked_sections()[&position].digest;
    let saved = std::fs::read(pages.path(digest)).unwrap();
    std::fs::write(pages.path(digest), b"broken").unwrap();
    server
        .set_residency_interest(&[SectionPos::ZERO, position])
        .unwrap();
    let before = server.authority().unwrap().cursor();
    assert!(matches!(
        server.poll_residency(&mut NoReplicas).unwrap(),
        ResidencyProgress::Pending
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if server.poll_residency(&mut NoReplicas).is_err() {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(server.authority().unwrap().cursor(), before);
    std::fs::write(pages.path(digest), saved).unwrap();
    settle(&mut server);
    assert!(
        server
            .authority()
            .unwrap()
            .world()
            .store()
            .section(position)
            .is_some()
    );
    server.checkpoint().unwrap();
    drop(server);
    assert!(
        open(&dir, pages, Vec::new())
            .authority()
            .unwrap()
            .parked_sections()
            .is_empty()
    );
}

pub(crate) struct EmptyGenerator {
    pub(crate) identity: GenerationIdentity,
    pub(crate) gate: Option<Arc<(Mutex<bool>, Condvar)>>,
}
struct GateRelease(Arc<(Mutex<bool>, Condvar)>);
impl Drop for GateRelease {
    fn drop(&mut self) {
        let (lock, condition) = &*self.0;
        *lock.lock().unwrap() = true;
        condition.notify_all();
    }
}
impl GenerationSource for EmptyGenerator {
    fn identity(&self) -> GenerationIdentity {
        self.identity.clone()
    }
    fn generate(&self, context: GenerationContext, pos: SectionPos) -> Result<Snapshot, Fault> {
        if let Some(gate) = &self.gate {
            let (lock, condition) = &**gate;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = condition.wait(open).unwrap();
            }
        }
        Ok(Snapshot {
            meta: context.meta,
            side: context.side,
            bounds: context.bounds,
            states: Vec::new(),
            sections: vec![SnapshotSection {
                pos,
                cells: SnapshotCells::Uniform(BlockStateId::EMPTY),
            }],
            entities: Vec::new(),
            next_entity_id: context.next_entity_id,
        })
    }
}
pub(crate) fn identity() -> GenerationIdentity {
    GenerationIdentity {
        id: base::key("test:empty-generator"),
        version: 1,
        configuration: Digest([7; 32]),
    }
}

fn settle_batch(server: &mut base::Files) -> Vec<ResidencyBatchReport> {
    let mut reports = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match server.poll_residency(&mut NoReplicas).unwrap() {
            ResidencyProgress::Idle => return reports,
            ResidencyProgress::Pending | ResidencyProgress::Published(_) => {}
            ResidencyProgress::Batch(report) => {
                assert!(report.failure.is_none(), "{report:?}");
                assert_eq!(
                    report.durability.through.0,
                    server.authority().unwrap().cursor().seq.0
                );
                reports.push(report);
            }
            other => panic!("unexpected result {other:?}"),
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn batch_rechecks_interest_and_external_cursor_before_publication() {
    for changed_interest in [true, false] {
        let dir = Directory::new();
        let pages = pages(&dir);
        let mut server = create(&dir, Vec::new());
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let _release = GateRelease(gate.clone());
        let desired: Vec<_> = (0..4).map(|x| SectionPos::new(x, 0, 0)).collect();
        server
            .enable_residency(
                ResidencyConfig {
                    max_resident: 4,
                    max_io_bytes: 64 * 1024 * 1024,
                },
                pages,
                Some(Arc::new(EmptyGenerator {
                    identity: identity(),
                    gate: Some(gate.clone()),
                })),
                &desired,
            )
            .unwrap();
        assert!(server.set_residency_batch_pages(0).is_err());
        assert!(server.set_residency_batch_pages(1025).is_err());
        server.set_residency_batch_pages(2).unwrap();
        assert!(matches!(
            server.poll_residency(&mut NoReplicas).unwrap(),
            ResidencyProgress::Pending
        ));
        assert!(server.set_residency_batch_pages(1).is_err());
        if changed_interest {
            server.set_residency_interest(&desired[..2]).unwrap();
        } else {
            server.drive_tick(&mut NoReplicas, 1024).unwrap();
        }
        let cursor = server.authority().unwrap().cursor();
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let ResidencyProgress::Batch(report) =
                server.poll_residency(&mut NoReplicas).unwrap()
            {
                assert!(report.committed.is_empty());
                assert_eq!(report.failure.is_some(), !changed_interest);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(server.authority().unwrap().cursor(), cursor);
        assert_eq!(server.authority().unwrap().world().store().len(), 2);
        settle_batch(&mut server);
        assert_eq!(
            server.authority().unwrap().world().store().len(),
            if changed_interest { 2 } else { 4 }
        );
    }
}

#[test]
fn batch_byte_budget_shortens_dispatch_without_growing_business_limits() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(&dir, Vec::new());
    let desired: Vec<_> = (0..6).map(|x| SectionPos::new(x, 0, 0)).collect();
    server
        .enable_residency(
            ResidencyConfig {
                max_resident: 6,
                max_io_bytes: 32 * 1024 * 1024,
            },
            pages,
            Some(Arc::new(EmptyGenerator {
                identity: identity(),
                gate: None,
            })),
            &desired,
        )
        .unwrap();
    server.set_residency_batch_pages(24).unwrap();
    let before = server.flush_metrics().calls;
    let reports = settle_batch(&mut server);
    assert_eq!(reports.len(), 4);
    assert!(reports.iter().all(|report| report.committed.len() == 1));
    assert_eq!(server.flush_metrics().calls - before, 4);
}

#[test]
fn batched_24_page_generation_and_reload_match_serial_state_log_and_recovery() {
    let run = |batch_pages| {
        let dir = Directory::new();
        let pages = pages(&dir);
        let mut server = create(&dir, Vec::new());
        let desired: Vec<_> = (0..26).map(|x| SectionPos::new(x, 0, 0)).collect();
        let source = Arc::new(EmptyGenerator {
            identity: identity(),
            gate: None,
        });
        let config = ResidencyConfig {
            max_resident: 26,
            max_io_bytes: 24 * 32 * 1024 * 1024,
        };
        server
            .enable_residency(config, pages.clone(), Some(source.clone()), &desired)
            .unwrap();
        server.set_residency_batch_pages(batch_pages).unwrap();
        let before = server.flush_metrics().calls;
        let generated = settle_batch(&mut server);
        let generate_calls = server.flush_metrics().calls - before;
        if batch_pages > 1 {
            assert_eq!(
                generated.iter().map(|r| r.committed.len()).sum::<usize>(),
                24
            );
            assert!(generated.iter().any(|r| r.blocked.is_some()));
        }
        for pos in desired.iter().skip(2) {
            assert!(matches!(
                server
                    .unload_section(&pages, *pos, &mut NoReplicas)
                    .unwrap(),
                CommitStep::Durable(_)
            ));
        }
        server
            .enable_residency(config, pages.clone(), Some(source), &desired)
            .unwrap();
        server.set_residency_batch_pages(batch_pages).unwrap();
        let before = server.flush_metrics().calls;
        settle_batch(&mut server);
        let reload_calls = server.flush_metrics().calls - before;
        let authority = server.authority().unwrap();
        let state = authority
            .world()
            .capture_snapshot(SnapshotMeta {
                cursor: authority.cursor(),
                compatibility: server.binding().compatibility,
                resume_at: authority.resume_point(),
            })
            .unwrap();
        assert_eq!(state.sections.len(), 26);
        drop(server);
        let log = std::fs::read(dir.0.join("journal.log")).unwrap();
        let recovered = open(&dir, pages, Vec::new());
        let authority = recovered.authority().unwrap();
        assert_eq!(
            authority.world().capture_snapshot(state.meta).unwrap(),
            state
        );
        drop(recovered);
        (state, log, generate_calls, reload_calls)
    };
    let serial = run(1);
    let batch = run(24);
    assert_eq!(serial.0, batch.0);
    assert_eq!(serial.1, batch.1);
    assert_eq!((serial.2, serial.3), (24, 24));
    assert_eq!((batch.2, batch.3), (6, 6));
}

#[test]
fn batch_page_integrity_failure_reports_the_durable_prefix_and_retries() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(&dir, Vec::new());
    let desired = [SectionPos::ZERO, SectionPos::new(1, 0, 0)];
    for pos in desired {
        server.unload_section(&pages, pos, &mut NoReplicas).unwrap();
    }
    let bad = server.authority().unwrap().parked_sections()[&desired[1]].digest;
    let path = pages.path(bad);
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"corrupt").unwrap();
    server
        .enable_residency(
            ResidencyConfig {
                max_resident: 4,
                max_io_bytes: 64 * 1024 * 1024,
            },
            pages.clone(),
            None,
            &desired,
        )
        .unwrap();
    server.set_residency_batch_pages(2).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let ResidencyProgress::Batch(report) = server.poll_residency(&mut NoReplicas).unwrap() {
            assert_eq!(report.committed, vec![desired[0]]);
            assert_eq!(report.failure.unwrap().0, desired[1]);
            assert_eq!(
                report.durability.through.0,
                server.authority().unwrap().cursor().seq.0
            );
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    std::fs::write(path, original).unwrap();
    settle_batch(&mut server);
    assert_eq!(server.authority().unwrap().world().store().len(), 2);
}

#[test]
fn batched_partial_append_and_sync_failure_stop_host_and_recover_valid_prefix() {
    use std::io::Write;
    use torse_storage::{Checkpoints, FileCheckpointBackend, Journal, JournalBackend};
    struct FailingFile {
        file: std::fs::File,
        appends: usize,
        partial: bool,
    }
    impl JournalBackend for FailingFile {
        fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            self.appends += 1;
            if self.partial && self.appends == 2 {
                self.file.write_all(&bytes[..bytes.len() / 2])?;
                return Err(std::io::Error::other("injected partial append"));
            }
            self.file.write_all(bytes)
        }
        fn sync(&mut self) -> std::io::Result<()> {
            self.file.sync_all()?;
            Err(std::io::Error::other(
                "injected failed sync acknowledgement",
            ))
        }
    }
    for partial in [true, false] {
        let dir = Directory::new();
        let pages = pages(&dir);
        let directory = dir.acquire();
        std::fs::create_dir(dir.0.join("checkpoints")).unwrap();
        let (world, actions) = base::world().unwrap();
        let binding = Binding {
            world: base::WORLD,
            epoch: directory.epoch(),
            compatibility: world.coordinator().compatibility(),
        };
        let config = base::file_policy();
        let codec = base::Codec::new(base::codec_limits()).unwrap();
        let backend = FailingFile {
            file: std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(dir.0.join("journal.log"))
                .unwrap(),
            appends: 0,
            partial,
        };
        let journal = Journal::with_backend(
            binding,
            CommitCursor::initial(base::WORLD),
            codec,
            config.journal,
            backend,
        )
        .unwrap();
        let checkpoints = Checkpoints::open(
            binding,
            codec,
            config.checkpoint,
            FileCheckpointBackend::open(dir.0.join("checkpoints")).unwrap(),
        )
        .unwrap();
        let host = ExecutionHost::new(
            binding,
            world,
            actions,
            Vec::new(),
            base::execution_limits(),
        )
        .unwrap();
        let mut server =
            Server::initialize(directory, host, journal, checkpoints, config.server).unwrap();
        let desired: Vec<_> = (0..6).map(|x| SectionPos::new(x, 0, 0)).collect();
        server
            .enable_residency(
                ResidencyConfig {
                    max_resident: 6,
                    max_io_bytes: 128 * 1024 * 1024,
                },
                pages.clone(),
                Some(Arc::new(EmptyGenerator {
                    identity: identity(),
                    gate: None,
                })),
                &desired,
            )
            .unwrap();
        server.set_residency_batch_pages(4).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match server.poll_residency(&mut NoReplicas) {
                Err(_) => break,
                Ok(ResidencyProgress::Pending) => {}
                other => panic!("expected injected failure: {other:?}"),
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(server.state(), ServerState::Faulted);
        assert_eq!(server.durability().through.0, 0);
        assert!(server.authority().is_err());
        drop(server);
        let recovered = open(&dir, pages, Vec::new());
        let expected = if partial { 1 } else { 4 };
        assert_eq!(recovered.authority().unwrap().cursor().seq.0, expected);
        assert_eq!(
            recovered.authority().unwrap().world().store().len(),
            2 + expected as usize
        );
    }
}

#[test]
fn due_dependencies_pause_before_the_tick_and_automatic_io_resumes_with_queued_input() {
    use torse_sim::action::{ActionTarget, ActorContext};
    let dir = Directory::new();
    let pages = pages(&dir);
    let target = SectionPos::new(2, 0, 0);
    let mut server = create(
        &dir,
        vec![Box::new(Rule {
            dependencies: vec![EventDependency::Section(target)],
            move_entity: false,
            event_tick: 1,
        })],
    );
    server.drive_tick(&mut NoReplicas, 1024).unwrap();
    let before = server.authority().unwrap().cursor();
    assert!(
        server
            .authority()
            .unwrap()
            .waiting_for_event_sections()
            .unwrap()
    );
    assert!(matches!(
        server.drive_one(&mut NoReplicas).unwrap(),
        CommitStep::Idle
    ));
    assert_eq!(server.authority().unwrap().cursor(), before);
    server
        .register_session(
            Viewer {
                session: SessionId(1),
                actor: ActorContextId(1),
            },
            ActorContext::new(base::key("test:actor")),
            base::scope(),
        )
        .unwrap();
    assert!(matches!(
        server.submit(WorldRequest {
            id: RequestId {
                world: server.binding().world,
                epoch: server.binding().epoch,
                session: SessionId(1),
                seq: RequestSeq(1)
            },
            actor: ActorContextId(1),
            input: Call {
                action: base::key("test:unknown"),
                version: 1,
                target: ActionTarget::Block(BlockPos::new(0, 0, 0)),
                parameters: Default::default()
            },
        }),
        Admission::Queued { .. }
    ));
    let source = Arc::new(EmptyGenerator {
        identity: identity(),
        gate: None,
    });
    server
        .enable_residency(
            config(),
            pages,
            Some(source),
            &[SectionPos::ZERO, SectionPos::new(1, 0, 0)],
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.authority().unwrap().resume_point().tick == Tick(1) {
        server.drive_tick(&mut NoReplicas, 1024).unwrap();
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(server.authority().unwrap().runtime().events.is_empty());
    assert!(
        server
            .authority()
            .unwrap()
            .world()
            .store()
            .section(target)
            .is_some()
    );
}

#[test]
fn generator_descriptor_damage_and_overfull_interests_fail_without_rewriting_data() {
    let mut encoded = Vec::new();
    assert!(
        torse_codec::wire::encode(
            &GenerationIdentity {
                version: 0,
                ..identity()
            },
            &mut encoded,
            base::budget(),
            base::codec_limits()
        )
        .is_err()
    );
    assert!(encoded.is_empty());
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(&dir, Vec::new());
    let before = server.authority().unwrap().cursor();
    assert!(
        server
            .enable_residency(config(), pages.clone(), None, &[SectionPos::ZERO; 5])
            .is_err()
    );
    assert_eq!(server.authority().unwrap().cursor(), before);
    let source = Arc::new(EmptyGenerator {
        identity: identity(),
        gate: None,
    });
    server
        .enable_residency(
            config(),
            pages,
            Some(source),
            &[SectionPos::ZERO, SectionPos::new(1, 0, 0)],
        )
        .unwrap();
    drop(server);
    let path = dir.0.join("generation.bin");
    let bytes = std::fs::read(&path).unwrap();
    for bad in [bytes[..bytes.len() - 1].to_vec(), {
        let mut damaged = bytes.clone();
        damaged[15] ^= 1;
        damaged
    }] {
        std::fs::write(&path, &bad).unwrap();
        let lease = WorldLease::inspect(&dir.0, base::WORLD, 8192).unwrap();
        assert!(GenerationIdentity::inspect(lease.root()).is_err());
        let (world, _) = base::world().unwrap();
        assert!(
            base::Files::preflight_file(
                &lease,
                world.coordinator().compatibility(),
                base::Codec::new(base::codec_limits()).unwrap(),
                base::file_policy()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), bad);
    }
}

#[test]
fn blocked_generation_does_not_block_poll_or_allow_page_collection_and_stale_results_are_discarded()
{
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(&dir, Vec::new());
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let source = Arc::new(EmptyGenerator {
        identity: identity(),
        gate: Some(gate.clone()),
    });
    let fresh = SectionPos::new(2, 0, 0);
    server
        .enable_residency(
            config(),
            pages.clone(),
            Some(source),
            &[SectionPos::ZERO, SectionPos::new(1, 0, 0), fresh],
        )
        .unwrap();
    let _release_on_failure = GateRelease(gate.clone());
    let before = server.authority().unwrap().cursor();
    assert!(matches!(
        server.poll_residency(&mut NoReplicas).unwrap(),
        ResidencyProgress::Pending
    ));
    for _ in 0..16 {
        assert!(matches!(
            server.poll_residency(&mut NoReplicas).unwrap(),
            ResidencyProgress::Pending
        ));
    }
    assert!(server.collect_section_pages(&pages, 1).is_err());
    server
        .set_residency_interest(&[SectionPos::ZERO, SectionPos::new(1, 0, 0)])
        .unwrap();
    {
        let (lock, condition) = &*gate;
        *lock.lock().unwrap() = true;
        condition.notify_all();
    }
    settle(&mut server);
    assert_eq!(server.authority().unwrap().cursor(), before);
    assert!(
        server
            .authority()
            .unwrap()
            .world()
            .store()
            .section(fresh)
            .is_none()
    );
    server
        .set_residency_interest(&[SectionPos::ZERO, SectionPos::new(1, 0, 0), fresh])
        .unwrap();
    settle(&mut server);
    assert!(
        server
            .authority()
            .unwrap()
            .world()
            .store()
            .section(fresh)
            .is_some()
    );
    drop(server);
    let lease = WorldLease::inspect(&dir.0, base::WORLD, 8192).unwrap();
    assert!(identity().verify(lease.root()).unwrap());
    let original = std::fs::read(dir.0.join("generation.bin")).unwrap();
    for changed in [
        GenerationIdentity {
            version: 2,
            ..identity()
        },
        GenerationIdentity {
            configuration: Digest([8; 32]),
            ..identity()
        },
        GenerationIdentity {
            id: base::key("test:other"),
            ..identity()
        },
    ] {
        assert!(changed.verify(lease.root()).is_err());
        assert_eq!(
            std::fs::read(dir.0.join("generation.bin")).unwrap(),
            original
        );
    }
    drop(lease);
    let mut server = open(&dir, pages.clone(), Vec::new());
    let source = Arc::new(EmptyGenerator {
        identity: identity(),
        gate: None,
    });
    server
        .enable_residency(config(), pages, Some(source), &[fresh])
        .unwrap();
    settle(&mut server);
    assert!(
        server
            .authority()
            .unwrap()
            .world()
            .store()
            .section(fresh)
            .is_some()
    );
}
