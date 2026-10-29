//! Authoritative simulation: phases, systems, transactions, and commit order.
//!
//! M1 delivered editable world storage. M2 begins here with the execution model
//! that turns storage into an authority: a fixed-step clock, an ordered phase
//! graph, typed read/write transactions, a single coordinator that assigns the
//! world's commit sequence, and a tick driver that runs systems in that order.
//!
//! Simulation retains the serial reference executor. Hosts may parallelize
//! read-only planning and movement preparation, while publication retains the
//! same serial arbitration order.

#![forbid(unsafe_code)]

pub mod action;
pub mod authority;
pub mod block_actions;
mod block_instances;
mod body;
pub mod clock;
mod collider_plan;
pub mod coordinator;
pub mod durable;
mod event_dependencies;
mod lifecycle;
pub mod movement;
pub mod payload;
pub mod phase;
mod placement;
pub mod player;
mod presentation;
mod relocation;
pub mod replica;
pub mod simulation;
pub mod streaming;
pub mod system;
pub mod transaction;

pub use authority::{AuthorityError, Recovery, RecoveryError, WorldAuthority};
pub use clock::{TickClock, TickReport};
pub use coordinator::{
    CommitCoordinator, CommitFault, Committed, ComponentPrevalidation, ComponentWork,
    MovementPrevalidation, PrepareError, PreparedChange, Publication,
};
pub use payload::{
    changes_from_committed, changes_from_prepared, persistent_changes_from_prepared,
};
pub use phase::{OrderError, Phase, SystemSpec, next_resume_point, order_phase};
pub use presentation::PresentationClock;
pub use simulation::{
    DEFAULT_PHASE_TRANSACTION_LIMIT, Simulation, SimulationError, SimulationLimits, TickOutcome,
};
pub use system::{System, SystemContext};
pub use transaction::{
    BlockPrecondition, Despawn, DestinationPolicy, MAX_GROUP_MEMBERS, Movement, Mutation,
    OrderingKey, Relocation, Spawn, Transaction, TransactionGroup,
};
