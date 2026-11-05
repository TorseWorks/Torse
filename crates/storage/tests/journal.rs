//! Behavioral tests shared by memory/file journals and deterministic I/O failures.

mod support;

use std::io::Cursor;

use support::*;
use torse_storage::{CommitJournal, FileJournal, Journal, MemoryJournal, scan_journal};
use torse_types::*;

type Memory = MemoryJournal<TestModel, TestCodec>;
type Disk = FileJournal<TestModel, TestCodec>;

fn memory() -> Memory {
    Memory::memory(binding(), baseline(), TestCodec::new(), config(), 65536).unwrap()
}

fn available<T>(value: Diagnostic<T>) -> T {
    match value {
        Diagnostic::Available(value) => value,
        Diagnostic::Unavailable(reason) => panic!("{reason:?}"),
    }
}

#[test]
fn queue_diagnostics_track_staging_cancellation_acceptance_and_failed_durability() {
    let enabled = QueueDiagnostics::enabled(4096).unwrap();
    let mut journal = Journal::<TestModel, _, _>::with_backend(
        binding(),
        baseline(),
        TestCodec::new(),
        config(),
        FaultyBackend {
            fail_sync_call: Some(1),
            ..FaultyBackend::default()
        },
    )
    .unwrap();
    assert_eq!(
        journal.queue_diagnostics(&QueueDiagnostics::default()),
        Diagnostic::Unavailable(Unavailable::NotEnabled)
    );
    assert_eq!(
        journal.queue_diagnostics(&QueueDiagnostics::enabled(1).unwrap()),
        Diagnostic::Unavailable(Unavailable::BudgetUnavailable)
    );
    let token = journal.try_stage(&commit(1)).unwrap();
    let staged = available(journal.queue_diagnostics(&enabled));
    assert_eq!(
        (
            staged.reserved_records,
            staged.reserved_encoded_bytes,
            staged.accepted
        ),
        (1, encoded(&commit(1)).len() as u64, baseline())
    );
    drop(token);
    assert_eq!(
        available(journal.queue_diagnostics(&enabled)).reserved_records,
        0
    );
    let token = journal.try_stage(&commit(1)).unwrap();
    journal.accept(token).unwrap();
    assert_eq!(
        available(journal.queue_diagnostics(&enabled)).accepted,
        cursor(1)
    );
    assert_eq!(journal.durability().through.0, 0);
    for _ in 0..1000 {
        assert_eq!(
            available(journal.queue_diagnostics(&enabled)).reserved_records,
            1
        );
    }
    assert_eq!(
        (journal.backend().append_calls, journal.backend().sync_calls),
        (0, 0)
    );
    assert!(journal.flush_through(cursor(1)).is_err());
    let failed = available(journal.queue_diagnostics(&enabled));
    assert_eq!(
        (
            failed.reserved_records,
            failed.reserved_encoded_bytes,
            failed.accepted
        ),
        (0, 0, cursor(1))
    );
    assert_eq!(
        journal.durability().through.0,
        0,
        "observation must not confirm failed sync"
    );
    assert_eq!(
        (journal.backend().append_calls, journal.backend().sync_calls),
        (1, 1)
    );
}

#[test]
fn stage_and_accept_perform_no_io_and_only_sync_advances_durability() {
    let mut journal = Journal::<TestModel, _, _>::with_backend(
        binding(),
        baseline(),
        TestCodec::new(),
        config(),
        FaultyBackend::default(),
    )
    .unwrap();
    for sequence in 1..=3 {
        let token = journal.try_stage(&commit(sequence)).unwrap();
        journal.accept(token).unwrap();
    }
    assert_eq!(journal.backend().append_calls, 0);
    assert_eq!(journal.backend().sync_calls, 0);
    assert_eq!(journal.durability().through, DurableSeq(0));
    assert_eq!(
        journal.flush_through(cursor(1)).unwrap().through,
        DurableSeq(1)
    );
    assert_eq!(journal.pending_usage().0, 2);
    assert_eq!(journal.backend().bytes, encoded(&commit(1)));
    assert_eq!(
        journal.flush_through(cursor(3)).unwrap().through,
        DurableSeq(3)
    );
    assert_eq!(journal.pending_usage(), (0, 0));
    let syncs = journal.backend().sync_calls;
    assert_eq!(
        journal.flush_through(cursor(2)).unwrap().through,
        DurableSeq(3)
    );
    assert_eq!(journal.backend().sync_calls, syncs);
}

