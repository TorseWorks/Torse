//! Reservation-before-publication journals with a preallocated acceptance queue.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom};
use std::marker::PhantomData;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use torse_codec::record::{self, RecordKind};
use torse_codec::{CodecLimits, Encoder, StorageCodec};
use torse_diagnostics::{SerialProfiler, Stage};
use torse_types::{
    AuthorityData, Binding, CommitCursor, CommitSeq, Durability, DurableSeq, Fault, FaultCode,
    ModelCommit, OutputBudget, StageError,
};

use crate::{FileBackend, JournalBackend, MemoryBackend, RecoveryReport, segments};

/// Storage-side interface from the authority/I/O contract.
///
/// The host stages before authority publication, then immediately accepts the same token.
/// Neither staging nor acceptance performs file I/O. External success notifications wait
/// for `flush_through`; dropping a token cancels only its storage reservation.
pub trait CommitJournal<M: AuthorityData> {
    /// Sequential read of existing ledgers, without I/O, allocation or new accounting.
    /// Custom journals report unsupported by default.
    fn queue_diagnostics(
        &self,
        config: &torse_types::QueueDiagnostics,
    ) -> torse_types::Diagnostic<torse_types::JournalQueues> {
        if let Err(reason) = config.check::<torse_types::JournalQueues>() {
            return torse_types::Diagnostic::Unavailable(reason);
        }
        torse_types::Diagnostic::Unavailable(torse_types::Unavailable::NotSupported)
    }
    /// Implementation-owned reservation; the concrete token must not implement Clone or Copy.
    type Staged;

    /// Validate and encode before authority publication. Busy permits retry after flushing;
    /// TooLarge and InvalidRecord leave sequence and reservations unchanged.
    fn try_stage(&mut self, record: &ModelCommit<M>) -> Result<Self::Staged, StageError>;
    /// Consume the reserved token immediately after publication. Errors are terminal;
    /// the host stops authority advancement instead of publishing the same change again.
    fn accept(&mut self, staged: Self::Staged) -> Result<(), Fault>;
    /// Append and synchronize the accepted prefix through target. Success may cover a later
    /// position if it was already durable. Write or barrier failure stops the instance.
    fn flush_through(&mut self, target: CommitCursor) -> Result<Durability, Fault>;
    /// Observe the last confirmed position, including after a terminal storage fault.
    fn durability(&self) -> Durability;
    /// Persist the oldest retained checkpoint cursor before reclaiming covered segments.
    /// The caller owns checkpoint retention; a no-op backend keeps all history.
    fn reclaim(&mut self, _floor: CommitCursor) -> Result<(), Fault> {
        Ok(())
    }
}

/// Explicit bounds for queue storage, single-record encoding, and input framing.
///
/// `max_pending_bytes` counts framed bytes, including outstanding reservations. Encoding
/// uses separate bounded scratch, at most two record buffers in addition to the queue.
/// `codec_budget` also limits logical payload entries. There are no unbounded defaults.
#[derive(Clone, Copy, Debug)]
pub struct JournalConfig {
    /// Queue slots shared by accepted entries and the one permitted outstanding token.
    pub max_pending_records: u32,
    /// Sum of encoded envelope lengths held by those slots, excluding bounded scratch.
    pub max_pending_bytes: u64,
    /// Per-record domain encoding limit; the byte limit excludes the outer envelope.
    pub codec_budget: OutputBudget,
    /// Outer frame limit. The supplied domain codec maintains its own field/depth checks.
    pub codec_limits: CodecLimits,
    /// Maximum encoded bytes per segment after explicit journal reclamation is enabled.
    pub max_segment_bytes: u64,
    /// Maximum segment files, excluding the fixed retention root and its replacement.
    pub max_segment_files: u32,
}

impl JournalConfig {
    /// Validate local queue policy before a host opens or repairs world data.
    pub fn validate(self) -> Result<(), Fault> {
        if self.max_pending_records == 0
            || self.max_pending_bytes < record::OVERHEAD_BYTES as u64
            || self.codec_budget.max_items == 0
            || self.max_segment_bytes <= record::OVERHEAD_BYTES as u64
            || self.max_segment_files == 0
        {
            return Err(Fault::invalid_data("journal limits cannot hold a record"));
        }
        Ok(())
    }

