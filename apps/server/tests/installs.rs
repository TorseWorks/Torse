#[path = "support/streaming.rs"]
mod fixture;
mod support;
use fixture::*;
use support::Directory;
use torse_server::{bootstrap as base, installs::*, *};
use torse_types::*;
use torse_world::*;

fn inputs(server: &base::Files, count: i64) -> Vec<Snapshot> {
    (2..2 + count)
        .map(|x| {
            generated_with_entity(
                server.authority().unwrap().world(),
                SectionPos::new(x, 0, 0),
            )
        })
        .collect()
}
fn limits() -> SectionInstallLimits {
    SectionInstallLimits {
        max_pages: 24,
        max_bytes: 8 * 1024 * 1024,
    }
}

#[test]
fn twenty_four_entity_pages_retry_backpressure_with_stable_ids_and_recover() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(&dir);
    let first = server
        .authority()
        .unwrap()
        .world()
        .entities()
        .next_entity_id();
    let mut batch = server
        .begin_section_installs(pages.clone(), inputs(&server, 24), limits())
        .unwrap();
    let before = server.flush_metrics().calls;
    let mut count = 0;
    while batch.remaining() != 0 {
        let report = server
            .install_sections(&mut batch, &mut NoReplicas)
            .unwrap();
        assert!(report.failure.is_none());
        assert!(!report.installed.is_empty());
        assert!(
            report
                .installed
                .iter()
                .all(|p| report.durability.covers(p.at))
        );
        for installed in report.installed {
            assert_eq!(installed.position, SectionPos::new(count + 2, 0, 0));
            assert_eq!(
                installed.entities.values().copied().collect::<Vec<_>>(),
                [EntityId(first + count as u128)]
            );
            count += 1;
        }
        assert_eq!(report.blocked.is_some(), batch.remaining() != 0);
    }
    assert_eq!(count, 24);
    assert_eq!(server.flush_metrics().calls - before, 6);
    assert_eq!(batch.accepted().len(), 24);
    assert_eq!(batch.durability(), server.durability());
    let next = server
        .authority()
        .unwrap()
        .world()
        .entities()
        .next_entity_id();
    assert_eq!(next, first + 24);
    drop(batch);
    drop(server);
    let server = open(&dir, pages).unwrap();
    assert_eq!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id(),
        next
    );
    for i in 0..24 {
        let world = server.authority().unwrap().world();
        let entity = world
            .entities()
            .get(world.entities().handle_of(EntityId(first + i)).unwrap())
            .unwrap();
        assert_eq!(
            entity.position.section(16),
            Some(SectionPos::new(i as i64 + 2, 0, 0))
        );
    }
}

#[test]
fn invalid_page_confirms_only_earlier_prefix_and_budget_rejection_changes_nothing() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(&dir);
    let before = server.authority().unwrap().cursor();
    assert!(
        server
            .begin_section_installs(
                pages.clone(),
                inputs(&server, 24),
                SectionInstallLimits {
                    max_pages: 23,
                    ..limits()
                }
            )
            .is_err()
    );
    assert!(
        server
            .begin_section_installs(
                pages.clone(),
                inputs(&server, 24),
                SectionInstallLimits {
                    max_bytes: 32,
                    ..limits()
                }
            )
            .is_err()
    );
    assert_eq!(server.authority().unwrap().cursor(), before);
    let first = server
        .authority()
        .unwrap()
        .world()
        .entities()
        .next_entity_id();
    let mut input = inputs(&server, 3);
    input[1].entities[0].position_bits = Position::new(1.5, 1.5, 1.5).to_bits();
    let mut batch = server
        .begin_section_installs(pages.clone(), input, limits())
        .unwrap();
    let report = server
        .install_sections(&mut batch, &mut NoReplicas)
        .unwrap();
    assert_eq!(report.installed.len(), 1);
    assert_eq!(report.failure.unwrap().0, SectionPos::new(3, 0, 0));
    assert_eq!(batch.remaining(), 2);
    assert!(report.durability.covers(report.installed[0].at));
    assert_eq!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id(),
        first + 1
    );
    assert_eq!(
        server
            .install_sections(&mut batch, &mut NoReplicas)
            .unwrap()
            .installed
            .len(),
        0
    );
    assert_eq!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id(),
        first + 1
    );
}

#[test]
fn intervening_authority_changes_make_the_original_batch_stale() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(&dir);
    let mut batch = server
        .begin_section_installs(pages.clone(), inputs(&server, 3), limits())
        .unwrap();
    let page = generated_with_entity(
        server.authority().unwrap().world(),
        SectionPos::new(9, 0, 0),
    );
    server
        .install_section(&pages, page, &mut NoReplicas)
        .unwrap();
    let before = server.authority().unwrap().cursor();
    assert_eq!(
        server
            .install_sections(&mut batch, &mut NoReplicas)
            .err()
            .unwrap()
            .code,
        FaultCode::ResetRequired
    );
    assert_eq!(server.authority().unwrap().cursor(), before);
    assert_eq!(batch.remaining(), 3);
}

