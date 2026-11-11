//! Atomic generated installations: real file barriers and complete journal recovery.
mod support;
use std::{
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use support::Directory;
use torse_server::{bootstrap as base, execution::*, section_pages::SectionPages, *};
use torse_sim::WorldAuthority;
use torse_storage::{Checkpoints, FileCheckpointBackend, Journal, JournalBackend};
use torse_types::*;
use torse_world::*;

#[derive(Default)]
struct Counts {
    appends: AtomicUsize,
    syncs: AtomicUsize,
    fail: AtomicUsize,
}
struct ObservedFile {
    file: std::fs::File,
    counts: Arc<Counts>,
}
impl JournalBackend for ObservedFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.counts.appends.fetch_add(1, Ordering::SeqCst);
        if self.counts.fail.load(Ordering::SeqCst) == 3 {
            self.file.write_all(&bytes[..bytes.len() / 2])?;
            self.file.sync_all()?;
            std::process::exit(74);
        }
        if self.counts.fail.load(Ordering::SeqCst) == 1 {
            self.file.write_all(&bytes[..bytes.len() / 2])?;
            return Err(std::io::Error::other("partial atomic record"));
        }
        self.file.write_all(bytes)
    }
    fn sync(&mut self) -> std::io::Result<()> {
        self.counts.syncs.fetch_add(1, Ordering::SeqCst);
        self.file.sync_all()?;
        if self.counts.fail.load(Ordering::SeqCst) == 4 {
            std::process::exit(75);
        }
        if self.counts.fail.load(Ordering::SeqCst) == 2 {
            return Err(std::io::Error::other("lost sync acknowledgement"));
        }
        Ok(())
    }
}
type Files = Server<
    ExecutionHost,
    Journal<ExecutionModel, base::Codec, ObservedFile>,
    Checkpoints<ExecutionModel, base::Codec, FileCheckpointBackend>,
>;
fn setup(
    dir: &Directory,
    runtime: OutputBudget,
    journal_bytes: u64,
) -> (Files, SectionPages, Arc<Counts>) {
    let directory = dir.acquire();
    let (model, actions) = base::world().unwrap();
    let world = WorldAuthority::new(
        base::WORLD,
        model.registry().clone(),
        model.schemas().clone(),
        16,
        WorldBounds::new(-64, 320).unwrap(),
        model.coordinator().compatibility(),
    )
    .unwrap();
    let binding = Binding {
        world: base::WORLD,
        epoch: directory.epoch(),
        compatibility: model.coordinator().compatibility(),
    };
    let mut config = base::file_policy();
    config.journal.codec_budget.max_bytes = journal_bytes;
    let codec = base::Codec::new(base::codec_limits()).unwrap();
    let counts = Arc::new(Counts::default());
    let backend = ObservedFile {
        file: std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(dir.0.join("journal.log"))
            .unwrap(),
        counts: counts.clone(),
    };
    let journal = Journal::with_backend(
        binding,
        CommitCursor::initial(base::WORLD),
        codec,
        config.journal,
        backend,
    )
    .unwrap();
    std::fs::create_dir(dir.0.join("checkpoints")).unwrap();
    let checkpoints = Checkpoints::open(
        binding,
        codec,
        config.checkpoint,
        FileCheckpointBackend::open(dir.0.join("checkpoints")).unwrap(),
    )
    .unwrap();
    let mut limits = base::execution_limits();
    limits.runtime = runtime;
    let host = ExecutionHost::new(binding, world, actions, Vec::new(), limits).unwrap();
    let server = Server::initialize(directory, host, journal, checkpoints, config.server).unwrap();
    std::fs::create_dir(dir.0.join("sections")).unwrap();
    let pages = SectionPages::new(
        dir.0.join("sections"),
        base::budget(),
        base::codec_limits(),
        64,
    )
    .unwrap();
    (server, pages, counts)
}
fn column(host: &ExecutionHost, x: i64, entities: bool) -> Vec<Snapshot> {
    let world = host.world();
    let first = world.entities().next_entity_id();
    (-4..20)
        .enumerate()
        .map(|(index, y)| {
            let next = first + if entities { index as u128 } else { 0 };
            Snapshot {
                meta: torse_sim::streaming::page_meta(world),
                side: 16,
                bounds: world.store().bounds(),
                next_entity_id: next + u128::from(entities),
                states: vec![StateEntry {
                    id: BlockStateId(1),
                    block: base::key("base:stone"),
                    values: Vec::new(),
                }],
                sections: vec![SnapshotSection {
                    pos: SectionPos::new(x, y, 0),
                    cells: SnapshotCells::Uniform(BlockStateId(1)),
                }],
                entities: if entities {
                    vec![SnapshotEntity {
                        id: EntityId(next),
                        position_bits: Position::new(
                            x as f64 * 16.0 + 0.5,
                            y as f64 * 16.0 + 0.5,
                            0.5,
                        )
                        .to_bits(),
                        components: Vec::new(),
                    }]
                } else {
                    Vec::new()
                },
            }
        })
        .collect()
}
fn open(dir: &Directory, pages: SectionPages) -> base::Files {
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
                Vec::new(),
                base::execution_limits(),
            )
            .with_section_pages(pages))
        },
    )
    .unwrap()
}
fn image(host: &ExecutionHost) -> Snapshot {
    host.world()
        .capture_snapshot(torse_sim::streaming::page_meta(host.world()))
        .unwrap()
}

