//! Local host adapters; these do not redefine A's shared world exchange contract.

use torse_types::{
    Admission, AuthorityData, Binding, CommitCursor, Fault, FaultCode, ModelCommit, ModelRequest,
    ModelSnapshot, OutputBudget, Publication, ResumePoint, SubscriptionId, Tick,
};

/// Preparation owns the complete immutable record, not merely its metadata. Dropping it
/// must preserve queued input, scheduled events, sequence numbers, and persistent IDs.
pub trait PreparedChange {
    type Model: AuthorityData;
    fn record(&self) -> &ModelCommit<Self::Model>;
}

/// The server owns one adapter and never exposes mutable world access to its callers.
///
/// Preparation/publication must not perform file or network I/O. Returned preparations must
/// own their data rather than retaining a world lock across staging or synchronization.
/// `publish` errors are terminal invariant failures, never recoverable gameplay rejection.
/// The adapter remains responsible for bounded domain queues and complete world capture.
pub trait HostAuthority: Sized {
    /// Observe existing ledgers without I/O, clocks, allocation or state changes.
    fn queue_diagnostics(
        &self,
        config: &torse_types::QueueDiagnostics,
    ) -> torse_types::Diagnostic<torse_types::AuthorityQueues> {
        if let Err(reason) = config.check::<torse_types::AuthorityQueues>() {
            return torse_types::Diagnostic::Unavailable(reason);
        }
        torse_types::Diagnostic::Unavailable(torse_types::Unavailable::NotSupported)
    }
    type Model: AuthorityData;
    type Prepared: PreparedChange<Model = Self::Model>;

    fn binding(&self) -> Binding;
    fn cursor(&self) -> CommitCursor;
    fn resume_point(&self) -> ResumePoint;
    /// Interpret a proposed resume point using the locked phase graph. This must identify
    /// the start of a tick, before any work in that tick, including the initial step.
    fn is_tick_start(&self, point: ResumePoint) -> bool;
    fn at_tick_boundary(&self) -> bool {
        self.is_tick_start(self.resume_point())
    }
    fn submit(&mut self, request: ModelRequest<Self::Model>) -> Admission;
    fn prepare_next(&mut self) -> Result<Option<Self::Prepared>, Fault>;
    fn publish(&mut self, prepared: Self::Prepared) -> Result<Publication, Fault>;
    fn capture_snapshot(
        &mut self,
        budget: OutputBudget,
    ) -> Result<ModelSnapshot<Self::Model>, Fault>;

    /// Reject future submissions immediately. Settle already-admitted input through deadline
    /// and reject later input with Stopping. Do not execute unbounded future machine events.
    fn stop_admission(&mut self, deadline: Tick) -> Result<(), Fault>;

    /// True only after eligible input is settled and the authority reaches a tick boundary.
    /// Intermediate tick/event progress required for recovery must itself produce records.
    fn shutdown_ready(&self) -> bool;
}

/// Build an isolated world. Errors discard the entire builder before admission opens.
pub trait RecoveryTarget: Sized {
    type Model: AuthorityData;
    type Authority: HostAuthority<Model = Self::Model>;
    fn restore_snapshot(&mut self, snapshot: ModelSnapshot<Self::Model>) -> Result<(), Fault>;
    fn replay(&mut self, record: ModelCommit<Self::Model>) -> Result<(), Fault>;
    fn finish(self, expected: CommitCursor) -> Result<Self::Authority, Fault>;
}

/// Invalidation is a control operation, independent of ordinary replica data queues. It must
/// purge/revoke the listed subscriptions before any new external output can be released.
/// An error stops the world even if authority publication has already happened.
pub trait InvalidationSink {
    fn invalidate(
        &mut self,
        binding: Binding,
        subscriptions: &[SubscriptionId],
    ) -> Result<(), Fault>;
}

/// A host without replicas must reject unexpected invalidations instead of ignoring them.
pub struct NoReplicas;

impl InvalidationSink for NoReplicas {
    fn invalidate(&mut self, _: Binding, subscriptions: &[SubscriptionId]) -> Result<(), Fault> {
        if !subscriptions.is_empty() {
            return Err(Fault::new(
                FaultCode::InvalidState,
                "replica invalidation has no handler",
            ));
        }
        Ok(())
    }
}
