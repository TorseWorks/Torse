//! Compact runtime records and grouped durability preserve every serial prefix.
mod support;
#[allow(dead_code)]
#[path = "../examples/serial_baseline/workload.rs"]
mod workload;
use std::sync::{Arc, Mutex};
use support::Directory;
use torse_codec::{
    Decoder, Encoder, StorageCodec,
    wire::{self, Wire},
};
use torse_server::{bootstrap as base, execution::*, *};
use torse_sim::action::ActorContext;
use torse_sim::durable::*;
use torse_storage::*;
use torse_types::*;
use workload::Workload;

fn host(binding: Binding, load: Workload, optimized: bool) -> ExecutionHost {
    let (world, actions) = base::world().unwrap();
    let mut host = ExecutionHost::new(
        binding,
        world,
        actions,
        load.rules(),
        base::execution_limits(),
    )
    .unwrap();
    host.set_runtime_optimization(optimized);
    host
}
fn binding() -> Binding {
    let (world, _) = base::world().unwrap();
    Binding {
        world: base::WORLD,
        epoch: WorldEpoch(1),
        compatibility: world.coordinator().compatibility(),
    }
}
fn recovery<'a>(
    world: &'a torse_sim::WorldAuthority,
    binding: Binding,
    load: Workload,
) -> ExecutionRecovery<'a> {
    let (_, actions) = base::world().unwrap();
    ExecutionRecovery::new(
        binding,
        world.registry(),
        world.schemas(),
        actions,
        load.rules(),
        base::execution_limits(),
    )
}

#[test]
fn full_and_compact_records_recover_identical_state_at_every_prefix_and_reject_forged_progress() {
    let load = Workload {
        tasks: 8,
        contended: true,
    };
    let mut full = host(binding(), load, false);
    let mut compact = host(binding(), load, true);
    let snapshot = compact.capture_snapshot(base::budget()).unwrap();
    let (world, _) = base::world().unwrap();
    let codec = base::Codec::new(base::codec_limits()).unwrap();
    let mut records: Vec<Vec<u8>> = vec![];
    let mut progress = 0;
    for _ in 0..40 {
        let a = full.prepare_next().unwrap().unwrap();
        let b = compact.prepare_next().unwrap().unwrap();
        if matches!(b.record().changes.runtime, RuntimeUpdate::Progress(_)) {
            progress += 1;
            let mut invalid = codec_bytes(b.record());
            let mut record = codec.decode_commit(&invalid, base::budget()).unwrap();
            let RuntimeUpdate::Progress(p) = &mut record.changes.runtime else {
                unreachable!()
            };
            p.next += 1;
            invalid.clear();
            codec
                .encode_commit(&record, &mut invalid, base::budget())
                .unwrap();
            let mut target = recovery(&world, binding(), load);
            target
                .restore_snapshot(WorldSnapshot {
                    meta: snapshot.meta,
                    state: snapshot.state.clone(),
                })
                .unwrap();
            for bytes in &records {
                target
                    .replay(codec.decode_commit(bytes, base::budget()).unwrap())
                    .unwrap();
            }
            assert!(
                target
                    .replay(codec.decode_commit(&invalid, base::budget()).unwrap())
                    .is_err()
            );
        }
        let bytes = codec_bytes(b.record());
        assert_eq!(
            &bytes[..2],
            &torse_codec::wire::EXECUTION_COMMIT_VERSION.to_le_bytes()
        );
        for end in 0..bytes.len() {
            assert!(codec.decode_commit(&bytes[..end], base::budget()).is_err());
        }
        let mut tail = bytes.clone();
        tail.push(0);
        assert!(codec.decode_commit(&tail, base::budget()).is_err());
        records.push(bytes);
        full.publish(a).unwrap();
        compact.publish(b).unwrap();
        assert_eq!(
            workload::state_bytes(&full).unwrap(),
            workload::state_bytes(&compact).unwrap()
        );
        let mut target = recovery(&world, binding(), load);
        target
            .restore_snapshot(WorldSnapshot {
                meta: snapshot.meta,
                state: snapshot.state.clone(),
            })
            .unwrap();
        for bytes in &records {
            target
                .replay(codec.decode_commit(bytes, base::budget()).unwrap())
                .unwrap();
        }
        let recovered = target.finish(compact.cursor()).unwrap();
        assert_eq!(
            workload::state_bytes(&recovered).unwrap(),
            workload::state_bytes(&compact).unwrap()
        );
    }
    assert_eq!(progress, 16);
}
fn codec_bytes(record: &CommitRecord<ExecutionChanges>) -> Vec<u8> {
    let mut bytes = vec![];
    base::Codec::new(base::codec_limits())
        .unwrap()
        .encode_commit(record, &mut bytes, base::budget())
        .unwrap();
    bytes
}

