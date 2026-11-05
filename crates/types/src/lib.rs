//! Shared types for world identity, coordinates, time, memory accounting, and I/O contracts.
//!
//! This module defines the foundational types used across authority simulation,
//! storage, and network layers. Types here must remain stable across module boundaries.

#![forbid(unsafe_code)]

use core::fmt;

mod asset;
pub use asset::{AssetGuid, valid_asset_path};

mod diagnostics;
mod identity;
pub use diagnostics::{
    AuthorityQueues, Diagnostic, FrameQueue, HubQueues, JournalQueues, PeerQueues, QueueByteKind,
    QueueDiagnostics, QueueProgress, ReplicaQueues, RequestQueue, ServerQueues, TransportQueues,
    Unavailable,
};
mod lifecycle;
mod memory;
mod memory_pool;
pub use memory_pool::{MemoryLease, MemoryPool, MemoryUsage};
mod replication;
mod spatial;
mod validation;

#[cfg(test)]
mod tests;

pub use identity::{ContentKey, ContentKeyError, EntityHandle, EntityId, EntityIdError};
pub use lifecycle::{
    EventId, EventIdError, Revision, ScheduledEvent, TransactionId, TransactionIdError,
    TransactionOutcome, TransactionResult,
};
pub use memory::{
    AdmittedDemand, EvictionPlan, EvictionRank, MemoryBudget, MemoryChargeId, MemoryClass,
    MemoryLedger, MemoryResident, RetainFailure, RetainedDemandId, RetainedFailure, RetryReport,
};
pub use replication::{
    AuthorityMode, PresentationHint, ReplicationDeclaration, ReplicationVisibility, UpdateMode,
};
pub use spatial::{BlockPos, LocalPos, SectionPos, SpatialError};
pub use validation::ValidationError;

// ================================================================================================
// World Identity and Lifecycle
// ================================================================================================

/// Unique identifier for a world, stable across its lifetime.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct WorldId(pub u128);

/// World session epoch, regenerated on each load or restart.
/// Used to reject stale tasks, subscriptions, and requests from previous sessions.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct WorldEpoch(pub u128);

/// Sequential commit number for completed transactions within a world.
/// Starts at 0 for initial baseline, must be continuous.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CommitSeq(pub u64);

/// Durable sequence number marking the recoverable prefix of commits.
/// Only advances within a healthy storage instance, never exceeds CommitSeq.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DurableSeq(pub u64);

/// Simulation tick counter.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Tick(pub u64);

/// Cross-process commit position, excludes the ephemeral WorldEpoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitCursor {
    pub world: WorldId,
    pub seq: CommitSeq,
}

/// World binding including identity, session epoch, and compatibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Binding {
    pub world: WorldId,
    pub epoch: WorldEpoch,
    pub compatibility: Compatibility,
}

// ================================================================================================
// Network Session Identity
// ================================================================================================

/// Network session identifier, reassigned on reconnection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SessionId(pub u128);

/// Per-session request sequence number, starts at 1.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct RequestSeq(pub u64);

/// Host-managed identity and permission context, not client-provided.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ActorContextId(pub u128);

/// Unique subscription identifier within current WorldEpoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SubscriptionId(pub u128);

/// Sequential batch number for a single subscription, starts at 1.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ReplicaSeq(pub u64);

/// Request identifier combining world, epoch, session, and sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct RequestId {
    pub world: WorldId,
    pub epoch: WorldEpoch,
    pub session: SessionId,
    pub seq: RequestSeq,
}

// ================================================================================================
// Compatibility and Versioning
// ================================================================================================

/// Content digest, currently SHA-256 (32 bytes).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Digest(pub [u8; 32]);

/// Compatibility record for authority schema, rules, and content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Compatibility {
    pub authority_schema: u32,
    pub rules_digest: Digest,
    pub content_digest: Digest,
}

// ================================================================================================
// Resume and Recovery
// ================================================================================================

/// Resume point within a tick, identifying next phase and step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResumePoint {
    pub tick: Tick,
    pub next_phase: u16,
    pub next_step: u64,
}

/// Snapshot metadata including cursor, compatibility, and resume position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotMeta {
    pub cursor: CommitCursor,
    pub compatibility: Compatibility,
    pub resume_at: ResumePoint,
}

