//! Host-selected cache targets, independent of reservation and protocol ceilings.
use crate::Fault;
use std::time::Duration;

/// Advisory resident target. Reservations continue to enforce the hard budget.
/// Owners should reduce prefetch and reclaim derived data toward this target;
/// pinned work may temporarily exceed it. Changing policy does not change
/// protocol input limits, execution quotas, allocations or the hard budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryPolicy {
    Fixed { target_bytes: u64 },
    Adaptive(AdaptiveMemoryPolicy),
}

/// Portable feedback policy; the host samples system pressure and owns reclamation.
/// On each eligible sample, the desired target is live logical usage plus the
/// additional host allowance less headroom, clamped to the configured range.
/// Normal changes use hysteresis and one bounded step. Pressure forces one
/// downward step, still bounded by the minimum. Repeated samples inside the
/// interval do not accelerate adjustments; late samples do not catch up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdaptiveMemoryPolicy {
    pub min_bytes: u64,
    pub max_bytes: u64,
    /// Additional host allowance kept unused before considering growth.
    pub headroom_bytes: u64,
    pub grow_step_bytes: u64,
    pub shrink_step_bytes: u64,
    /// Ignore desired changes of this size or smaller, except under pressure.
    pub hysteresis_bytes: u64,
    /// Minimum time between adjustments; there is no catch-up after a long gap.
    pub sample_interval: Duration,
}

/// Host observation for this shared pool, not a reading of raw free system RAM.
/// Account for untracked RSS, other pools and processes before granting allowance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryPressure {
    /// Additional bytes the host permits this pool to acquire at this sample.
    pub available_bytes: u64,
    /// Request a downward step even when the available allowance is large.
    pub pressured: bool,
}

impl MemoryPolicy {
    pub(crate) fn validate(self, hard_bytes: u64) -> Result<(), Fault> {
        let valid = match self {
            Self::Fixed { target_bytes } => target_bytes <= hard_bytes,
            Self::Adaptive(policy) => {
                policy.min_bytes <= policy.max_bytes
                    && policy.max_bytes <= hard_bytes
                    && policy.grow_step_bytes > 0
                    && policy.shrink_step_bytes > 0
                    && !policy.sample_interval.is_zero()
            }
        };
        if valid {
            Ok(())
        } else {
            Err(Fault::invalid_data(
                "memory policy range or step is invalid for hard budget",
            ))
        }
    }

    pub(crate) fn target(self, previous: u64) -> u64 {
        match self {
            Self::Fixed { target_bytes } => target_bytes,
            Self::Adaptive(policy) => previous.clamp(policy.min_bytes, policy.max_bytes),
        }
    }
}

pub(crate) struct MemoryController {
    pub policy: MemoryPolicy,
    pub target: u64,
    last_observed: Option<Duration>,
    last_adjusted: Option<Duration>,
}

impl MemoryController {
    pub fn new(policy: MemoryPolicy, previous: u64) -> Self {
        Self {
            policy,
            target: policy.target(previous),
            last_observed: None,
            last_adjusted: None,
        }
    }

    pub fn observe(
        &mut self,
        now: Duration,
        usage: u64,
        pressure: MemoryPressure,
    ) -> Result<(), Fault> {
        if self.last_observed.is_some_and(|previous| now <= previous) {
            return Err(Fault::invalid_data(
                "memory pressure samples must advance monotonically",
            ));
        }
        self.last_observed = Some(now);
        let MemoryPolicy::Adaptive(policy) = self.policy else {
            return Ok(());
        };
        if self
            .last_adjusted
            .is_some_and(|previous| now - previous < policy.sample_interval)
        {
            return Ok(());
        }
        self.last_adjusted = Some(now);
        let desired = usage
            .saturating_add(
                pressure
                    .available_bytes
                    .saturating_sub(policy.headroom_bytes),
            )
            .clamp(policy.min_bytes, policy.max_bytes);
        if pressure.pressured {
            self.target = self
                .target
                .saturating_sub(policy.shrink_step_bytes)
                .max(policy.min_bytes);
        } else if self.target.abs_diff(desired) > policy.hysteresis_bytes {
            self.target = if desired > self.target {
                desired.min(self.target.saturating_add(policy.grow_step_bytes))
            } else {
                desired.max(self.target.saturating_sub(policy.shrink_step_bytes))
            };
        }
        Ok(())
    }
}