#[test]
fn input_reservation_is_bounded_and_owned_by_the_install_handle() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(&dir);
    let pool = MemoryPool::new(MemoryBudget::new(256 * 1024 * 1024));
    server
        .attach_memory(pool.clone(), 48 * 1024 * 1024)
        .unwrap();
    let before = pool.usage().total;
    let batch = server
        .begin_section_installs(pages.clone(), inputs(&server, 3), limits())
        .unwrap();
    assert_eq!(pool.usage().total, before + limits().max_bytes);
    drop(batch);
    assert_eq!(pool.usage().total, before);
    assert!(
        server
            .begin_section_installs(
                pages,
                inputs(&server, 3),
                SectionInstallLimits {
                    max_bytes: 512 * 1024 * 1024,
                    ..limits()
                }
            )
            .is_err()
    );
    assert_eq!(pool.usage().total, before);
}

#[test]
fn trusted_install_crash_child() {
    let Some(root) = std::env::var_os("TORSE_INSTALL_CRASH_ROOT") else {
        return;
    };
    let dir = Directory(root.into());
    let pages = pages(&dir);
    let mut server = create(&dir);
    let mut batch = server
        .begin_section_installs(pages, inputs(&server, 24), limits())
        .unwrap();
    let report = server
        .install_sections(&mut batch, &mut NoReplicas)
        .unwrap();
    assert_eq!(report.installed.len(), 4);
    std::fs::write(dir.0.join("ready"), b"durable-prefix-4").unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn terminated_process_recovers_confirmed_prefix_then_continues_ids() {
    let dir = Directory::new();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "trusted_install_crash_child", "--nocapture"])
        .env("TORSE_INSTALL_CRASH_ROOT", &dir.0)
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    while !dir.0.join("ready").exists() {
        if child.try_wait().unwrap().is_some() {
            panic!("child exited before durable prefix");
        }
        if start.elapsed() > std::time::Duration::from_secs(30) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("child did not signal ready");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let pages = pages(&dir);
    let mut server = open(&dir, pages.clone()).unwrap();
    let next = server
        .authority()
        .unwrap()
        .world()
        .entities()
        .next_entity_id();
    assert_eq!(server.authority().unwrap().world().store().len(), 6);
    let mut input = inputs(&server, 24);
    input.drain(..4);
    let mut batch = server
        .begin_section_installs(pages, input, limits())
        .unwrap();
    while batch.remaining() != 0 {
        assert!(
            server
                .install_sections(&mut batch, &mut NoReplicas)
                .unwrap()
                .failure
                .is_none()
        );
    }
    assert_eq!(server.authority().unwrap().world().store().len(), 26);
    assert_eq!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id(),
        next + 20
    );
}

#[test]
fn partial_append_and_failed_sync_recover_actual_prefix_and_allocator() {
    use std::io::Write;
    use torse_server::execution::ExecutionHost;
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
        let first = world.entities().next_entity_id();
        let input: Vec<_> = (2..8)
            .map(|x| generated_with_entity(&world, SectionPos::new(x, 0, 0)))
            .collect();
        let binding = Binding {
            world: base::WORLD,
            epoch: directory.epoch(),
            compatibility: world.coordinator().compatibility(),
        };
        let policy = base::file_policy();
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
            policy.journal,
            backend,
        )
        .unwrap();
        let checkpoints = Checkpoints::open(
            binding,
            codec,
            policy.checkpoint,
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
            Server::initialize(directory, host, journal, checkpoints, policy.server).unwrap();
        let mut batch = server
            .begin_section_installs(pages.clone(), input, limits())
            .unwrap();
        assert!(
            server
                .install_sections(&mut batch, &mut NoReplicas)
                .is_err()
        );
        assert!(server.authority().is_err());
        let accepted = 4;
        let recovered = if partial { 1 } else { 4 };
        assert_eq!(batch.accepted().len(), accepted);
        assert_eq!(batch.durability().through, DurableSeq(0));
        assert_eq!(batch.remaining(), 6 - accepted);
        drop(batch);
        drop(server);
        let server = open(&dir, pages).unwrap();
        let world = server.authority().unwrap().world();
        assert_eq!(world.store().len(), 2 + recovered);
        assert_eq!(world.entities().next_entity_id(), first + recovered as u128);
        for i in 0..recovered {
            let entity = world
                .entities()
                .get(
                    world
                        .entities()
                        .handle_of(EntityId(first + i as u128))
                        .unwrap(),
                )
                .unwrap();
            assert_eq!(
                entity.position.section(16),
                Some(SectionPos::new(i as i64 + 2, 0, 0))
            );
        }
    }
}