/// Commit metadata including cursor, previous sequence, compatibility, and resume position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitMeta {
    pub cursor: CommitCursor,
    pub previous: CommitSeq,
    pub compatibility: Compatibility,
    pub resume_at: ResumePoint,
}

// ================================================================================================
// Budget and Limits
// ================================================================================================

/// Budget for output operations, limiting item count and byte size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputBudget {
    pub max_items: u32,
    pub max_bytes: u64,
}

// ================================================================================================
// Error Codes
// ================================================================================================

/// Rejection codes for requests that cannot be processed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectCode {
    Conflict,
    NotLoaded,
    MissingTarget,
    PermissionDenied,
    QuotaExceeded,
    InvalidData,
    StaleSession,
    DuplicateMismatch,
    SequenceExpired,
    Stopping,
}

/// Fault codes for storage, encoding, and recovery errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultCode {
    InvalidData,
    Incompatible,
    CorruptData,
    SequenceGap,
    ResourceLimit,
    StorageUnavailable,
    ResetRequired,
    InvalidState,
}

/// Fault result with code and context message.
#[derive(Debug)]
pub struct Fault {
    pub code: FaultCode,
    pub context: String,
}

// ================================================================================================
// Admission and Results
// ================================================================================================

/// Admission result for submitted requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    Queued { target_tick: Tick },
    Duplicate,
    Rejected(RejectCode),
}

/// Outcome of a processed request.
#[derive(Debug)]
pub enum Outcome<O> {
    Committed { at: CommitCursor, value: O },
    ReadOnly { observed_at: CommitCursor, value: O },
    Rejected(RejectCode),
}

/// Request result pairing request ID with its outcome.
#[derive(Debug)]
pub struct RequestResult<O> {
    pub request: RequestId,
    pub outcome: Outcome<O>,
}

// ================================================================================================
// Durability
// ================================================================================================

/// Durability confirmation with binding and durable sequence position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Durability {
    pub binding: Binding,
    pub through: DurableSeq,
}

/// Stage error variants for commit journal operations.
#[derive(Debug)]
pub enum StageError {
    Busy,
    TooLarge,
    InvalidRecord(Fault),
    Stopped(Fault),
}

// ================================================================================================
// Publication
// ================================================================================================

/// Publication result including commit cursor and invalidated subscriptions.
#[derive(Debug)]
pub struct Publication {
    pub at: CommitCursor,
    pub invalidated_replicas: Vec<SubscriptionId>,
}

// ================================================================================================
// Replication
// ================================================================================================

/// Viewer identity for replica authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Viewer {
    pub session: SessionId,
    pub actor: ActorContextId,
}

// ================================================================================================
// Domain Payload Wrappers
// ================================================================================================

/// World snapshot wrapping metadata and domain-specific state.
#[derive(Debug)]
pub struct WorldSnapshot<S> {
    pub meta: SnapshotMeta,
    pub state: S,
}

/// Commit record wrapping metadata and domain-specific changes.
#[derive(Debug)]
pub struct CommitRecord<C> {
    pub meta: CommitMeta,
    pub changes: C,
}

/// World request wrapping request identity, actor, and domain input.
#[derive(Debug)]
pub struct WorldRequest<I> {
    pub id: RequestId,
    pub actor: ActorContextId,
    pub input: I,
}

/// Replica seed wrapping binding, subscription, baseline, and initial state.
#[derive(Debug)]
pub struct ReplicaSeed<S> {
    pub binding: Binding,
    pub subscription: SubscriptionId,
    pub baseline: CommitCursor,
    pub state: S,
}

/// Replica batch wrapping binding, subscription, sequence, cursor, and changes.
#[derive(Debug)]
pub struct ReplicaBatch<C> {
    pub binding: Binding,
    pub subscription: SubscriptionId,
    pub seq: ReplicaSeq,
    pub through: CommitCursor,
    pub changes: C,
}

// ================================================================================================
// Authority Domain Types
// ================================================================================================

/// Associated data types belonging to the authority domain.
/// Implementations define concrete payload structures for simulation state.
pub trait AuthorityData {
    type Snapshot: Send + Sync + 'static;
    type Changes: Send + Sync + 'static;
    type Input: Send + 'static;
    type Output: Send + 'static;
    type ReplicaSnapshot: Send + Sync + 'static;
    type ReplicaChanges: Send + Sync + 'static;
}

