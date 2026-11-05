//! Allocation-free observations of existing queue ledgers. Not a globally atomic cut.
use crate::{CommitCursor, Durability, Fault};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Unavailable {
    NotEnabled,
    NotSupported,
    NotCollected,
    BudgetUnavailable,
    Overflow,
    Busy,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Diagnostic<T> {
    Available(T),
    Unavailable(Unavailable),
}

/// Explicit opt-in at the read boundary; normal execution stores no new counters.
/// Only fixed-size value observations are returned. The limit includes the outer
/// Diagnostic enum, not caller-retained history or an external exporter.
#[derive(Clone, Copy, Debug, Default)]
pub struct QueueDiagnostics {
    limit: Option<usize>,
}
impl QueueDiagnostics {
    pub const MAX_BYTES: usize = 1_048_576;
    pub fn enabled(max_snapshot_bytes: usize) -> Result<Self, Fault> {
        if max_snapshot_bytes == 0 || max_snapshot_bytes > Self::MAX_BYTES {
            return Err(Fault::invalid_data(
                "diagnostic snapshot budget must be 1..=1048576",
            ));
        }
        Ok(Self {
            limit: Some(max_snapshot_bytes),
        })
    }
    pub fn check<T>(&self) -> Result<(), Unavailable> {
        match self.limit {
            None => Err(Unavailable::NotEnabled),
            Some(limit) if size_of::<Diagnostic<T>>() > limit => {
                Err(Unavailable::BudgetUnavailable)
            }
            Some(_) => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestQueue {
    pub records: u64,
    /// No substitute using configured capacity or re-encoding is permitted.
    pub bytes: Diagnostic<u64>,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReplicaQueues {
    pub subscriptions: u64,
    pub batches: u64,
    pub items: u64,
    /// Existing conservative owned-payload cost, not wire bytes or RSS.
    pub logical_bytes: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorityQueues {
    pub requests: RequestQueue,
    pub frozen_tasks: u64,
    pub scheduled_events: u64,
    pub replicas: Diagnostic<ReplicaQueues>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalQueues {
    /// Includes accepted records and the outstanding staged reservation, if any.
    pub reserved_records: u64,
    pub reserved_encoded_bytes: u64,
    pub accepted: CommitCursor,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueProgress {
    pub published: CommitCursor,
    pub accepted: Diagnostic<CommitCursor>,
    pub durable: Durability,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServerQueues {
    pub authority: Diagnostic<AuthorityQueues>,
    pub journal: Diagnostic<JournalQueues>,
    pub progress: QueueProgress,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueByteKind {
    Payload,
    FramedRemaining,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameQueue {
    pub records: u64,
    pub bytes: u64,
    pub byte_kind: QueueByteKind,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportQueues {
    pub incoming: FrameQueue,
    /// Framed plaintext bytes still awaiting a complete frame, including its prefix.
    pub incomplete_frame_bytes: u64,
    /// TLS buffers after plaintext acceptance and socket traffic are not included.
    pub outgoing: FrameQueue,
    pub closed: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerQueues {
    pub waiting_requests: RequestQueue,
    pub subscriptions: u64,
    /// Existing queued payload bytes, excluding transport framing.
    pub control: Diagnostic<RequestQueue>,
    pub transport: Diagnostic<TransportQueues>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HubQueues {
    /// Registered entries, including closed peers until normal reaping.
    pub connections: u64,
    pub waiting_requests: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_and_budget_limits_are_explicit() {
        assert_eq!(
            QueueDiagnostics::default().check::<ServerQueues>(),
            Err(Unavailable::NotEnabled)
        );
        assert!(QueueDiagnostics::enabled(0).is_err());
        assert!(QueueDiagnostics::enabled(QueueDiagnostics::MAX_BYTES + 1).is_err());
        assert_eq!(
            QueueDiagnostics::enabled(1)
                .unwrap()
                .check::<ServerQueues>(),
            Err(Unavailable::BudgetUnavailable)
        );
        let exact = QueueDiagnostics::enabled(size_of::<Diagnostic<ServerQueues>>()).unwrap();
        assert_eq!(exact.check::<ServerQueues>(), Ok(()));
        assert!(size_of::<Diagnostic<ServerQueues>>() < 4096);
        assert!(size_of::<Diagnostic<PeerQueues>>() < 4096);
    }
}