#[derive(Clone, Copy)]
struct LegacyCodec(base::Codec);
struct LegacyRecord<'a>(&'a CommitRecord<ExecutionChanges>);
impl Wire for LegacyRecord<'_> {
    fn write(&self, e: &mut Encoder<'_>) -> Result<(), Fault> {
        self.0.meta.write(e)?;
        self.0.changes.world.write(e)?;
        let RuntimeUpdate::Full(state) = &self.0.changes.runtime else {
            return Err(Fault::invalid_data("legacy writer needs full state"));
        };
        state.write(e)?;
        self.0.changes.outcome.write(e)?;
        self.0.changes.cancelled.write(e)
    }
    fn read(_: &mut Decoder<'_>) -> Result<Self, Fault> {
        Err(Fault::invalid_data("write-only legacy fixture"))
    }
}
impl StorageCodec<ExecutionModel> for LegacyCodec {
    fn encode_snapshot(
        &self,
        v: &WorldSnapshot<ExecutionSnapshot>,
        out: &mut Vec<u8>,
        b: OutputBudget,
    ) -> Result<(), Fault> {
        self.0.encode_snapshot(v, out, b)
    }
    fn decode_snapshot(
        &self,
        v: &[u8],
        b: OutputBudget,
    ) -> Result<WorldSnapshot<ExecutionSnapshot>, Fault> {
        self.0.decode_snapshot(v, b)
    }
    fn encode_commit(
        &self,
        v: &CommitRecord<ExecutionChanges>,
        out: &mut Vec<u8>,
        b: OutputBudget,
    ) -> Result<(), Fault> {
        wire::encode(&LegacyRecord(v), out, b, base::codec_limits())
    }
    fn decode_commit(
        &self,
        v: &[u8],
        b: OutputBudget,
    ) -> Result<CommitRecord<ExecutionChanges>, Fault> {
        self.0.decode_commit(v, b)
    }
}

#[test]
fn old_and_new_journal_versions_mix_without_checkpoint_migration() {
    let dir = Directory::new();
    let load = Workload {
        tasks: 8,
        contended: false,
    };
    let codec = base::Codec::new(base::codec_limits()).unwrap();
    let mut old = FileServer::<ExecutionHost, LegacyCodec>::create_file(
        dir.acquire(),
        binding().compatibility,
        LegacyCodec(codec),
        base::file_policy(),
        |b| Ok(host(b, load, false)),
    )
    .unwrap();
    old.drive_tick(&mut NoReplicas, 64).unwrap();
    drop(old);
    let mut new = base::Files::open_file(
        dir.acquire(),
        binding().compatibility,
        codec,
        base::file_policy(),
        |b| {
            // The registry references must live outside this closure.
            Ok(recovery(legacy_world(), b, load))
        },
    )
    .unwrap();
    new.set_commit_batch_limit(16).unwrap();
    new.drive_tick(&mut NoReplicas, 64).unwrap();
    let expected = workload::state_bytes(new.authority().unwrap()).unwrap();
    drop(new);
    let recovered = base::Files::open_file(
        dir.acquire(),
        binding().compatibility,
        codec,
        base::file_policy(),
        |b| Ok(recovery(legacy_world(), b, load)),
    )
    .unwrap();
    assert_eq!(
        expected,
        workload::state_bytes(recovered.authority().unwrap()).unwrap()
    );
}
fn legacy_world() -> &'static torse_sim::WorldAuthority {
    static WORLD: std::sync::OnceLock<torse_sim::WorldAuthority> = std::sync::OnceLock::new();
    WORLD.get_or_init(|| base::world().unwrap().0)
}

