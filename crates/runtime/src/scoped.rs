//! Joined CPU batches borrowing an immutable phase view without copying it.
use crate::{ExecutorConfig, JobBudget, JobError, JobOutput};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

#[cfg(feature = "scoped-pool")]
use rayon::prelude::*;

/// Reusable workers for synchronous batches borrowing the caller's data.
#[cfg(feature = "scoped-pool")]
pub struct ScopedPool(rayon::ThreadPool);

#[cfg(feature = "scoped-pool")]
impl ScopedPool {
    pub fn new(workers: usize) -> io::Result<Self> {
        if workers == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero scoped workers",
            ));
        }
        rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|index| format!("torse-planning-{index}"))
            .build()
            .map(Self)
            .map_err(io::Error::other)
    }

    /// Complete the admitted batch before returning results in input order.
    pub fn run<T: Send, F>(
        &self,
        config: ExecutorConfig,
        budget: JobBudget,
        count: usize,
        work: F,
    ) -> io::Result<Vec<Result<T, JobError>>>
    where
        F: Fn(usize) -> Result<JobOutput<T>, JobError> + Sync,
    {
        validate_batch(config, budget, count)?;
        if self.0.current_thread_index().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "nested scoped CPU batch",
            ));
        }
        if self.0.current_num_threads().min(count) > config.workers {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "scoped CPU worker budget",
            ));
        }
        if count == 0 {
            return Ok(Vec::new());
        }
        let work = &work;
        Ok(self.0.install(|| {
            (0..count)
                .into_par_iter()
                .map(|index| run_job(budget, || work(index)))
                .collect()
        }))
    }
}

/// Run a bounded batch and return results in input order. Every worker joins
/// before the borrowed context can change, including on callback/spawn failure.
/// Budgets cover the whole batch, including completed results. Trusted adapters
/// must bound intermediate allocation; this is not a native-code memory sandbox.
/// Unlike `Executor`, threads live for this call and work cannot outlive it.
pub fn run_scoped<T: Send, F>(
    config: ExecutorConfig,
    budget: JobBudget,
    count: usize,
    work: F,
) -> io::Result<Vec<Result<T, JobError>>>
where
    F: Fn(usize) -> Result<JobOutput<T>, JobError> + Sync,
{
    validate_batch(config, budget, count)?;
    let results: Vec<_> = (0..count).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    thread::scope(|scope| -> io::Result<()> {
        for index in 0..config.workers.min(count) {
            let (results, next, work) = (&results, &next, &work);
            thread::Builder::new()
                .name(format!("torse-phase-{index}"))
                .spawn_scoped(scope, move || {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        if index >= results.len() {
                            break;
                        }
                        let result = run_job(budget, || work(index));
                        *results[index].lock().expect("scoped result lock") = Some(result);
                    }
                })?;
        }
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

fn validate_batch(config: ExecutorConfig, budget: JobBudget, count: usize) -> io::Result<()> {
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
    Ok(())
}

fn run_job<T>(
    budget: JobBudget,
    work: impl FnOnce() -> Result<JobOutput<T>, JobError>,
) -> Result<T, JobError> {
    let output = catch_unwind(AssertUnwindSafe(work)).unwrap_or(Err(JobError::Panicked))?;
    if output.bytes > budget.output_bytes {
        Err(JobError::OutputLimit)
    } else {
        Ok(output.value)
    }
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

    #[cfg(feature = "scoped-pool")]
    #[test]
    fn pool_reuses_worker_across_batches_and_rejects_nested_admission() {
        let pool = ScopedPool::new(1).unwrap();
        let budget = JobBudget {
            input_bytes: 4,
            output_bytes: 4,
        };
        let worker = || {
            pool.run(config(), budget, 1, |_| {
                assert!(
                    pool.run(
                        config(),
                        budget,
                        1,
                        |_| -> Result<JobOutput<()>, JobError> {
                            panic!("nested batch must not run");
                        }
                    )
                    .is_err()
                );
                Ok(JobOutput {
                    value: thread::current().id(),
                    bytes: 4,
                })
            })
            .unwrap()[0]
                .clone()
                .unwrap()
        };
        let first = worker();
        assert_ne!(first, thread::current().id());
        assert_eq!(first, worker());
    }

    #[test]
    fn borrows_context_runs_concurrently_and_retains_input_order() {
        let input = [11, 17, 23, 29];
        let gate = Barrier::new(2);
        let result = run_scoped(
            config(),
            JobBudget {
                input_bytes: 4,
                output_bytes: 4,
            },
            4,
            |i| {
                gate.wait();
                Ok(JobOutput {
                    value: &input[i],
                    bytes: 4,
                })
            },
        )
        .unwrap();
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
        let results = run_scoped(config(), budget, 4, |i| {
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