#[test]
fn dropping_a_token_releases_capacity_and_the_same_position_can_be_retried() {
    let mut journal = memory();
    let token = journal.try_stage(&commit(1)).unwrap();
    assert_eq!(
        journal.pending_usage(),
        (1, encoded(&commit(1)).len() as u64)
    );
    assert!(matches!(
        journal.try_stage(&commit(1)),
        Err(StageError::Busy)
    ));
    std::thread::spawn(move || drop(token)).join().unwrap();
    assert_eq!(journal.pending_usage(), (0, 0));
    assert!(journal.backend().bytes().is_empty());
    let token = journal.try_stage(&commit(1)).unwrap();
    journal.accept(token).unwrap();
    assert_eq!(
        journal.flush_through(cursor(1)).unwrap().through,
        DurableSeq(1)
    );
}

#[test]
fn queue_slot_and_byte_limits_produce_retryable_backpressure() {
    for byte_limited in [false, true] {
        let mut limits = config();
        if byte_limited {
            limits.max_pending_bytes = (encoded(&commit(1)).len() * 2 - 1) as u64;
        } else {
            limits.max_pending_records = 1;
        }
        let mut journal =
            Memory::memory(binding(), baseline(), TestCodec::new(), limits, 65536).unwrap();
        let token = journal.try_stage(&commit(1)).unwrap();
        journal.accept(token).unwrap();
        let usage = journal.pending_usage();
        assert!(matches!(
            journal.try_stage(&commit(2)),
            Err(StageError::Busy)
        ));
        assert_eq!(journal.pending_usage(), usage);
        journal.flush_through(cursor(1)).unwrap();
        let token = journal.try_stage(&commit(2)).unwrap();
        journal.accept(token).unwrap();
        journal.flush_through(cursor(2)).unwrap();
    }
}

#[test]
fn oversized_records_and_codec_errors_consume_neither_capacity_nor_sequence() {
    let mut limits = config();
    limits.codec_budget.max_bytes = 130;
    let mut journal =
        Memory::memory(binding(), baseline(), TestCodec::new(), limits, 65536).unwrap();
    let mut oversized = commit(1);
    oversized.changes = vec![1; 32];
    assert!(matches!(
        journal.try_stage(&oversized),
        Err(StageError::TooLarge)
    ));
    assert_eq!(journal.pending_usage(), (0, 0));
    let token = journal.try_stage(&commit(1)).unwrap();
    journal.accept(token).unwrap();
    journal.flush_through(cursor(1)).unwrap();

    let mut journal = Memory::memory(
        binding(),
        baseline(),
        TestCodec { fail_encode: true },
        config(),
        65536,
    )
    .unwrap();
    assert!(matches!(
        journal.try_stage(&commit(1)),
        Err(StageError::InvalidRecord(_))
    ));
    assert_eq!(journal.pending_usage(), (0, 0));
    assert_eq!(journal.durability().through, DurableSeq(0));
    assert!(!journal.is_stopped());
}

#[test]
fn staging_checks_world_compatibility_and_contiguous_positions() {
    let mut journal = memory();
    let mut wrong_world = commit(1);
    wrong_world.meta.cursor.world = WorldId(99);
    let mut incompatible = commit(1);
    incompatible.meta.compatibility.authority_schema += 1;
    for value in [wrong_world, incompatible, commit(2)] {
        assert!(matches!(
            journal.try_stage(&value),
            Err(StageError::InvalidRecord(_))
        ));
        assert_eq!(journal.pending_usage(), (0, 0));
    }
    let token = journal.try_stage(&commit(1)).unwrap();
    journal.accept(token).unwrap();
    assert!(matches!(
        journal.try_stage(&commit(1)),
        Err(StageError::InvalidRecord(_))
    ));
    assert!(journal.flush_through(cursor(2)).is_err());
    assert!(
        journal
            .flush_through(CommitCursor::initial(WorldId(99)))
            .is_err()
    );
    assert_eq!(journal.durability().through, DurableSeq(0));
    assert!(!journal.is_stopped());
}