#[derive(Default)]
struct Trace {
    events: Vec<&'static str>,
    bytes: Vec<u8>,
    appends: usize,
    syncs: usize,
}
struct Backend {
    trace: Arc<Mutex<Trace>>,
    fail_append: usize,
    fail_sync: bool,
}
impl JournalBackend for Backend {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let mut t = self.trace.lock().unwrap();
        t.appends += 1;
        t.events.push("append");
        if t.appends == self.fail_append {
            return Err(std::io::Error::other("injected batch append failure"));
        }
        t.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn sync(&mut self) -> std::io::Result<()> {
        let mut t = self.trace.lock().unwrap();
        t.syncs += 1;
        t.events.push("sync");
        if self.fail_sync {
            Err(std::io::Error::other("injected batch sync failure"))
        } else {
            Ok(())
        }
    }
}
struct Sink(Arc<Mutex<Trace>>);
impl InvalidationSink for Sink {
    fn invalidate(&mut self, _: Binding, ids: &[SubscriptionId]) -> Result<(), Fault> {
        self.0.lock().unwrap().events.push(if ids.is_empty() {
            "invalidate"
        } else {
            "revoke"
        });
        Ok(())
    }
}
type Memory = Server<
    ExecutionHost,
    Journal<ExecutionModel, base::Codec, Backend>,
    MemoryCheckpoints<ExecutionModel, base::Codec>,
>;

struct CrashHost {
    inner: ExecutionHost,
    published: usize,
    early: bool,
}
impl HostAuthority for CrashHost {
    type Model = ExecutionModel;
    type Prepared = ExecutionPrepared;
    fn binding(&self) -> Binding {
        self.inner.binding()
    }
    fn cursor(&self) -> CommitCursor {
        self.inner.cursor()
    }
    fn resume_point(&self) -> ResumePoint {
        self.inner.resume_point()
    }
    fn is_tick_start(&self, p: ResumePoint) -> bool {
        self.inner.is_tick_start(p)
    }
    fn at_tick_boundary(&self) -> bool {
        self.inner.at_tick_boundary()
    }
    fn submit(&mut self, r: WorldRequest<Call>) -> Admission {
        self.inner.submit(r)
    }
    fn prepare_next(&mut self) -> Result<Option<ExecutionPrepared>, Fault> {
        self.inner.prepare_next()
    }
    fn publish(&mut self, p: ExecutionPrepared) -> Result<Publication, Fault> {
        let result = self.inner.publish(p)?;
        self.published += 1;
        if self.early && self.published == 8 {
            std::process::exit(81);
        }
        Ok(result)
    }
    fn capture_snapshot(
        &mut self,
        b: OutputBudget,
    ) -> Result<WorldSnapshot<ExecutionSnapshot>, Fault> {
        self.inner.capture_snapshot(b)
    }
    fn stop_admission(&mut self, t: Tick) -> Result<(), Fault> {
        self.inner.stop_admission(t)
    }
    fn shutdown_ready(&self) -> bool {
        self.inner.shutdown_ready()
    }
}

#[test]
fn batch_crash_child() {
    let Ok(path) = std::env::var("TORSE_BATCH_CRASH_PATH") else {
        return;
    };
    let mode = std::env::var("TORSE_BATCH_CRASH_MODE").unwrap();
    let mut config = base::file_policy();
    config.journal.max_pending_records = 16;
    if mode == "prelude" {
        let mut server = base::Files::create_file(
            WorldDirectory::acquire(path, base::WORLD, 65536).unwrap(),
            binding().compatibility,
            base::Codec::new(base::codec_limits()).unwrap(),
            config,
            |b| {
                Ok(host(
                    b,
                    Workload {
                        tasks: 8,
                        contended: false,
                    },
                    true,
                ))
            },
        )
        .unwrap();
        let flushes = server.flush_metrics().calls;
        let report = server.drive_phase_prelude(&mut NoReplicas, 16).unwrap();
        assert_eq!(report.committed, 7);
        assert_eq!(report.durability.through.0, 7);
        assert_eq!(server.flush_metrics().calls - flushes, 1);
        std::process::exit(82);
    }
    let mut server = FileServer::<CrashHost, base::Codec>::create_file(
        WorldDirectory::acquire(path, base::WORLD, 65536).unwrap(),
        binding().compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        config,
        |b| {
            Ok(CrashHost {
                inner: host(
                    b,
                    Workload {
                        tasks: 8,
                        contended: false,
                    },
                    true,
                ),
                published: 0,
                early: mode == "publish",
            })
        },
    )
    .unwrap();
    assert_eq!(
        server.drive_batch(&mut NoReplicas, 16).unwrap().committed,
        16
    );
    std::process::exit(82);
}