#[test]
fn one_record_one_barrier_one_cursor_and_exact_replay_for_24_sections() {
    for entities in [false, true] {
        let dir = Directory::new();
        let (mut server, pages, counts) = setup(&dir, base::execution_limits().runtime, 8_388_608);
        let batch = column(server.authority().unwrap(), 3, entities);
        let before = server.authority().unwrap().cursor();
        let result = server
            .install_section_batch(&pages, batch, &mut NoReplicas)
            .unwrap();
        let CommitStep::Durable(done) = result else {
            panic!("not durable")
        };
        assert_eq!(done.publication.at.seq.0, before.seq.0 + 1);
        assert_eq!(done.durability.through.0, done.publication.at.seq.0);
        assert_eq!(counts.appends.load(Ordering::SeqCst), 1);
        assert_eq!(counts.syncs.load(Ordering::SeqCst), 1);
        let expected = image(server.authority().unwrap());
        assert_eq!(expected.sections.len(), 24);
        assert_eq!(expected.entities.len(), if entities { 24 } else { 0 });
        for y in -4..20 {
            assert!(matches!(
                server
                    .authority()
                    .unwrap()
                    .world()
                    .store()
                    .query(BlockPos::new(48, y * 16, 0)),
                TerrainQuery::Ready(_)
            ));
        }
        assert_eq!(server.section_install_diagnostics().sections, 24);
        assert!(server.section_install_diagnostics().failure.is_none());
        drop(server);
        let reopened = open(&dir, pages);
        let actual = image(reopened.authority().unwrap());
        assert_eq!(actual, expected);
    }
}

#[test]
fn malformed_last_page_duplicate_position_allocator_and_limits_reject_without_prefix() {
    let dir = Directory::new();
    let (mut server, pages, counts) = setup(&dir, base::execution_limits().runtime, 8_388_608);
    for value in [0, 1025] {
        assert!(server.set_section_install_limit(value).is_err());
    }
    server.set_section_install_limit(1024).unwrap();
    server.set_section_install_limit(24).unwrap();
    for case in 0..6 {
        let mut batch = column(server.authority().unwrap(), 3, true);
        match case {
            0 => batch[23].sections[0].cells = SnapshotCells::Uniform(BlockStateId(65535)),
            1 => batch[23].sections[0].pos = batch[0].sections[0].pos,
            2 => batch[23].entities[0].id = batch[0].entities[0].id,
            3 => batch[23].meta.compatibility.authority_schema += 1,
            4 => server.set_section_install_limit(23).unwrap(),
            _ => batch.clear(),
        }
        let before = image(server.authority().unwrap());
        assert!(
            server
                .install_section_batch(&pages, batch, &mut NoReplicas)
                .is_err()
        );
        assert_eq!(before, image(server.authority().unwrap()));
        assert!(server.section_install_diagnostics().failure.is_some());
        server.set_section_install_limit(24).unwrap();
    }
    assert_eq!(counts.appends.load(Ordering::SeqCst), 0);
    assert_eq!(counts.syncs.load(Ordering::SeqCst), 0);
}

#[test]
fn aggregate_output_and_journal_pressure_are_atomic_and_retryable() {
    for journal in [false, true] {
        let dir = Directory::new();
        let budget = if journal {
            base::execution_limits().runtime
        } else {
            OutputBudget::new(32_000, 2000)
        };
        let (mut server, pages, counts) =
            setup(&dir, budget, if journal { 2000 } else { 8_388_608 });
        let before = image(server.authority().unwrap());
        let batch = column(server.authority().unwrap(), 3, false);
        let result = server.install_section_batch(&pages, batch.clone(), &mut NoReplicas);
        assert!(matches!(result, Err(_) | Ok(CommitStep::Blocked(_))));
        assert_eq!(before, image(server.authority().unwrap()));
        assert_eq!(counts.appends.load(Ordering::SeqCst), 0);
        server.set_section_install_limit(1).unwrap();
        assert!(matches!(
            server
                .install_section_batch(&pages, vec![batch[0].clone()], &mut NoReplicas)
                .unwrap(),
            CommitStep::Durable(_)
        ));
    }
}