#[test]
fn foreign_and_old_instance_tokens_are_terminal_even_with_matching_bindings() {
    for changed_epoch in [false, true] {
        let mut source = memory();
        let token = source.try_stage(&commit(1)).unwrap();
        let mut target_binding = binding();
        if changed_epoch {
            target_binding.epoch = WorldEpoch(99);
        }
        let mut target = Memory::memory(
            target_binding,
            baseline(),
            TestCodec::new(),
            config(),
            65536,
        )
        .unwrap();
        assert_eq!(
            target.accept(token).unwrap_err().code,
            FaultCode::InvalidState
        );
        assert!(target.is_stopped());
        assert!(matches!(
            target.try_stage(&commit(1)),
            Err(StageError::Stopped(_))
        ));
        assert_eq!(source.pending_usage(), (0, 0));
        assert!(!source.is_stopped());
    }
    let token = {
        let mut old = memory();
        old.try_stage(&commit(1)).unwrap()
    };
    let mut new = memory();
    assert!(new.accept(token).is_err());
    assert!(new.backend().bytes().is_empty());
}

#[test]
fn partial_append_at_every_byte_stops_without_confirming_the_failed_commit() {
    let first = encoded(&commit(1));
    let second = encoded(&commit(2));
    for cut in 0..second.len() {
        let backend = FaultyBackend {
            fail_after_bytes: Some(first.len() + cut),
            ..Default::default()
        };
        let mut journal = Journal::<TestModel, _, _>::with_backend(
            binding(),
            baseline(),
            TestCodec::new(),
            config(),
            backend,
        )
        .unwrap();
        let token = journal.try_stage(&commit(1)).unwrap();
        journal.accept(token).unwrap();
        journal.flush_through(cursor(1)).unwrap();
        let token = journal.try_stage(&commit(2)).unwrap();
        journal.accept(token).unwrap();
        assert_eq!(
            journal.flush_through(cursor(2)).unwrap_err().code,
            FaultCode::StorageUnavailable
        );
        assert!(journal.is_stopped());
        assert_eq!(journal.durability().through, DurableSeq(1));
        assert_eq!(journal.pending_usage(), (0, 0));
        assert!(journal.flush_through(cursor(2)).is_err());
        assert!(matches!(
            journal.try_stage(&commit(3)),
            Err(StageError::Stopped(_))
        ));
        let mut values = Vec::new();
        let report = scan_journal::<TestModel, _, _, _>(
            &mut Cursor::new(&journal.backend().bytes),
            binding(),
            baseline(),
            &TestCodec::new(),
            LIMITS,
            config().codec_budget,
            |value| {
                values.push(value.changes);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(report.through, cursor(1));
        assert_eq!(values, [commit(1).changes]);
        assert_eq!(report.incomplete_tail, cut != 0);
    }
}

#[test]
fn failed_sync_keeps_the_old_durable_prefix_and_stops_all_operations() {
    let backend = FaultyBackend {
        fail_sync_call: Some(2),
        ..Default::default()
    };
    let mut journal = Journal::<TestModel, _, _>::with_backend(
        binding(),
        baseline(),
        TestCodec::new(),
        config(),
        backend,
    )
    .unwrap();
    let token = journal.try_stage(&commit(1)).unwrap();
    journal.accept(token).unwrap();
    journal.flush_through(cursor(1)).unwrap();
    let token = journal.try_stage(&commit(2)).unwrap();
    journal.accept(token).unwrap();
    let still_staged = journal.try_stage(&commit(3)).unwrap();
    assert_eq!(
        journal.flush_through(cursor(2)).unwrap_err().code,
        FaultCode::StorageUnavailable
    );
    assert_eq!(journal.durability().through, DurableSeq(1));
    assert_eq!(journal.backend().bytes.len(), encoded(&commit(1)).len() * 2);
    assert!(journal.accept(still_staged).is_err());
    assert_eq!(journal.pending_usage(), (0, 0));
    assert!(journal.flush_through(cursor(1)).is_err());
}

#[test]
fn memory_retention_is_bounded_and_invalid_configuration_is_rejected() {
    let mut journal = Memory::memory(binding(), baseline(), TestCodec::new(), config(), 0).unwrap();
    let token = journal.try_stage(&commit(1)).unwrap();
    journal.accept(token).unwrap();
    assert!(journal.flush_through(cursor(1)).is_err());
    assert!(journal.is_stopped());
    assert!(journal.backend().bytes().is_empty());
    for invalid in [0, 1, 2] {
        let mut limits = config();
        match invalid {
            0 => limits.max_pending_records = 0,
            1 => limits.max_pending_bytes = 0,
            _ => limits.codec_budget.max_items = 0,
        }
        assert!(Memory::memory(binding(), baseline(), TestCodec::new(), limits, 65536).is_err());
    }
    assert!(
        Memory::memory(
            binding(),
            CommitCursor::initial(WorldId(99)),
            TestCodec::new(),
            config(),
            65536
        )
        .is_err()
    );
}

#[test]
fn the_final_sequence_is_valid_but_wraparound_can_never_be_staged() {
    let start = cursor(u64::MAX - 1);
    let mut journal = Memory::memory(binding(), start, TestCodec::new(), config(), 65536).unwrap();
    let token = journal.try_stage(&commit(u64::MAX)).unwrap();
    journal.accept(token).unwrap();
    journal.flush_through(cursor(u64::MAX)).unwrap();
    let mut wrapped = commit(1);
    wrapped.meta.previous = CommitSeq(u64::MAX);
    wrapped.meta.cursor.seq = CommitSeq(0);
    assert!(matches!(
        journal.try_stage(&wrapped),
        Err(StageError::InvalidRecord(_))
    ));
    assert_eq!(journal.durability().through, DurableSeq(u64::MAX));
}

#[test]
fn hard_frame_and_empty_queue_limits_are_too_large_instead_of_permanent_busy() {
    for limit_frame in [false, true] {
        let mut limits = config();
        if limit_frame {
            limits.codec_limits.max_frame_bytes = 124;
        } else {
            limits.max_pending_bytes = encoded(&commit(1)).len() as u64 - 1;
        }
        let mut journal =
            Memory::memory(binding(), baseline(), TestCodec::new(), limits, 65536).unwrap();
        assert!(matches!(
            journal.try_stage(&commit(1)),
            Err(StageError::TooLarge)
        ));
        assert_eq!(journal.pending_usage(), (0, 0));
        assert_eq!(journal.durability().through, DurableSeq(0));
        assert!(!journal.is_stopped());
    }
}

#[test]
fn file_journal_reopens_at_the_validated_prefix_and_continues_without_duplicates() {
    let path = TestFile::new();
    {
        let mut journal =
            Disk::create(&path.0, binding(), baseline(), TestCodec::new(), config()).unwrap();
        let token = journal.try_stage(&commit(1)).unwrap();
        journal.accept(token).unwrap();
        assert_eq!(std::fs::metadata(&path.0).unwrap().len(), 0);
        journal.flush_through(cursor(1)).unwrap();
        assert!(Disk::create(&path.0, binding(), baseline(), TestCodec::new(), config()).is_err());
        assert!(Disk::open(&path.0, binding(), baseline(), TestCodec::new(), config()).is_err());
    }
    let mut fresh = binding();
    fresh.epoch = WorldEpoch(12);
    let (mut journal, report) =
        Disk::open(&path.0, fresh, baseline(), TestCodec::new(), config()).unwrap();
    assert_eq!(report.through, cursor(1));
    assert_eq!(journal.durability().binding, fresh);
    let token = journal.try_stage(&commit(2)).unwrap();
    journal.accept(token).unwrap();
    journal.flush_through(cursor(2)).unwrap();
    drop(journal);
    let bytes = std::fs::read(&path.0).unwrap();
    assert_eq!(bytes, [encoded(&commit(1)), encoded(&commit(2))].concat());
}

#[test]
fn file_open_repairs_torn_tail_but_never_modifies_complete_corruption() {
    for cut in [1, 19, 20, 80, 150, 152] {
        let path = TestFile::new();
        let first = encoded(&commit(1));
        let second = encoded(&commit(2));
        std::fs::write(&path.0, [&first[..], &second[..cut]].concat()).unwrap();
        let (mut journal, report) =
            Disk::open(&path.0, binding(), baseline(), TestCodec::new(), config()).unwrap();
        assert!(report.incomplete_tail);
        assert_eq!(report.valid_bytes, first.len() as u64);
        assert_eq!(
            std::fs::metadata(&path.0).unwrap().len(),
            first.len() as u64
        );
        let token = journal.try_stage(&commit(2)).unwrap();
        journal.accept(token).unwrap();
        journal.flush_through(cursor(2)).unwrap();
        drop(journal);
        assert_eq!(std::fs::read(&path.0).unwrap(), [first, second].concat());
    }
    let path = TestFile::new();
    let mut bytes = [encoded(&commit(1)), encoded(&commit(2))].concat();
    bytes[30] ^= 1;
    std::fs::write(&path.0, &bytes).unwrap();
    assert!(Disk::open(&path.0, binding(), baseline(), TestCodec::new(), config()).is_err());
    assert_eq!(std::fs::read(&path.0).unwrap(), bytes);
}
