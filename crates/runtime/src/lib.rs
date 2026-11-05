//! CPU task execution, I/O, and bounded experiments.

#![forbid(unsafe_code)]

mod executor;
mod scoped;
pub use executor::{
    Cancellation, Executor, ExecutorConfig, Job, JobBudget, JobError, JobOutput, Priority,
    Reservation, ReserveError, Usage,
};
pub use scoped::run_scoped;

#[cfg(feature = "ecs-probe")]
mod ecs_probe;

#[cfg(feature = "ecs-probe")]
pub use ecs_probe::{EcsProbeExecutor, EcsProbeReport, run_ecs_probe, run_ecs_probe_with_executor};
