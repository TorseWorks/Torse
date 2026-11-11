//! Profiling must preserve final records, conflict outcomes, and recovery.
mod support;
#[path = "../examples/serial_baseline/workload.rs"]
mod workload;

use support::Directory;
use torse_diagnostics::{SerialProfiler, Stage};
use torse_server::{bootstrap as base, execution::*, *};
use torse_sim::action::ActorContext;
use torse_storage::*;
use torse_types::*;
use workload::Workload;

fn create(dir: &Directory, load: Workload) -> base::Files {
    let (world, actions) = base::world().unwrap();
    let compatibility = world.coordinator().compatibility();
    base::Files::create_file(
        dir.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            ExecutionHost::new(
                binding,
                world,
                actions,
                load.rules(),
                base::execution_limits(),
            )
        },
    )
    .unwrap()
}

#[test]
fn profiling_preserves_log_bytes_conflicts_and_recovered_execution() {
    let load = Workload {
        tasks: 8,
        contended: true,
    };
    assert_eq!(load.name(), "contended-8");
    let measured_dir = Directory::new();
    let plain_dir = Directory::new();
    let mut measured = create(&measured_dir, load);
    let mut plain = create(&plain_dir, load);
    let profiler = SerialProfiler::new(1024);
    measured.set_serial_profiler(profiler.clone());
    for server in [&mut measured, &mut plain] {
        server
            .register_session(
                Viewer {
                    session: SessionId(1),
                    actor: ActorContextId(1),
                },
                ActorContext::new(base::key("baseline:viewer")),
                base::scope(),
            )
            .unwrap();
        let subscription = server
            .open_replica(SessionId(1), base::scope(), base::budget())
            .unwrap()
            .subscription;
        assert_eq!(server.drive_tick(&mut NoReplicas, 64).unwrap(), 20);
        let batches = server.take_replica(subscription, base::budget()).unwrap();
        assert_eq!(batches.len(), 4, "only one of each competing pair commits");
        load.verify(server.authority().unwrap().world(), 1);
    }
    let expected = workload::state_bytes(measured.authority().unwrap()).unwrap();
    assert_eq!(
        expected,
        workload::state_bytes(plain.authority().unwrap()).unwrap()
    );
    let profile = profiler.profile();
    assert_eq!(profile.discarded_samples, 0);
    assert_eq!(profile.totals[Stage::Planning as usize].calls, 6);
    assert_eq!(profile.totals[Stage::Projection as usize].calls, 4);
    for stage in [
        Stage::Preparation,
        Stage::Encoding,
        Stage::Publication,
        Stage::Flush,
        Stage::JournalAppend,
        Stage::JournalSync,
    ] {
        assert_eq!(profile.totals[stage as usize].calls, 20);
    }
    drop(measured);
    drop(plain);
    let measured_log = std::fs::read(measured_dir.0.join("journal.log")).unwrap();
    assert_eq!(
        profile.totals[Stage::JournalAppend as usize].bytes,
        measured_log.len() as u64
    );
    assert_eq!(
        measured_log,
        std::fs::read(plain_dir.0.join("journal.log")).unwrap(),
    );
    let (world, actions) = base::world().unwrap();
    let recovered = base::Files::open_file(
        measured_dir.acquire(),
        world.coordinator().compatibility(),
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                world.registry(),
                world.schemas(),
                actions,
                load.rules(),
                base::execution_limits(),
            ))
        },
    )
    .unwrap();
    assert_eq!(
        expected,
        workload::state_bytes(recovered.authority().unwrap()).unwrap()
    );
}

#[test]
fn empty_ticks_skip_projection_and_disable_clears_all_measurement_hooks() {
    let directory = Directory::new();
    let load = Workload {
        tasks: 0,
        contended: false,
    };
    let mut server = create(&directory, load);
    let profiler = SerialProfiler::new(1024);
    server.set_serial_profiler(profiler.clone());
    assert_eq!(server.drive_tick(&mut NoReplicas, 64).unwrap(), 12);
    assert_eq!(profiler.totals()[Stage::Planning as usize].calls, 6);
    assert_eq!(profiler.totals()[Stage::Projection as usize].calls, 0);
    let previous = profiler.totals();
    server.set_serial_profiler(SerialProfiler::default());
    assert_eq!(server.drive_tick(&mut NoReplicas, 64).unwrap(), 12);
    assert_eq!(profiler.totals(), previous);
}

struct FailingAppend;
impl JournalBackend for FailingAppend {
    fn append(&mut self, _: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other("injected append failure"))
    }
    fn sync(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn failed_encoding_and_flush_keep_the_existing_publication_gates() {
    for oversized in [true, false] {
        let directory = Directory::new();
        let directory_lease = directory.acquire();
        let (world, actions) = base::world().unwrap();
        let binding = Binding {
            world: base::WORLD,
            epoch: directory_lease.epoch(),
            compatibility: world.coordinator().compatibility(),
        };
        let config = base::file_policy();
        let mut journal_config = config.journal;
        if oversized {
            journal_config.max_pending_bytes = 128;
        }
        let codec = base::Codec::new(base::codec_limits()).unwrap();
        let journal = Journal::with_backend(
            binding,
            CommitCursor::initial(base::WORLD),
            codec,
            journal_config,
            FailingAppend,
        )
        .unwrap();
        let checkpoints = Checkpoints::open(
            binding,
            codec,
            config.checkpoint,
            MemoryCheckpointBackend::new(16_777_216),
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
            Server::initialize(directory_lease, host, journal, checkpoints, config.server).unwrap();
        let profiler = SerialProfiler::new(64);
        server.set_serial_profiler(profiler.clone());
        if oversized {
            assert!(matches!(
                server.drive_one(&mut NoReplicas).unwrap(),
                CommitStep::Blocked(StageError::TooLarge)
            ));
            assert_eq!(server.authority().unwrap().cursor().seq.0, 0);
            assert!(matches!(
                server.drive_one(&mut NoReplicas).unwrap(),
                CommitStep::Blocked(StageError::TooLarge)
            ));
            assert_eq!(
                profiler.totals()[Stage::Planning as usize].calls,
                1,
                "blocked retries reuse the frozen plan"
            );
            assert_eq!(profiler.totals()[Stage::Publication as usize].calls, 0);
            assert_eq!(profiler.totals()[Stage::Flush as usize].calls, 0);
        } else {
            assert!(server.drive_one(&mut NoReplicas).is_err());
            assert_eq!(server.state(), ServerState::Faulted);
            assert!(server.authority().is_err());
            assert_eq!(server.durability().through.0, 0);
            assert_eq!(profiler.totals()[Stage::Publication as usize].calls, 1);
            assert_eq!(profiler.totals()[Stage::Flush as usize].calls, 1);
            assert_eq!(profiler.totals()[Stage::JournalAppend as usize].calls, 1);
            assert_eq!(profiler.totals()[Stage::JournalSync as usize].calls, 0);
        }
        assert!(profiler.totals()[Stage::Encoding as usize].calls > 0);
    }
}
