//! Borrowed phase-start planning; durable publication and replay stay serial.
use super::*;
use crate::{HostCheckpointStore, Server};
use torse_runtime::{ExecutorConfig, JobBudget, JobError, JobOutput, run_scoped};
use torse_storage::CommitJournal;

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

impl ExecutionHost {
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
