//! Borrowed phase-start planning; durable publication and replay stay serial.
use super::*;
use crate::{HostCheckpointStore, Server};
use torse_runtime::{ExecutorConfig, JobBudget, JobError, JobOutput, run_scoped};
use torse_storage::CommitJournal;

#[cfg(test)]
#[path = "planning/tests.rs"]
mod tests;

/// Limits one joined planning batch. The phase world is borrowed once, never
/// cloned. Each admitted rule reserves four times `ExecutionLimits.runtime`
/// bytes for trusted planner work, stable capture, and bounded wire validation.
#[derive(Clone, Copy, Debug)]
pub struct PlanningConfig {
    pub workers: usize,
    pub max_in_flight: usize,
}

impl Default for PlanningConfig {
    fn default() -> Self {
        Self {
            workers: std::thread::available_parallelism()
                .map_or(1, usize::from)
                .saturating_sub(2)
                .clamp(1, 4),
            max_in_flight: 4,
        }
    }
}

impl PlanningConfig {
    fn validate(self, limits: ExecutionLimits) -> Result<(), Fault> {
        if self.workers == 0
            || self.workers > 64
            || self.max_in_flight == 0
            || self.max_in_flight > 64
        {
            return Err(Fault::invalid_data("invalid parallel planning limits"));
        }
        self.executor(limits)?;
        Ok(())
    }

    fn executor(self, limits: ExecutionLimits) -> Result<(ExecutorConfig, JobBudget), Fault> {
        let output = usize::try_from(limits.runtime.max_bytes)
            .ok()
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| Fault::resource_limit("planner byte reservation overflow"))?;
        let input = size_of::<&(dyn DurableRule + Sync)>() + size_of::<usize>();
        Ok((
            ExecutorConfig {
                workers: self.workers,
                max_jobs: self.max_in_flight,
                max_input_bytes: input * self.max_in_flight,
                max_output_bytes: output
                    .checked_mul(self.max_in_flight)
                    .ok_or_else(|| Fault::resource_limit("planning batch reservation overflow"))?,
            },
            JobBudget {
                input_bytes: input,
                output_bytes: output,
            },
        ))
    }
}

/// Borrowed native job admission within an already reserved rule-planning batch.
/// Adapter estimates cover retained inputs/results and concurrent scratch. This
/// is a cooperative budget for trusted code, not an allocator sandbox.
pub struct PlanningContext {
    config: PlanningConfig,
    max_work_bytes: usize,
    stats: std::cell::Cell<NativePlanningStats>,
}

/// Process-local diagnostics; never part of ordering, compatibility or recovery.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NativePlanningStats {
    pub batches: u64,
    pub jobs: u64,
    pub peak_lanes: usize,
    pub budget_fallbacks: u64,
}

impl PlanningContext {
    fn fallback<T>(&self) -> Result<Option<Vec<T>>, Fault> {
        let mut stats = self.stats.get();
        stats.budget_fallbacks = stats.budget_fallbacks.saturating_add(1);
        self.stats.set(stats);
        Ok(None)
    }
    /// Return None when the estimated batch cannot fit, so the rule can use its
    /// serial reference. Results preserve input order and no worker outlives us.
    pub fn map<T: Send, F: Fn(usize) -> T + Sync>(
        &self,
        count: usize,
        retained_bytes_per_job: usize,
        scratch_bytes_per_job: usize,
        work: F,
    ) -> Result<Option<Vec<T>>, Fault> {
        if count == 0 {
            return Ok(Some(Vec::new()));
        }
        // Each lane owns a bounded slice of retained results and reuses scratch
        // across bodies. Start one joined thread batch rather than one per window.
        let retained_bytes_per_job =
            retained_bytes_per_job.max(size_of::<T>().saturating_mul(4).saturating_add(64));
        let Some(retained) = retained_bytes_per_job.checked_mul(count) else {
            return self.fallback();
        };
        let Some(available) = self.max_work_bytes.checked_sub(retained) else {
            return self.fallback();
        };
        let lanes = self
            .config
            .workers
            .min(self.config.max_in_flight)
            .min(count)
            .min(available / scratch_bytes_per_job.max(1));
        if lanes == 0 {
            return self.fallback();
        }
        let Some(per_lane) = count
            .div_ceil(lanes)
            .checked_mul(retained_bytes_per_job)
            .and_then(|bytes| bytes.checked_add(scratch_bytes_per_job))
        else {
            return self.fallback();
        };
        let Some(total) = per_lane
            .checked_mul(lanes)
            .filter(|bytes| *bytes <= self.max_work_bytes)
        else {
            return self.fallback();
        };
        let executor = ExecutorConfig {
            workers: lanes,
            max_jobs: lanes,
            max_input_bytes: lanes,
            max_output_bytes: total,
        };
        let budget = JobBudget {
            input_bytes: 0,
            output_bytes: per_lane,
        };
        let mut stats = self.stats.get();
        stats.batches = stats.batches.saturating_add(1);
        stats.jobs = stats.jobs.saturating_add(count as u64);
        stats.peak_lanes = stats.peak_lanes.max(lanes);
        self.stats.set(stats);
        let batches = run_scoped(executor, budget, lanes, |lane| {
            let values: Vec<_> = (lane..count)
                .step_by(lanes)
                .map(|index| (index, work(index)))
                .collect();
            Ok(JobOutput {
                value: values,
                bytes: per_lane,
            })
        })
        .map_err(|error| {
            Fault::new(
                FaultCode::InvalidState,
                format!("native planning workers: {error}"),
            )
        })?;
        let mut indexed = Vec::with_capacity(count);
        for batch in batches {
            indexed.extend(batch.map_err(|error| {
                Fault::new(
                    FaultCode::InvalidState,
                    format!("native planning job: {error:?}"),
                )
            })?);
        }
        indexed.sort_unstable_by_key(|(index, _)| *index);
        let results = indexed.into_iter().map(|(_, value)| value).collect();
        Ok(Some(results))
    }
}