#[test]
fn partial_append_and_failed_sync_recover_zero_or_whole_batch_above_confirmed_floor() {
    for mode in [1, 2] {
        let dir = Directory::new();
        let (mut server, pages, counts) = setup(&dir, base::execution_limits().runtime, 8_388_608);
        let anchor = column(server.authority().unwrap(), 2, false).remove(0);
        assert!(matches!(
            server
                .install_section_batch(&pages, vec![anchor], &mut NoReplicas)
                .unwrap(),
            CommitStep::Durable(_)
        ));
        let floor = server.authority().unwrap().cursor().seq.0;
        counts.fail.store(mode, Ordering::SeqCst);
        let batch = column(server.authority().unwrap(), 3, true);
        assert!(
            server
                .install_section_batch(&pages, batch, &mut NoReplicas)
                .is_err()
        );
        assert!(server.authority().is_err());
        drop(server);
        let reopened = open(&dir, pages);
        let state = image(reopened.authority().unwrap());
        assert!(state.meta.cursor.seq.0 >= floor);
        assert_eq!(state.sections.len(), if mode == 1 { 1 } else { 25 });
        assert_eq!(state.entities.len(), if mode == 1 { 0 } else { 24 });
    }
}

#[test]
#[ignore = "run with --release --ignored --nocapture --test-threads=1"]
fn release_atomic_column_install_comparison() {
    for iteration in 0..6 {
        for count in if iteration % 2 == 0 {
            [1, 24, 0]
        } else {
            [0, 24, 1]
        } {
            let dir = Directory::new();
            let (mut server, pages, counts) =
                setup(&dir, base::execution_limits().runtime, 8_388_608);
            let batch = column(server.authority().unwrap(), 3, false);
            let start = Instant::now();
            if count == 0 {
                for mut page in batch {
                    page.meta =
                        torse_sim::streaming::page_meta(server.authority().unwrap().world());
                    assert!(matches!(
                        server
                            .install_section(&pages, page, &mut NoReplicas)
                            .unwrap(),
                        CommitStep::Durable(_)
                    ));
                }
            } else {
                assert!(matches!(
                    server
                        .install_section_batch(
                            &pages,
                            batch.into_iter().take(count).collect(),
                            &mut NoReplicas
                        )
                        .unwrap(),
                    CommitStep::Durable(_)
                ));
            }
            let elapsed = start.elapsed();
            println!(
                "atomic-install iteration={iteration} sections={} appends={} syncs={} us={} sections_per_second={:.1}",
                if count == 0 { 24 } else { count },
                counts.appends.load(Ordering::SeqCst),
                counts.syncs.load(Ordering::SeqCst),
                elapsed.as_micros(),
                (if count == 0 { 24 } else { count }) as f64 / elapsed.as_secs_f64()
            );
            assert_eq!(
                counts.syncs.load(Ordering::SeqCst),
                if count == 0 { 24 } else { 1 }
            );
            let expected = image(server.authority().unwrap());
            drop(server);
            assert_eq!(image(open(&dir, pages).authority().unwrap()), expected);
        }
    }
}

#[test]
fn tick_boundary_and_shared_memory_pressure_reject_without_mutation() {
    let dir = Directory::new();
    let (mut server, pages, counts) = setup(&dir, base::execution_limits().runtime, 8_388_608);
    server
        .attach_memory(
            MemoryPool::new(
                MemoryBudget::new(128 * 1024 * 1024)
                    .with_class_limit(MemoryClass::Codec, 9 * 1024 * 1024),
            ),
            1024 * 1024,
        )
        .unwrap();
    let batch = column(server.authority().unwrap(), 3, false);
    let before = image(server.authority().unwrap());
    assert!(
        server
            .install_section_batch(&pages, batch, &mut NoReplicas)
            .is_err()
    );
    assert_eq!(image(server.authority().unwrap()), before);
    assert_eq!(counts.appends.load(Ordering::SeqCst), 0);

    let dir = Directory::new();
    let (mut server, pages, _) = setup(&dir, base::execution_limits().runtime, 8_388_608);
    server.drive_one(&mut NoReplicas).unwrap();
    assert!(!server.authority().unwrap().at_tick_boundary());
    let batch = column(server.authority().unwrap(), 3, false);
    let before = image(server.authority().unwrap());
    assert!(
        server
            .install_section_batch(&pages, batch, &mut NoReplicas)
            .is_err()
    );
    assert_eq!(image(server.authority().unwrap()), before);
}

