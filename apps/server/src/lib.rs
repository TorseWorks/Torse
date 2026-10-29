//! Synchronous host coordination and durable local world transactions.

#![forbid(unsafe_code)]

pub mod archive;
pub mod authentication;
mod authority;
pub mod bootstrap;
mod checkpoint;
mod coordinator;
mod directory;
mod generation;
pub use generation::GenerationIdentity;
pub mod execution;
pub mod item_drops;
mod lifecycle;
#[cfg(feature = "mods")]
pub mod mods;
pub mod network;
mod profiling;
mod reclamation;
pub mod residency;
pub mod section_pages;
pub mod systems;
pub use execution::PlanningConfig;
mod world;

pub use authority::{HostAuthority, InvalidationSink, NoReplicas, PreparedChange, RecoveryTarget};
pub use checkpoint::HostCheckpointStore;
pub use coordinator::{
    CommitBatch, CommitStep, DurablePublication, FlushMetrics, Server, ServerConfig, ServerState,
    ShutdownReport,
};
pub use directory::{WorldDirectory, WorldLease};
pub use lifecycle::{FileServer, FileServerConfig};
pub use world::{LocalCommit, LocalWorldModel, WorldHost, WorldHostRecovery, WorldPrepared};