/// Type alias for model-specific world snapshot.
pub type ModelSnapshot<M> = WorldSnapshot<<M as AuthorityData>::Snapshot>;

/// Type alias for model-specific commit record.
pub type ModelCommit<M> = CommitRecord<<M as AuthorityData>::Changes>;

/// Type alias for model-specific world request.
pub type ModelRequest<M> = WorldRequest<<M as AuthorityData>::Input>;

/// Type alias for model-specific request result.
pub type ModelResult<M> = RequestResult<<M as AuthorityData>::Output>;

/// Type alias for model-specific replica seed.
pub type ModelReplicaSeed<M> = ReplicaSeed<<M as AuthorityData>::ReplicaSnapshot>;

/// Type alias for model-specific replica batch.
pub type ModelReplicaBatch<M> = ReplicaBatch<<M as AuthorityData>::ReplicaChanges>;

// ================================================================================================
// Validation and Helper Methods
// ================================================================================================

impl CommitSeq {
    /// Initial baseline commit sequence.
    pub const INITIAL: Self = Self(0);

    /// Check if this sequence immediately follows the previous one.
    pub fn follows(self, previous: Self) -> Result<(), ValidationError> {
        match previous.next() {
            Some(expected) if expected == self => Ok(()),
            Some(_) => Err(ValidationError::NonConsecutiveCommit {
                previous: previous.0,
                current: self.0,
            }),
            None => Err(ValidationError::CommitSeqOverflow),
        }
    }

    /// Attempt to get the next sequence number, returning None on overflow.
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

impl DurableSeq {
    /// Initial baseline durable sequence.
    pub const INITIAL: Self = Self(0);

    /// Check if this durable sequence does not exceed the commit sequence.
    pub fn within(self, commit: CommitSeq) -> Result<(), ValidationError> {
        if self.0 <= commit.0 {
            Ok(())
        } else {
            Err(ValidationError::DurableExceedsCommit {
                durable: self.0,
                commit: commit.0,
            })
        }
    }
}

impl RequestSeq {
    /// Validate that request sequence starts at 1.
    pub fn validate(self) -> Result<(), ValidationError> {
        if self.0 == 0 {
            Err(ValidationError::RequestSeqZero)
        } else {
            Ok(())
        }
    }
}

impl ReplicaSeq {
    /// Validate that replica sequence starts at 1.
    pub fn validate(self) -> Result<(), ValidationError> {
        if self.0 == 0 {
            Err(ValidationError::ReplicaSeqZero)
        } else {
            Ok(())
        }
    }
}

impl CommitCursor {
    /// Create a cursor at the initial baseline.
    pub fn initial(world: WorldId) -> Self {
        Self {
            world,
            seq: CommitSeq::INITIAL,
        }
    }
}

impl CommitMeta {
    /// Validate that cursor follows previous sequence.
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.cursor.seq.follows(self.previous)
    }
}

impl Durability {
    /// Check if durability covers the target cursor.
    pub fn covers(&self, target: CommitCursor) -> bool {
        self.binding.world == target.world && self.through.0 >= target.seq.0
    }
}

impl OutputBudget {
    /// Create a budget with specified limits.
    pub fn new(max_items: u32, max_bytes: u64) -> Self {
        Self {
            max_items,
            max_bytes,
        }
    }

    /// Check whether the given total usage stays within the budget.
    pub fn allows(&self, items: u32, bytes: u64) -> bool {
        items <= self.max_items && bytes <= self.max_bytes
    }
}

impl Fault {
    /// Create a fault with code and context.
    pub fn new(code: FaultCode, context: impl Into<String>) -> Self {
        Self {
            code,
            context: context.into(),
        }
    }

    /// Create an invalid data fault.
    pub fn invalid_data(context: impl Into<String>) -> Self {
        Self::new(FaultCode::InvalidData, context)
    }

    /// Create an incompatible fault.
    pub fn incompatible(context: impl Into<String>) -> Self {
        Self::new(FaultCode::Incompatible, context)
    }

    /// Create a resource limit fault.
    pub fn resource_limit(context: impl Into<String>) -> Self {
        Self::new(FaultCode::ResourceLimit, context)
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.context)
    }
}

impl std::error::Error for Fault {}

impl From<ValidationError> for Fault {
    fn from(err: ValidationError) -> Self {
        Self::invalid_data(err.to_string())
    }
}
