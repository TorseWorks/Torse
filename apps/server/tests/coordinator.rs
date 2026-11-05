//! Host sequencing is observed across the authority, journal, and invalidation boundaries.

mod support;

use support::*;
use torse_server::*;
use torse_types::*;

#[test]
fn queue_diagnostics_keep_custom_adapter_gaps_explicit() {
    let fixture = fixture();
    fixture.control.reset();
    let Diagnostic::Available(snapshot) = fixture
        .server
        .queue_diagnostics(&QueueDiagnostics::enabled(4096).unwrap())
    else {
        panic!("server snapshot unavailable")
    };
    assert_eq!(
        snapshot.authority,
        Diagnostic::Unavailable(Unavailable::NotSupported)
    );
    assert_eq!(
        snapshot.journal,
        Diagnostic::Unavailable(Unavailable::NotSupported)
    );
    assert_eq!(
        snapshot.progress.accepted,
        Diagnostic::Unavailable(Unavailable::NotSupported)
    );
    assert_eq!(snapshot.progress.published.seq, CommitSeq(0));
    assert_eq!(snapshot.progress.durable.through, DurableSeq(0));
    // Existing progress remains readable without asking an adapter to flush or prepare.
    assert!(fixture.control.trace.borrow().is_empty());
}

fn admit(fixture: &mut Fixture, sequence: u64, target: u64, closes: bool) {
    let binding = fixture.server.authority().unwrap().binding();
    assert!(matches!(
        fixture
            .server
            .submit(request(binding, sequence, target, closes)),
        Admission::Queued { .. }
    ));
}

#[test]
fn initial_checkpoint_precedes_admission_and_the_directory_stays_locked() {
    let mut fixture = fixture();
    assert_eq!(
        &*fixture.control.trace.borrow(),
        &["flush", "capture", "prune", "checkpoint"]
    );
    assert!(WorldDirectory::acquire(&fixture.directory.0, WORLD, 8192).is_err());
    admit(&mut fixture, 1, 0, true);
    assert_eq!(fixture.server.authority().unwrap().value, 0);
}

#[test]
fn publication_accept_invalidation_and_durability_follow_the_contract_order() {
    let mut fixture = fixture();
    admit(&mut fixture, 1, 0, true);
    fixture.control.invalidations.set(true);
    fixture.control.reset();
    let step = fixture
        .server
        .drive_one(&mut Sink(fixture.control.clone()))
        .unwrap();
    let CommitStep::Durable(confirmed) = step else {
        panic!("durable publication expected")
    };
    assert_eq!(confirmed.publication.at.seq, CommitSeq(1));
    assert_eq!(confirmed.durability.through, DurableSeq(1));
    assert_eq!(
        &*fixture.control.trace.borrow(),
        &[
            "prepare",
            "stage",
            "publish",
            "accept",
            "invalidate",
            "flush",
            "append",
            "sync"
        ]
    );
    assert_eq!(fixture.server.authority().unwrap().value, 1);
    assert_eq!(fixture.server.flush_metrics().calls, 2);
}

#[test]
fn staging_rejection_preserves_input_world_and_sequence_for_revalidation() {
    for failure in [
        Failure::StageBusy,
        Failure::StageLarge,
        Failure::StageInvalid,
    ] {
        let mut fixture = fixture();
        admit(&mut fixture, 1, 0, true);
        fixture.control.failure.set(failure);
        fixture.control.reset();
        assert!(matches!(
            fixture.server.drive_one(&mut NoReplicas).unwrap(),
            CommitStep::Blocked(_)
        ));
        assert_eq!(&*fixture.control.trace.borrow(), &["prepare", "stage"]);
        assert_eq!(fixture.server.authority().unwrap().value, 0);
        assert_eq!(
            fixture.server.authority().unwrap().cursor().seq,
            CommitSeq(0)
        );
        fixture.control.failure.set(Failure::None);
        assert!(matches!(
            fixture.server.drive_one(&mut NoReplicas).unwrap(),
            CommitStep::Durable(_)
        ));
        assert_eq!(fixture.server.authority().unwrap().value, 1);
        assert_eq!(
            fixture.server.authority().unwrap().cursor().seq,
            CommitSeq(1)
        );
    }
}