    fn encode_budget(self) -> OutputBudget {
        // A record that cannot fit an otherwise empty queue is TooLarge, never Busy.
        OutputBudget::new(
            self.codec_budget.max_items,
            self.codec_budget
                .max_bytes
                .min(u64::from(self.codec_limits.max_frame_bytes))
                .min(self.max_pending_bytes - record::OVERHEAD_BYTES as u64)
                .min(self.max_segment_bytes - record::OVERHEAD_BYTES as u64),
        )
    }
}

#[derive(Debug)]
struct ReservationPool {
    records: AtomicU32,
    bytes: AtomicU64,
    staged: AtomicBool,
}

/// Only the coordinator adds leases; tokens may release them from other threads.
#[derive(Debug)]
struct Reservation {
    pool: Arc<ReservationPool>,
    bytes: u64,
    staged: bool,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.pool.bytes.fetch_sub(self.bytes, Ordering::AcqRel);
        self.pool.records.fetch_sub(1, Ordering::AcqRel);
        if self.staged {
            self.pool.staged.store(false, Ordering::Release);
        }
    }
}

#[derive(Debug)]
struct Pending {
    bytes: Vec<u8>,
    cursor: CommitCursor,
    reservation: Reservation,
}

/// Owned, non-cloneable reservation bound to one instance and live Binding.
///
/// Private fields prevent forging positions or replacing encoded bytes. Dropping the token
/// releases capacity and permits retry at the same position without consuming a sequence.
#[derive(Debug)]
#[must_use = "accept the staged record after publication, or drop it to cancel the reservation"]
pub struct Staged {
    pending: Pending,
    binding: Binding,
    previous: CommitSeq,
}

pub type MemoryJournal<M, C> = Journal<M, C, MemoryBackend>;
pub type FileJournal<M, C> = Journal<M, C, FileBackend>;

/// A single coordinator owns commit order; tokens own bounded reservations.
pub struct Journal<M, C, B> {
    binding: Binding,
    accepted: CommitSeq,
    durable: DurableSeq,
    codec: C,
    config: JournalConfig,
    backend: B,
    pool: Arc<ReservationPool>,
    pending: VecDeque<Pending>,
    stopped: Option<Fault>,
    model: PhantomData<fn() -> M>,
    profiler: SerialProfiler,
}

impl<M: AuthorityData, C: StorageCodec<M>, B: JournalBackend> Journal<M, C, B> {
    /// Start at a trusted, already durable checkpoint/log position.
    ///
    /// Custom backends must be exclusively owned and positioned after the validated prefix
    /// described by `baseline`. Filesystem callers use `FileJournal::create` or `open`.
    /// This constructor also permits deterministic append/sync fault injection in tests.
    pub fn with_backend(
        binding: Binding,
        baseline: CommitCursor,
        codec: C,
        config: JournalConfig,
        backend: B,
    ) -> Result<Self, Fault> {
        validate_baseline(binding, baseline, config)?;
        let mut pending = VecDeque::new();
        pending
            .try_reserve_exact(config.max_pending_records as usize)
            .map_err(|error| Fault::resource_limit(format!("reserve journal slots: {error}")))?;
        Ok(Self {
            binding,
            accepted: baseline.seq,
            durable: DurableSeq(baseline.seq.0),
            codec,
            config,
            backend,
            pool: Arc::new(ReservationPool {
                records: AtomicU32::new(0),
                bytes: AtomicU64::new(0),
                staged: AtomicBool::new(false),
            }),
            pending,
            stopped: None,
            model: PhantomData,
            profiler: SerialProfiler::default(),
        })
    }

    /// Read-only access cannot bypass ordering by appending or truncating the backend.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Use the host's recorder to include domain encoding, framing, and checksum.
    pub fn set_serial_profiler(&mut self, profiler: SerialProfiler) {
        self.profiler = profiler;
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.is_some()
    }

    /// Counts accepted entries and the outstanding staged token, if any.
    pub fn pending_usage(&self) -> (u32, u64) {
        (
            self.pool.records.load(Ordering::Acquire),
            self.pool.bytes.load(Ordering::Acquire),
        )
    }

    fn check_running(&self) -> Result<(), Fault> {
        match &self.stopped {
            Some(error) => Err(Fault::new(error.code, error.context.clone())),
            None => Ok(()),
        }
    }