#[test]
fn process_exit_discards_unflushed_batch_or_recovers_synced_batch_without_duplicate_edits() {
    let load = Workload {
        tasks: 8,
        contended: false,
    };
    for mode in ["publish", "durable", "prelude"] {
        let dir = Directory::new();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "batch_crash_child", "--nocapture"])
            .env("TORSE_BATCH_CRASH_PATH", &dir.0)
            .env("TORSE_BATCH_CRASH_MODE", mode)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(if mode == "publish" { 81 } else { 82 }));
        let mut recovered = base::Files::open_file(
            dir.acquire(),
            binding().compatibility,
            base::Codec::new(base::codec_limits()).unwrap(),
            base::file_policy(),
            |b| Ok(recovery(legacy_world(), b, load)),
        )
        .unwrap();
        assert_eq!(
            recovered.authority().unwrap().cursor().seq.0,
            match mode {
                "publish" => 0,
                "prelude" => 7,
                _ => 16,
            }
        );
        if mode == "prelude" {
            let work = recovered
                .authority()
                .unwrap()
                .runtime()
                .work
                .as_ref()
                .unwrap();
            assert_eq!((work.tasks.len(), work.next), (8, 0));
            assert!(recovered.drive_phase_prelude(&mut NoReplicas, 16).is_err());
            assert_eq!(recovered.authority().unwrap().cursor().seq.0, 7);
        }
        recovered.set_commit_batch_limit(16).unwrap();
        recovered.drive_tick(&mut NoReplicas, 64).unwrap();
        load.verify(recovered.authority().unwrap().world(), 1);
        assert_eq!(recovered.authority().unwrap().cursor().seq.0, 20);
    }
}
fn memory(
    dir: &Directory,
    trace: &Arc<Mutex<Trace>>,
    capacity: u32,
    fail_append: usize,
    fail_sync: bool,
    replica_queue: usize,
) -> Memory {
    let lease = dir.acquire();
    let mut binding = binding();
    binding.epoch = lease.epoch();
    let mut config = base::file_policy();
    config.journal.max_pending_records = capacity;
    let codec = base::Codec::new(base::codec_limits()).unwrap();
    let journal = Journal::with_backend(
        binding,
        CommitCursor::initial(base::WORLD),
        codec,
        config.journal,
        Backend {
            trace: Arc::clone(trace),
            fail_append,
            fail_sync,
        },
    )
    .unwrap();
    let cp = Checkpoints::open(
        binding,
        codec,
        config.checkpoint,
        MemoryCheckpointBackend::new(16_777_216),
    )
    .unwrap();
    let (world, actions) = base::world().unwrap();
    let mut limits = base::execution_limits();
    limits.replicas.max_queued_batches = replica_queue;
    let authority = ExecutionHost::new(
        binding,
        world,
        actions,
        Workload {
            tasks: 8,
            contended: false,
        }
        .rules(),
        limits,
    )
    .unwrap();
    Server::initialize(lease, authority, journal, cp, config.server).unwrap()
}

#[test]
fn batch_capacity_flushes_accepted_prefix_and_invalidation_precedes_barrier() {
    let dir = Directory::new();
    let trace = Arc::new(Mutex::new(Trace::default()));
    let mut server = memory(&dir, &trace, 2, 0, false, 16);
    let mut sink = Sink(Arc::clone(&trace));
    let report = server.drive_batch(&mut sink, 16).unwrap();
    assert_eq!(report.committed, 2);
    assert!(matches!(report.blocked, Some(StageError::Busy)));
    assert_eq!(report.durability.through.0, 2);
    assert_eq!(
        trace.lock().unwrap().events,
        vec!["invalidate", "invalidate", "append", "append", "sync"]
    );
    server.set_commit_batch_limit(16).unwrap();
    assert_eq!(server.drive_tick(&mut sink, 64).unwrap(), 18);
    assert_eq!(server.authority().unwrap().cursor().seq.0, 20);
    assert_eq!(trace.lock().unwrap().syncs, 10);
    assert_eq!(server.durability().through.0, 20);
    assert!(server.set_commit_batch_limit(0).is_err());
    assert!(server.drive_batch(&mut sink, 1025).is_err());
}