#[test]
fn terminal_failures_never_release_confirmation_or_allow_further_world_access() {
    for failure in [
        Failure::Prepare,
        Failure::Publish,
        Failure::StageStopped,
        Failure::Accept,
        Failure::Append,
        Failure::Sync,
        Failure::Invalidate,
        Failure::BadBinding,
        Failure::ShortBarrier,
        Failure::AheadBarrier,
        Failure::BadPublication,
        Failure::MutatingPrepare,
    ] {
        let mut fixture = fixture();
        let binding = fixture.server.authority().unwrap().binding();
        admit(&mut fixture, 1, 0, true);
        fixture.control.failure.set(failure);
        assert!(
            fixture
                .server
                .drive_one(&mut Sink(fixture.control.clone()))
                .is_err(),
            "{failure:?}"
        );
        assert_eq!(fixture.server.state(), ServerState::Faulted);
        assert!(fixture.server.authority().is_err());
        assert!(fixture.server.drive_one(&mut NoReplicas).is_err());
        assert!(fixture.server.checkpoint().is_err());
        assert_eq!(
            fixture.server.submit(request(binding, 2, 1, true)),
            Admission::Rejected(RejectCode::Stopping)
        );
        assert!(WorldDirectory::acquire(&fixture.directory.0, WORLD, 8192).is_err());
    }
}

#[test]
fn append_and_sync_failure_keep_the_last_confirmed_position() {
    for failure in [Failure::Append, Failure::Sync] {
        let mut fixture = fixture();
        admit(&mut fixture, 1, 0, true);
        fixture.control.failure.set(failure);
        assert!(fixture.server.drive_one(&mut NoReplicas).is_err());
        assert_eq!(
            fixture.control.value.get(),
            1,
            "the live world is not partially rolled back"
        );
        assert_eq!(fixture.server.durability().through, DurableSeq(0));
    }
}

#[test]
fn missing_invalidation_handler_stops_the_world_before_a_flush() {
    let mut fixture = fixture();
    admit(&mut fixture, 1, 0, true);
    fixture.control.invalidations.set(true);
    fixture.control.reset();
    assert!(fixture.server.drive_one(&mut NoReplicas).is_err());
    assert_eq!(fixture.server.durability().through, DurableSeq(0));
    assert!(!fixture.control.trace.borrow().contains(&"flush"));
}

#[test]
fn invalidation_budget_is_enforced_as_a_terminal_invariant() {
    let config = ServerConfig {
        max_invalidations: 0,
        ..server_config()
    };
    let mut fixture = fixture_with(config, Control::new()).unwrap();
    admit(&mut fixture, 1, 0, true);
    fixture.control.invalidations.set(true);
    assert!(
        fixture
            .server
            .drive_one(&mut Sink(fixture.control.clone()))
            .is_err()
    );
    assert_eq!(fixture.server.state(), ServerState::Faulted);
}

#[test]
fn foreign_world_and_old_epoch_are_rejected_before_authority_admission() {
    let mut fixture = fixture();
    let binding = fixture.server.authority().unwrap().binding();
    fixture.control.reset();
    for foreign_world in [true, false] {
        let mut value = request(binding, 1, 0, true);
        if foreign_world {
            value.id.world = WorldId(99);
        } else {
            value.id.epoch = WorldEpoch(99);
        }
        assert_eq!(
            fixture.server.submit(value),
            Admission::Rejected(RejectCode::StaleSession)
        );
    }
    assert!(fixture.control.trace.borrow().is_empty());
    assert_eq!(fixture.server.authority().unwrap().value, 0);
}

#[test]
fn checkpoint_requires_a_tick_boundary_and_flushes_before_capture() {
    let mut fixture = fixture();
    admit(&mut fixture, 1, 0, false);
    fixture.server.drive_one(&mut NoReplicas).unwrap();
    assert!(fixture.server.checkpoint().is_err());
    assert_eq!(fixture.server.state(), ServerState::Running);
    admit(&mut fixture, 2, 0, true);
    fixture.server.drive_one(&mut NoReplicas).unwrap();
    fixture.control.reset();
    fixture.server.checkpoint().unwrap();
    assert_eq!(
        &*fixture.control.trace.borrow(),
        &["flush", "capture", "prune", "checkpoint"]
    );
}

#[test]
fn capture_and_checkpoint_failure_never_report_saved_state() {
    for failure in [
        Failure::Capture,
        Failure::Checkpoint,
        Failure::BadSnapshot,
        Failure::MutatingCapture,
    ] {
        let mut fixture = fixture();
        fixture.control.failure.set(failure);
        assert!(fixture.server.checkpoint().is_err());
        assert_eq!(fixture.server.state(), ServerState::Faulted);
    }
}

#[test]
fn failed_initial_checkpoint_never_returns_a_running_server() {
    for failure in [Failure::Capture, Failure::Checkpoint] {
        let control = Control::new();
        control.failure.set(failure);
        assert!(fixture_with(server_config(), control.clone()).is_err());
        assert!(!control.trace.borrow().contains(&"submit"));
    }
}