    fn stop(&mut self, fault: Fault) -> Fault {
        self.stopped = Some(Fault::new(fault.code, fault.context.clone()));
        // Release queued buffers. Never retry a potentially partial write in this instance;
        // only a new recovery scan can determine the surviving complete prefix.
        self.pending.clear();
        fault
    }
}

impl<M: AuthorityData, C: StorageCodec<M>> MemoryJournal<M, C> {
    pub fn memory(
        binding: Binding,
        baseline: CommitCursor,
        codec: C,
        config: JournalConfig,
        max_retained_bytes: u64,
    ) -> Result<Self, Fault> {
        Self::with_backend(
            binding,
            baseline,
            codec,
            config,
            MemoryBackend::new(max_retained_bytes),
        )
    }
}

impl<M: AuthorityData, C: StorageCodec<M>> FileJournal<M, C> {
    /// Create without overwriting an existing journal. The caller persists the initial
    /// checkpoint and directory entry before opening authority admission.
    pub fn create(
        path: impl AsRef<Path>,
        binding: Binding,
        baseline: CommitCursor,
        codec: C,
        config: JournalConfig,
    ) -> Result<Self, Fault> {
        validate_baseline(binding, baseline, config)?;
        let path = path.as_ref();
        if segments::root(path)
            .try_exists()
            .map_err(|e| io_fault("inspect journal segment namespace", e))?
        {
            return Err(Fault::invalid_data(
                "journal segment namespace already exists",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| io_fault("create journal", error))?;
        file.try_lock()
            .map_err(|error| io_fault("lock journal", error))?;
        Self::with_backend(
            binding,
            baseline,
            codec,
            config,
            FileBackend {
                file,
                path: std::fs::canonicalize(path)
                    .map_err(|e| io_fault("resolve journal path", e))?,
                baseline,
                segments: None,
            },
        )
    }

    /// Validate everything before modification, then remove only an incomplete EOF tail.
    ///
    /// Corruption, gaps, and compatibility mismatches leave the file untouched. Reopening
    /// syncs the surviving prefix before reporting durability. Restoring domain state remains
    /// a separate operation through the authority's independent recovery builder.
    pub fn open(
        path: impl AsRef<Path>,
        binding: Binding,
        baseline: CommitCursor,
        codec: C,
        config: JournalConfig,
    ) -> Result<(Self, RecoveryReport), Fault> {
        Self::open_from_checkpoint(path, binding, baseline, baseline, codec, config)
    }

    /// Reopen a legacy baseline or a segmented suffix covered by a trusted checkpoint.
    pub fn open_from_checkpoint(
        path: impl AsRef<Path>,
        binding: Binding,
        baseline: CommitCursor,
        checkpoint: CommitCursor,
        codec: C,
        config: JournalConfig,
    ) -> Result<(Self, RecoveryReport), Fault> {
        validate_baseline(binding, baseline, config)?;
        let path = path.as_ref();
        segments::regular(path, false)?;
        let resolved =
            std::fs::canonicalize(path).map_err(|e| io_fault("resolve journal path", e))?;
        let path = resolved.as_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| io_fault("open journal", error))?;
        file.try_lock()
            .map_err(|error| io_fault("lock journal", error))?;
        let mut backend = FileBackend {
            file,
            path: path.to_owned(),
            baseline,
            segments: None,
        };
        let inspected = segments::inspect(
            (path, &mut backend.file),
            binding,
            baseline,
            checkpoint,
            &codec,
            config,
            |_| Ok(()),
        )?;
        let report = inspected.report;
        if let Some(anchor) = inspected.anchor {
            segments::install_marker(&mut backend.file, anchor)?;
            let active = if let Some(last) = inspected
                .files
                .last()
                .filter(|_| inspected.last_through >= anchor.floor)
            {
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&last.path)
                    .map_err(|e| io_fault("open active journal segment", e))?;
                if report.incomplete_tail {
                    file.set_len(inspected.last_valid)
                        .map_err(|e| io_fault("repair final segment", e))?;
                }
                file.sync_all()
                    .map_err(|e| io_fault("sync recovered segment", e))?;
                let mut file = file;
                file.seek(SeekFrom::End(0))
                    .map_err(|e| io_fault("seek active segment", e))?;
                Some((
                    file,
                    if report.incomplete_tail {
                        inspected.last_valid
                    } else {
                        last.length
                    },
                ))
            } else {
                None
            };
            backend.baseline = anchor.base;
            backend.segments = Some(segments::SegmentWriter {
                root: segments::root(path),
                anchor,
                active,
                files: inspected.files.len() as u32,
                next: report
                    .through
                    .seq
                    .0
                    .checked_add(1)
                    .ok_or_else(|| Fault::resource_limit("journal sequence exhausted"))?,
                config,
            });
            segments::sync_directory(&segments::root(path))?;
        } else if report.incomplete_tail {
            backend
                .file
                .set_len(report.valid_bytes)
                .map_err(|error| io_fault("truncate incomplete journal tail", error))?;
        }
        backend
            .file
            .sync_all()
            .map_err(|error| io_fault("sync recovered journal", error))?;
        backend
            .file
            .seek(SeekFrom::End(0))
            .map_err(|error| io_fault("seek journal end", error))?;
        let journal = Self::with_backend(binding, report.through, codec, config, backend)?;
        Ok((journal, report))
    }