#[test]
fn phase_prelude_retries_a_bounded_journal_prefix() {
    let dir = Directory::new();
    let trace = Arc::new(Mutex::new(Trace::default()));
    let mut server = memory(&dir, &trace, 2, 0, false, 16);
    let mut sink = Sink(Arc::clone(&trace));
    for seq in [2, 4, 6] {
        let batch = server.drive_phase_prelude(&mut sink, 16).unwrap();
        assert_eq!(batch.committed, 2);
        assert_eq!(batch.durability.through.0, seq);
        assert!(matches!(batch.blocked, Some(StageError::Busy)));
    }
    let batch = server.drive_phase_prelude(&mut sink, 16).unwrap();
    assert_eq!(batch.committed, 1);
    assert_eq!(batch.durability.through.0, 7);
    assert!(batch.blocked.is_none());
    let work = server.authority().unwrap().runtime().work.as_ref().unwrap();
    assert_eq!((work.tasks.len(), work.next), (8, 0));
    assert_eq!(trace.lock().unwrap().syncs, 4);
}

#[test]
fn batch_append_or_barrier_failure_never_releases_results_and_recovers_only_complete_prefix() {
    for fail_append in [8, 0] {
        let dir = Directory::new();
        let trace = Arc::new(Mutex::new(Trace::default()));
        let mut server = memory(&dir, &trace, 16, fail_append, fail_append == 0, 16);
        let viewer = Viewer {
            session: SessionId(1),
            actor: ActorContextId(1),
        };
        server
            .register_session(
                viewer,
                ActorContext::new(base::key("batch:reader")),
                base::scope(),
            )
            .unwrap();
        let subscription = server
            .open_replica(SessionId(1), base::scope(), base::budget())
            .unwrap()
            .subscription;
        assert!(
            server
                .drive_batch(&mut Sink(Arc::clone(&trace)), 16)
                .is_err()
        );
        assert_eq!(server.state(), ServerState::Faulted);
        assert_eq!(server.durability().through.0, 0);
        assert!(server.authority().is_err());
        assert!(server.take_replica(subscription, base::budget()).is_err());
        assert!(
            server
                .open_replica(SessionId(1), base::scope(), base::budget())
                .is_err()
        );
        let id = RequestId {
            world: base::WORLD,
            epoch: server.binding().epoch,
            session: SessionId(1),
            seq: RequestSeq(1),
        };
        assert!(server.receipt(id).is_err());
        assert!(server.checkpoint().is_err());
        let mut start = host(
            server.binding(),
            Workload {
                tasks: 8,
                contended: false,
            },
            true,
        );
        let snapshot = start.capture_snapshot(base::budget()).unwrap();
        let mut target = recovery(
            legacy_world(),
            server.binding(),
            Workload {
                tasks: 8,
                contended: false,
            },
        );
        target.restore_snapshot(snapshot).unwrap();
        let data = trace.lock().unwrap().bytes.clone();
        let report = scan_journal(
            &mut data.as_slice(),
            server.binding(),
            CommitCursor::initial(base::WORLD),
            &base::Codec::new(base::codec_limits()).unwrap(),
            base::codec_limits(),
            base::budget(),
            |r| target.replay(r),
        )
        .unwrap();
        assert_eq!(report.records, if fail_append == 0 { 16 } else { 7 });
        let recovered = target.finish(report.through).unwrap();
        assert_eq!(recovered.cursor(), report.through);
    }
}

#[test]
fn slow_replica_is_invalidated_inside_batch_before_any_barrier_output() {
    let dir = Directory::new();
    let trace = Arc::new(Mutex::new(Trace::default()));
    let mut server = memory(&dir, &trace, 16, 0, false, 1);
    server
        .register_session(
            Viewer {
                session: SessionId(1),
                actor: ActorContextId(1),
            },
            ActorContext::new(base::key("batch:viewer")),
            base::scope(),
        )
        .unwrap();
    let subscription = server
        .open_replica(SessionId(1), base::scope(), base::budget())
        .unwrap()
        .subscription;
    let report = server
        .drive_batch(&mut Sink(Arc::clone(&trace)), 16)
        .unwrap();
    assert_eq!(report.committed, 16);
    assert_eq!(report.durability.through.0, 16);
    let trace = trace.lock().unwrap();
    let revoke = trace.events.iter().position(|e| *e == "revoke").unwrap();
    let append = trace.events.iter().position(|e| *e == "append").unwrap();
    assert!(revoke < append);
    assert!(server.take_replica(subscription, base::budget()).is_err());
}