#[test]
fn shutdown_drains_only_through_cutoff_then_persists_a_tick_boundary() {
    let mut fixture = fixture();
    admit(&mut fixture, 1, 0, false);
    admit(&mut fixture, 2, 1000, true);
    fixture.control.reset();
    let report = fixture.server.shutdown(Tick(0), &mut NoReplicas).unwrap();
    assert_eq!(fixture.control.canceled.get(), 1);
    assert_eq!(fixture.control.value.get(), 1);
    assert_eq!(
        report.through.seq,
        CommitSeq(2),
        "tick completion also has a durable record"
    );
    assert_eq!(report.committed_during_shutdown, 2);
    assert_eq!(report.durability.through, DurableSeq(2));
    assert_eq!(
        fixture.control.trace.borrow().first(),
        Some(&"stop-admission")
    );
    let trace = fixture.control.trace.borrow();
    assert_eq!(
        &trace[trace.len() - 4..],
        &["flush", "capture", "prune", "checkpoint"]
    );
    drop(trace);
    assert!(WorldDirectory::acquire(&fixture.directory.0, WORLD, 8192).is_ok());
}

#[test]
fn shutdown_failure_is_finite_and_releases_the_directory_lease() {
    for failure in [
        Failure::StageBusy,
        Failure::StageLarge,
        Failure::Stall,
        Failure::Sync,
        Failure::Checkpoint,
    ] {
        let mut fixture = fixture();
        admit(&mut fixture, 1, 0, false);
        fixture.control.failure.set(failure);
        assert!(
            fixture.server.shutdown(Tick(0), &mut NoReplicas).is_err(),
            "{failure:?}"
        );
        assert!(WorldDirectory::acquire(&fixture.directory.0, WORLD, 8192).is_ok());
    }
}

#[test]
fn invalid_shutdown_deadline_does_not_publish_a_checkpoint() {
    let fixture = fixture();
    fixture.control.reset();
    assert!(
        fixture
            .server
            .shutdown(Tick(u64::MAX), &mut NoReplicas)
            .is_err()
    );
    assert!(fixture.control.trace.borrow().is_empty());
}

#[test]
fn a_faulty_adapter_cannot_publish_future_input_past_the_shutdown_cutoff() {
    let mut fixture = fixture();
    admit(&mut fixture, 1, 999, true);
    fixture.control.failure.set(Failure::IgnoreCutoff);
    assert!(fixture.server.shutdown(Tick(0), &mut NoReplicas).is_err());
    assert_eq!(fixture.control.value.get(), 0);
}

#[test]
fn shutdown_rejects_next_tick_intermediate_phases_and_nonzero_steps_before_publication() {
    for failure in [Failure::IgnoreCutoff, Failure::BoundaryStep] {
        let mut fixture = fixture();
        let (target, closes_tick) = if failure == Failure::IgnoreCutoff {
            (1, false)
        } else {
            (0, true)
        };
        admit(&mut fixture, 1, target, closes_tick);
        fixture.control.failure.set(failure);
        fixture.control.reset();
        assert!(fixture.server.shutdown(Tick(0), &mut NoReplicas).is_err());
        assert_eq!(fixture.control.value.get(), 0);
        let trace = fixture.control.trace.borrow();
        for forbidden in ["publish", "accept", "append", "sync"] {
            assert!(!trace.contains(&forbidden), "{failure:?}: {forbidden}");
        }
    }
}

#[test]
fn healthy_checkpoint_quota_is_retryable_but_partial_write_failure_is_terminal() {
    let mut fixture = fixture();
    fixture.control.failure.set(Failure::CheckpointQuota);
    assert_eq!(
        fixture.server.checkpoint().unwrap_err().code,
        FaultCode::ResourceLimit
    );
    assert_eq!(fixture.server.state(), ServerState::Running);
    assert!(fixture.server.authority().is_ok());
    assert!(fixture.server.prune_checkpoints(0).is_err());
    assert_eq!(fixture.server.state(), ServerState::Running);
    let retention = fixture.server.prune_checkpoints(1).unwrap();
    assert_eq!(retention.log_floor.unwrap().seq, CommitSeq(0));
    fixture.control.failure.set(Failure::None);
    fixture.server.checkpoint().unwrap();
    fixture
        .control
        .failure
        .set(Failure::TerminalCheckpointQuota);
    assert_eq!(
        fixture.server.checkpoint().unwrap_err().code,
        FaultCode::ResourceLimit
    );
    assert_eq!(fixture.server.state(), ServerState::Faulted);
    assert!(fixture.server.prune_checkpoints(1).is_err());
}