    /// Read the retained recovery suffix for host-owned reference tracing. No repair occurs.
    pub fn visit_retained(
        &mut self,
        replay: impl FnMut(ModelCommit<M>) -> Result<(), Fault>,
    ) -> Result<RecoveryReport, Fault> {
        self.check_running()?;
        let result = segments::inspect(
            (&self.backend.path, &mut self.backend.file),
            self.binding,
            self.backend.baseline,
            CommitCursor {
                world: self.binding.world,
                seq: self.accepted,
            },
            &self.codec,
            self.config,
            replay,
        )
        .map(|inspection| inspection.report);
        self.backend
            .file
            .seek(SeekFrom::End(0))
            .map_err(|e| io_fault("restore journal append position", e))?;
        result
    }
}

impl<M: AuthorityData, C: StorageCodec<M>, B: JournalBackend> CommitJournal<M>
    for Journal<M, C, B>
{
    type Staged = Staged;

    fn reclaim(&mut self, floor: CommitCursor) -> Result<(), Fault> {
        self.check_running()?;
        if floor.world != self.binding.world || floor.seq.0 > self.durable.0 {
            return Err(Fault::invalid_data(
                "journal retention floor is not durable in this world",
            ));
        }
        if self.pending_usage() != (0, 0) {
            return Err(Fault::new(
                FaultCode::InvalidState,
                "flush and release all journal reservations before reclamation",
            ));
        }
        let durable = CommitCursor {
            world: self.binding.world,
            seq: CommitSeq(self.durable.0),
        };
        if let Err(error) = self.backend.reclaim(floor, durable, self.config) {
            return Err(self.stop(error));
        }
        Ok(())
    }

    fn try_stage(&mut self, value: &ModelCommit<M>) -> Result<Staged, StageError> {
        self.check_running().map_err(StageError::Stopped)?;
        validate_record::<M>(self.binding, self.accepted, value)
            .map_err(StageError::InvalidRecord)?;
        let (used_records, used_bytes) = self.pending_usage();
        if self.pool.staged.load(Ordering::Acquire)
            || used_records >= self.config.max_pending_records
        {
            return Err(StageError::Busy);
        }

        // The codec must honor the budget while growing output. The final check additionally
        // catches a faulty codec that returns an oversized successful result.
        let budget = self.config.encode_budget();
        let mut encoding = self.profiler.span(Stage::Encoding);
        let mut payload = Vec::new();
        self.codec
            .encode_commit(value, &mut payload, budget)
            .map_err(|error| match error.code {
                FaultCode::ResourceLimit => StageError::TooLarge,
                _ => StageError::InvalidRecord(error),
            })?;
        if payload.len() as u64 > budget.max_bytes {
            return Err(StageError::TooLarge);
        }
        let total = payload.len() as u64 + record::OVERHEAD_BYTES as u64;
        if total > self.config.max_pending_bytes - used_bytes {
            return Err(StageError::Busy);
        }
        let total_size = usize::try_from(total).map_err(|_| StageError::TooLarge)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(total_size).map_err(|error| {
            StageError::InvalidRecord(Fault::resource_limit(format!("reserve record: {error}")))
        })?;
        let mut out = Encoder::new(
            &mut bytes,
            OutputBudget::new(1, total),
            self.config.codec_limits,
        );
        record::write_record(&mut out, RecordKind::Commit, &payload)
            .map_err(StageError::InvalidRecord)?;
        out.finish();
        encoding.set_bytes(total);
        drop(encoding);

        self.pool.records.fetch_add(1, Ordering::AcqRel);
        self.pool.bytes.fetch_add(total, Ordering::AcqRel);
        self.pool.staged.store(true, Ordering::Release);
        Ok(Staged {
            pending: Pending {
                bytes,
                cursor: value.meta.cursor,
                reservation: Reservation {
                    pool: Arc::clone(&self.pool),
                    bytes: total,
                    staged: true,
                },
            },
            binding: self.binding,
            previous: value.meta.previous,
        })
    }

    fn accept(&mut self, mut staged: Staged) -> Result<(), Fault> {
        self.check_running()?;
        if !Arc::ptr_eq(&self.pool, &staged.pending.reservation.pool)
            || staged.binding != self.binding
            || staged.previous != self.accepted
            || staged.pending.cursor.seq.follows(self.accepted).is_err()
        {
            return Err(self.stop(Fault::new(
                FaultCode::InvalidState,
                "staged token does not belong to this journal and next commit position",
            )));
        }
        // The queue slot was allocated at construction and leased by try_stage. This move
        // performs neither encoding nor allocation and cannot fail with ordinary backpressure.
        staged.pending.reservation.staged = false;
        self.pool.staged.store(false, Ordering::Release);
        self.accepted = staged.pending.cursor.seq;
        self.pending.push_back(staged.pending);
        Ok(())
    }

    fn flush_through(&mut self, target: CommitCursor) -> Result<Durability, Fault> {
        self.check_running()?;
        if target.world != self.binding.world || target.seq > self.accepted {
            return Err(Fault::invalid_data(
                "flush target is not an accepted world position",
            ));
        }
        if target.seq.0 <= self.durable.0 {
            return Ok(self.durability());
        }
        while self
            .pending
            .front()
            .is_some_and(|next| next.cursor.seq <= target.seq)
        {
            let next = self.pending.pop_front().expect("front was present");
            if let Err(error) = self.backend.append(&next.bytes) {
                return Err(self.stop(io_fault("append journal record", error)));
            }
        }
        if let Err(error) = self.backend.sync() {
            return Err(self.stop(io_fault("sync journal records", error)));
        }
        // Only a successful barrier advances the confirmed position. Write/sync failures
        // keep the last known durable prefix and permanently stop this storage instance.
        self.durable = DurableSeq(target.seq.0);
        Ok(self.durability())
    }

    fn queue_diagnostics(
        &self,
        config: &torse_types::QueueDiagnostics,
    ) -> torse_types::Diagnostic<torse_types::JournalQueues> {
        if let Err(reason) = config.check::<torse_types::JournalQueues>() {
            return torse_types::Diagnostic::Unavailable(reason);
        }
        let (records, bytes) = self.pending_usage();
        torse_types::Diagnostic::Available(torse_types::JournalQueues {
            reserved_records: u64::from(records),
            reserved_encoded_bytes: bytes,
            accepted: CommitCursor {
                world: self.binding.world,
                seq: self.accepted,
            },
        })
    }

    fn durability(&self) -> Durability {
        Durability {
            binding: self.binding,
            through: self.durable,
        }
    }
}

fn validate_baseline(
    binding: Binding,
    baseline: CommitCursor,
    config: JournalConfig,
) -> Result<(), Fault> {
    config.validate()?;
    if baseline.world != binding.world {
        return Err(Fault::invalid_data(
            "journal baseline belongs to another world",
        ));
    }
    Ok(())
}

pub(crate) fn validate_record<M: AuthorityData>(
    binding: Binding,
    previous: CommitSeq,
    value: &ModelCommit<M>,
) -> Result<(), Fault> {
    if value.meta.cursor.world != binding.world {
        return Err(Fault::invalid_data("record belongs to another world"));
    }
    if value.meta.compatibility != binding.compatibility {
        return Err(Fault::new(
            FaultCode::Incompatible,
            "record compatibility mismatch",
        ));
    }
    if value.meta.previous != previous || value.meta.cursor.seq.follows(previous).is_err() {
        return Err(Fault::new(
            FaultCode::SequenceGap,
            "record is not the next complete commit",
        ));
    }
    Ok(())
}

pub(crate) fn io_fault(operation: &str, error: impl std::fmt::Display) -> Fault {
    Fault::new(
        FaultCode::StorageUnavailable,
        format!("{operation}: {error}"),
    )
}
