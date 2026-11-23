//! Joined CPU batches borrowing an immutable phase view without copying it.
use crate::{ExecutorConfig, JobBudget, JobError, JobOutput};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// Run a bounded batch and return results in input order. Every worker joins
/// before the borrowed context can change, including on callback/spawn failure.
/// Budgets cover the whole batch, including completed results. Trusted adapters
/// must bound intermediate allocation; this is not a native-code memory sandbox.
/// The caller participates as one of the configured workers. Additional threads
/// live only for this call, and work cannot outlive it.
pub fn run_scoped<T: Send, F>(
    config: ExecutorConfig,
    budget: JobBudget,
    count: usize,
    work: F,
) -> io::Result<Vec<Result<T, JobError>>>
where
    F: Fn(usize) -> Result<JobOutput<T>, JobError> + Sync,
{
    config.validate()?;
    if count > config.max_jobs
        || budget
            .input_bytes
            .checked_mul(count)
            .is_none_or(|n| n > config.max_input_bytes)
        || budget
            .output_bytes
            .checked_mul(count)
            .is_none_or(|n| n > config.max_output_bytes)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "scoped CPU batch exceeds capacity",
        ));
    }
    let lanes = config.workers.min(count);
    let run = |index| {
        catch_unwind(AssertUnwindSafe(|| work(index)))
            .unwrap_or(Err(JobError::Panicked))
            .and_then(|output| {
                if output.bytes > budget.output_bytes {
                    Err(JobError::OutputLimit)
                } else {
                    Ok(output.value)
                }
            })
    };
    if lanes <= 1 {
        return Ok((0..count).map(run).collect());
    }

    let results: Vec<_> = (0..count).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let drain = || loop {
        let index = next.fetch_add(1, Ordering::Relaxed);
        if index >= results.len() {
            break;
        }
        let result = run(index);
        *results[index].lock().expect("scoped result lock") = Some(result);
    };
    thread::scope(|scope| -> io::Result<()> {
        for index in 1..lanes {
            thread::Builder::new()
                .name(format!("torse-phase-{index}"))
                .spawn_scoped(scope, drain)?;
        }
        drain();
        Ok(())
    })?;
    Ok(results
        .into_iter()
        .map(|r| {
            r.into_inner()
                .expect("scoped result lock")
                .expect("joined job")
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    fn config() -> ExecutorConfig {
        ExecutorConfig {
            workers: 2,
            max_jobs: 4,
            max_input_bytes: 16,
            max_output_bytes: 16,
        }
    }

    #[test]
    fn borrows_context_runs_concurrently_and_retains_input_order() {
        let input = [11, 17, 23, 29];
        let gate = Barrier::new(2);
        let caller = thread::current().id();
        let ran_on_caller = std::sync::atomic::AtomicBool::new(false);
        let result = run_scoped(
            config(),
            JobBudget {
                input_bytes: 4,
                output_bytes: 4,
            },
            4,
            |i| {
                if thread::current().id() == caller {
                    ran_on_caller.store(true, Ordering::Relaxed);
                }
                gate.wait();
                Ok(JobOutput {
                    value: &input[i],
                    bytes: 4,
                })
            },
        )
        .unwrap();
        assert!(ran_on_caller.load(Ordering::Relaxed));
        assert_eq!(
            result
                .into_iter()
                .map(Result::unwrap)
                .copied()
                .collect::<Vec<_>>(),
            input
        );
    }

    #[test]
    fn rejects_capacity_before_callbacks_and_joins_panics_and_output_errors() {
        let budget = JobBudget {
            input_bytes: 4,
            output_bytes: 4,
        };
        for (count, budget) in [
            (5, budget),
            (
                4,
                JobBudget {
                    input_bytes: 5,
                    ..budget
                },
            ),
            (
                4,
                JobBudget {
                    output_bytes: usize::MAX,
                    ..budget
                },
            ),
        ] {
            assert!(
                run_scoped(
                    config(),
                    budget,
                    count,
                    |_| -> Result<JobOutput<()>, JobError> {
                        panic!("over-budget batch must not run")
                    }
                )
                .is_err()
            );
        }
        for workers in [1, 2] {
            let config = ExecutorConfig {
                workers,
                ..config()
            };
            let results = run_scoped(config, budget, 4, |i| {
                if i == 1 {
                    panic!("injected planner panic");
                }
                Ok(JobOutput {
                    value: i,
                    bytes: if i == 2 { 5 } else { 4 },
                })
            })
            .unwrap();
            assert_eq!(
                results,
                vec![
                    Ok(0),
                    Err(JobError::Panicked),
                    Err(JobError::OutputLimit),
                    Ok(3)
                ]
            );
        }
    }
}