impl ExecutionHost {
    pub fn native_planning_stats(&self) -> NativePlanningStats {
        self.native_planning_stats.get()
    }
    /// Configure between drive calls, including after recovery. Existing frozen
    /// work is retained; this only affects the next uncollected phase.
    pub fn set_parallel_planning(&mut self, config: Option<PlanningConfig>) -> Result<(), Fault> {
        if let Some(config) = config {
            config.validate(self.limits)?;
        }
        self.planning = config;
        Ok(())
    }

    pub(super) fn collect_rules(
        &self,
        phase: Phase,
        tick: Tick,
        tasks: &mut Vec<FrozenTask>,
    ) -> Result<(), Fault> {
        let specs: Vec<_> = self
            .runtime
            .rules
            .iter()
            .filter(|s| s.phase == phase)
            .cloned()
            .collect();
        let order = order_phase(&specs).map_err(|e| Fault::invalid_data(e.to_string()))?;
        let ordered: Vec<_> = order
            .iter()
            .map(|id| {
                let spec = specs
                    .iter()
                    .find(|s| s.id == *id)
                    .expect("declared specification");
                let rule = self
                    .rules
                    .iter()
                    .find(|r| r.spec().id == *id)
                    .expect("declared rule");
                (spec, rule.as_ref())
            })
            .collect();
        let mut next = 0;
        while next < ordered.len() {
            let (_, rule) = ordered[next];
            let Some(config) = self.planning.filter(|_| rule.parallel_planner().is_some()) else {
                let plans = rule.plan(self.world(), tick)?;
                if plans.len() > self.limits.phase_tasks.saturating_sub(tasks.len()) {
                    return Err(Fault::resource_limit("phase task budget"));
                }
                for plan in plans {
                    tasks.push(self.freeze_plan(plan, phase, None)?);
                }
                next += 1;
                continue;
            };
            // Only contiguous independent opt-in rules overlap. A legacy rule
            // is a barrier, and every declared edge completes before its target.
            let mut batch = Vec::new();
            for (spec, rule) in ordered.iter().skip(next).take(config.max_in_flight) {
                if ordered[next..next + batch.len()]
                    .iter()
                    .any(|(previous, _)| {
                        previous.before.contains(&spec.id) || spec.after.contains(&previous.id)
                    })
                {
                    break;
                }
                let Some(planner) = rule.parallel_planner() else {
                    break;
                };
                if planner.spec() != **spec {
                    return Err(Fault::invalid_data("parallel planner declaration mismatch"));
                }
                batch.push(planner);
            }
            let world = self.world();
            let limits = self.limits;
            let (executor, budget) = config.executor(limits)?;
            let _memory = if let Some(pool) = self.memory_pool() {
                let bytes = (budget.input_bytes as u64)
                    .saturating_add(budget.output_bytes as u64)
                    .saturating_mul(batch.len() as u64);
                match pool.reserve(MemoryClass::Executor, bytes, EvictionRank::Pinned) {
                    Ok(lease) => Some(lease),
                    Err(error) if error.code == FaultCode::ResourceLimit => {
                        let plans = rule.plan(self.world(), tick)?;
                        if plans.len() > self.limits.phase_tasks.saturating_sub(tasks.len()) {
                            return Err(Fault::resource_limit("phase task budget"));
                        }
                        for plan in plans {
                            tasks.push(self.freeze_plan(plan, phase, None)?);
                        }
                        next += 1;
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            // A single rule may expose finer-grained native jobs. Execute its
            // dispatcher here so it never starts a nested pool inside a worker.
            if batch.len() == 1 {
                let context = PlanningContext {
                    config,
                    max_work_bytes: budget.output_bytes,
                    stats: Default::default(),
                };
                let plans = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    batch[0].plan_parallel(world, tick, &context)
                }));
                let mut stats = self.native_planning_stats.get();
                let current = context.stats.get();
                stats.batches = stats.batches.saturating_add(current.batches);
                stats.jobs = stats.jobs.saturating_add(current.jobs);
                stats.peak_lanes = stats.peak_lanes.max(current.peak_lanes);
                stats.budget_fallbacks = stats
                    .budget_fallbacks
                    .saturating_add(current.budget_fallbacks);
                self.native_planning_stats.set(stats);
                let plans = plans.map_err(|_| {
                    Fault::new(
                        FaultCode::InvalidState,
                        "native planning dispatcher panicked",
                    )
                })??;
                if plans.len() > limits.phase_tasks.saturating_sub(tasks.len()) {
                    return Err(Fault::resource_limit("phase task budget"));
                }
                let frozen = plans
                    .into_iter()
                    .map(|plan| freeze_rule_plan(world, plan, phase, None))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut bytes = Vec::new();
                wire::encode(&frozen, &mut bytes, limits.runtime, limits.codec)?;
                tasks.extend(wire::decode::<Vec<FrozenTask>>(
                    &bytes,
                    limits.runtime,
                    limits.codec,
                )?);
                next += 1;
                continue;
            }
            let results = run_scoped(executor, budget, batch.len(), |index| {
                let result = (|| {
                    let plans = batch[index].plan(world, tick)?;
                    if plans.len() > limits.phase_tasks {
                        return Err(Fault::resource_limit("planner task budget"));
                    }
                    let frozen = plans
                        .into_iter()
                        .map(|plan| freeze_rule_plan(world, plan, phase, None))
                        .collect::<Result<Vec<_>, _>>()?;
                    let mut bytes = Vec::new();
                    wire::encode(&frozen, &mut bytes, limits.runtime, limits.codec)?;
                    drop(frozen);
                    // Enforce owned allocation/node limits as well as wire size.
                    wire::decode::<Vec<FrozenTask>>(&bytes, limits.runtime, limits.codec)
                })();
                Ok(JobOutput {
                    value: result,
                    bytes: budget.output_bytes,
                })
            })
            .map_err(|e| Fault::new(FaultCode::InvalidState, format!("planning workers: {e}")))?;
            next += batch.len();
            // Completion order must never select a contention winner or error.
            for result in results {
                let plans = result.map_err(|e| match e {
                    JobError::OutputLimit => Fault::resource_limit("planner output budget"),
                    _ => Fault::new(FaultCode::InvalidState, format!("planning worker: {e:?}")),
                })??;
                if plans.len() > self.limits.phase_tasks.saturating_sub(tasks.len()) {
                    return Err(Fault::resource_limit("phase task budget"));
                }
                tasks.extend(plans);
            }
        }
        Ok(())
    }
}

pub(super) fn freeze_rule_plan(
    world: &WorldAuthority,
    plan: WorkPlan,
    phase: Phase,
    consume: Option<EventId>,
) -> Result<FrozenTask, Fault> {
    if plan
        .transaction
        .as_ref()
        .is_some_and(|t| t.ordering.phase != phase)
    {
        return Err(Fault::invalid_data("rule transaction phase mismatch"));
    }
    Ok(FrozenTask {
        transaction: plan
            .transaction
            .as_ref()
            .map(|t| StoredTransactionGroup::capture(world, t))
            .transpose()?,
        request: None,
        consume,
        schedule: plan.schedule,
        cancel: plan.cancel,
        status: ReplyStatus::Committed,
        output: ComponentFields::new(),
    })
}

impl<J: CommitJournal<ExecutionModel>, S: HostCheckpointStore<ExecutionModel>>
    Server<ExecutionHost, J, S>
{
    pub fn set_parallel_planning(&mut self, config: Option<PlanningConfig>) -> Result<(), Fault> {
        self.authority()?;
        self.authority.set_parallel_planning(config)
    }
}