#[test]
fn atomic_install_crash_child() {
    let Some(root) = std::env::var_os("TORSE_ATOMIC_INSTALL_CRASH_ROOT") else {
        return;
    };
    let mode: usize = std::env::var("TORSE_ATOMIC_INSTALL_CRASH_MODE")
        .unwrap()
        .parse()
        .unwrap();
    let dir = Directory(root.into());
    let (mut server, pages, counts) = setup(&dir, base::execution_limits().runtime, 8_388_608);
    counts.fail.store(mode, Ordering::SeqCst);
    let batch = column(server.authority().unwrap(), 3, true);
    assert!(matches!(
        server
            .install_section_batch(&pages, batch, &mut NoReplicas)
            .unwrap(),
        CommitStep::Durable(_)
    ));
    std::process::exit(73);
}

#[test]
fn actual_process_exit_replays_whole_records_and_discards_partial_batch() {
    for (mode, code, sections) in [(0, 73, 24), (3, 74, 0), (4, 75, 24)] {
        let dir = Directory::new();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "atomic_install_crash_child", "--nocapture"])
            .env("TORSE_ATOMIC_INSTALL_CRASH_ROOT", &dir.0)
            .env("TORSE_ATOMIC_INSTALL_CRASH_MODE", mode.to_string())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(code));
        let pages = SectionPages::new(
            dir.0.join("sections"),
            base::budget(),
            base::codec_limits(),
            64,
        )
        .unwrap();
        let reopened = open(&dir, pages);
        let state = image(reopened.authority().unwrap());
        assert_eq!(state.sections.len(), sections);
        assert_eq!(state.entities.len(), sections);
        assert_eq!(state.next_entity_id, 1 + sections as u128);
        assert_eq!(state.meta.cursor.seq.0, u64::from(sections > 0));
    }
}

#[test]
fn stale_last_section_token_prevents_any_batch_publication() {
    let dir = Directory::new();
    let (server, _, _) = setup(&dir, base::execution_limits().runtime, 8_388_608);
    let batch = column(server.authority().unwrap(), 3, false);
    let (model, _) = base::world().unwrap();
    let mut world = WorldAuthority::new(
        base::WORLD,
        model.registry().clone(),
        model.schemas().clone(),
        16,
        WorldBounds::new(-64, 320).unwrap(),
        model.coordinator().compatibility(),
    )
    .unwrap();
    let before = world.seq();
    let prepared = world
        .prepare_generated_sections(&batch, base::budget())
        .unwrap();
    world
        .store_mut()
        .request(batch[23].sections[0].pos)
        .unwrap();
    assert!(world.publish_sections(prepared).is_err());
    assert_eq!(world.seq(), before);
    assert!(world.store().section(batch[0].sections[0].pos).is_none());
}

#[test]
fn replicas_invalidate_once_and_reseed_the_entire_column_at_one_cut() {
    use torse_sim::action::ActorContext;
    struct Sink {
        calls: usize,
        invalidations: Vec<SubscriptionId>,
    }
    impl InvalidationSink for Sink {
        fn invalidate(&mut self, _: Binding, ids: &[SubscriptionId]) -> Result<(), Fault> {
            self.calls += 1;
            self.invalidations.extend_from_slice(ids);
            Ok(())
        }
    }
    let dir = Directory::new();
    let (mut server, pages, _) = setup(&dir, base::execution_limits().runtime, 8_388_608);
    let batch = column(server.authority().unwrap(), 3, false);
    let mut scope = base::scope();
    scope.sections = [batch[0].sections[0].pos, batch[23].sections[0].pos]
        .into_iter()
        .collect();
    let actor = scope.sections.iter().fold(
        ActorContext::new(base::key("test:atomic-viewer")),
        |a, p| a.section(*p),
    );
    server
        .register_session(
            Viewer {
                session: SessionId(8),
                actor: ActorContextId(8),
            },
            actor,
            scope.clone(),
        )
        .unwrap();
    let seed = server
        .open_replica(SessionId(8), scope.clone(), base::budget())
        .unwrap();
    assert!(seed.state.sections.is_empty());
    let mut sink = Sink {
        calls: 0,
        invalidations: Vec::new(),
    };
    let CommitStep::Durable(done) = server
        .install_section_batch(&pages, batch, &mut sink)
        .unwrap()
    else {
        panic!("not durable")
    };
    assert_eq!(sink.calls, 1);
    assert_eq!(sink.invalidations, vec![seed.subscription]);
    let seed = server
        .open_replica(SessionId(8), scope, base::budget())
        .unwrap();
    assert_eq!(seed.baseline, done.publication.at);
    assert_eq!(seed.state.sections.len(), 2);
    assert_eq!(server.authority().unwrap().world().store().len(), 24);
}
