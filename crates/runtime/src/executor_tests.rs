use super::*;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn config(jobs: usize) -> ExecutorConfig {
    ExecutorConfig {
        workers: 1,
        max_jobs: jobs,
        max_input_bytes: jobs * 8,
        max_output_bytes: jobs * 8,
    }
}
fn budget() -> JobBudget {
    JobBudget {
        input_bytes: 8,
        output_bytes: 8,
    }
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "CPU task did not make progress");
        thread::yield_now();
    }
}
fn take<T>(job: &mut Job<T>) -> Result<T, JobError> {
    let mut result = None;
    wait(|| {
        result = job.try_take();
        result.is_some()
    });
    result.unwrap()
}

#[test]
fn reservations_and_completed_results_keep_all_capacity_charged() {
    let pool = Executor::new(config(1)).unwrap();
    let lease = pool.try_reserve(budget()).unwrap();
    assert!(matches!(
        pool.try_reserve(budget()),
        Err(ReserveError::Busy)
    ));
    drop(lease);
    assert_eq!(pool.usage().jobs, 0);
    let (done, observed) = mpsc::channel();
    let mut job = pool
        .try_reserve(budget())
        .unwrap()
        .submit(Priority::Background, move |_| {
            done.send(()).unwrap();
            Ok(JobOutput {
                value: 42,
                bytes: 8,
            })
        });
    observed.recv_timeout(Duration::from_secs(5)).unwrap();
    wait(|| pool.usage().running == 0);
    assert!(
        matches!(pool.try_reserve(budget()), Err(ReserveError::Busy)),
        "unconsumed output owns its lease"
    );
    assert_eq!(take(&mut job), Ok(42));
    drop(job);
    wait(|| pool.usage().jobs == 0);
    assert_eq!(pool.usage().input_bytes, 0);
    assert_eq!(pool.usage().output_bytes, 0);
    assert!(matches!(
        pool.try_reserve(JobBudget {
            input_bytes: 9,
            output_bytes: 1
        }),
        Err(ReserveError::TooLarge)
    ));
}

#[test]
fn shared_process_pressure_retries_and_finished_output_transfers_its_reservation() {
    use torse_types::MemoryBudget;
    let memory = MemoryPool::new(MemoryBudget::new(4096).with_class_limit(MemoryClass::Mesh, 8));
    let executor = Executor::with_memory(config(2), memory.clone()).unwrap();
    let baseline = memory.usage().total;
    let pinned = memory
        .reserve(MemoryClass::World, 4096 - baseline, EvictionRank::Pinned)
        .unwrap();
    assert!(matches!(
        executor.try_reserve(budget()),
        Err(ReserveError::Busy)
    ));
    drop(pinned);
    let mut job = executor
        .try_reserve(budget())
        .unwrap()
        .submit(Priority::Visible, |_| {
            Ok(JobOutput {
                value: vec![3; 8],
                bytes: 8,
            })
        });
    // Usage counters are sampled separately: the worker can start between the
    // running and queued observations. Wait for the result this test transfers.
    wait(|| job.state.result.lock().expect("job result lock").is_some());
    let held = memory
        .reserve(MemoryClass::Mesh, 8, EvictionRank::Pinned)
        .unwrap();
    assert!(matches!(
        job.try_take_charged(MemoryClass::Mesh, EvictionRank::UnsubscribedDerived),
        Err(ReserveError::Busy)
    ));
    drop(held);
    let (bytes, charge) = job
        .try_take_charged(MemoryClass::Mesh, EvictionRank::UnsubscribedDerived)
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(bytes, [3; 8]);
    drop(job);
    wait(|| executor.usage().jobs == 0);
    assert_eq!(memory.usage().current[MemoryClass::Mesh.index()], 8);
    drop(bytes);
    drop(charge);
    drop(executor);
    assert_eq!(memory.usage().total, 0);
}

struct InputDrop(Arc<AtomicUsize>);
impl Drop for InputDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

#[test]
fn queued_cancel_drops_input_and_running_cancel_keeps_its_lease_until_completion() {
    let pool = Executor::new(config(2)).unwrap();
    let (started, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let mut running = pool
        .try_reserve(budget())
        .unwrap()
        .submit(Priority::Background, move |_| {
            started.send(()).unwrap();
            gate.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(JobOutput { value: 1, bytes: 8 })
        });
    observed.recv_timeout(Duration::from_secs(5)).unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    let input = InputDrop(Arc::clone(&dropped));
    let queued = pool
        .try_reserve(budget())
        .unwrap()
        .submit(Priority::Background, move |_| {
            drop(input);
            panic!("cancelled queued callback must never run")
        });
    drop(queued);
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    assert_eq!(pool.usage().jobs, 1);
    let held = pool.try_reserve(budget()).unwrap();
    running.cancel();
    assert!(matches!(
        pool.try_reserve(budget()),
        Err(ReserveError::Busy)
    ));
    release.send(()).unwrap();
    assert_eq!(take(&mut running), Err(JobError::Cancelled));
    drop(running);
    drop(held);
    wait(|| pool.usage().jobs == 0);
}

#[test]
fn oversized_outputs_and_panics_are_isolated_and_completed_cancel_discards_output() {
    let pool = Executor::new(config(1)).unwrap();
    for expected in [JobError::OutputLimit, JobError::Panicked] {
        let panic = expected == JobError::Panicked;
        let mut job = pool
            .try_reserve(budget())
            .unwrap()
            .submit(Priority::Visible, move |_| {
                assert!(!panic, "intentional derived task panic");
                Ok(JobOutput {
                    value: 1u32,
                    bytes: 9,
                })
            });
        assert_eq!(take(&mut job), Err(expected));
        drop(job);
        wait(|| pool.usage().jobs == 0);
    }
    let mut job = pool
        .try_reserve(budget())
        .unwrap()
        .submit(Priority::Visible, |_| Ok(JobOutput { value: 3, bytes: 8 }));
    wait(|| pool.usage().queued == 0 && pool.usage().running == 0);
    job.cancel();
    assert_eq!(take(&mut job), Err(JobError::Cancelled));
}

#[test]
fn shutdown_cancels_queued_and_running_work_and_joins_workers() {
    let pool = Executor::new(config(2)).unwrap();
    let (started, observed) = mpsc::channel();
    let mut running =
        pool.try_reserve(budget())
            .unwrap()
            .submit(Priority::Visible, move |cancel| {
                started.send(()).unwrap();
                while !cancel.is_cancelled() {
                    thread::yield_now();
                }
                cancel.check()?;
                Ok(JobOutput { value: 1, bytes: 8 })
            });
    observed.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut queued = pool
        .try_reserve(budget())
        .unwrap()
        .submit(Priority::Background, |_| {
            panic!("shutdown must discard queued work")
        });
    drop(pool);
    assert_eq!(take(&mut running), Err(JobError::Cancelled));
    assert_eq!(take(&mut queued), Err(JobError::Cancelled));
}

#[test]
fn priority_keeps_background_work_from_starving() {
    let pool = Executor::new(config(12)).unwrap();
    let (started, observed) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let mut blocker = pool
        .try_reserve(budget())
        .unwrap()
        .submit(Priority::Background, move |_| {
            started.send(()).unwrap();
            gate.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(JobOutput {
                value: 99,
                bytes: 1,
            })
        });
    observed.recv_timeout(Duration::from_secs(5)).unwrap();
    let (order, ordered) = mpsc::channel();
    let mut jobs = Vec::new();
    for index in 0..10 {
        let sender = order.clone();
        let priority = if index == 0 {
            Priority::Background
        } else {
            Priority::Visible
        };
        jobs.push(
            pool.try_reserve(budget())
                .unwrap()
                .submit(priority, move |_| {
                    sender.send(index).unwrap();
                    Ok(JobOutput {
                        value: index,
                        bytes: 1,
                    })
                }),
        );
    }
    release.send(()).unwrap();
    assert_eq!(take(&mut blocker), Ok(99));
    let order: Vec<_> = (0..10)
        .map(|_| ordered.recv_timeout(Duration::from_secs(5)).unwrap())
        .collect();
    assert_eq!(&order[..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(order[8], 0);
    for job in &mut jobs {
        take(job).unwrap();
    }
}

#[test]
fn independent_byte_limits_backpressure_before_job_slots_are_exhausted() {
    let pool = Executor::<()>::new(ExecutorConfig {
        workers: 1,
        max_jobs: 4,
        max_input_bytes: 10,
        max_output_bytes: 12,
    })
    .unwrap();
    let first = pool
        .try_reserve(JobBudget {
            input_bytes: 6,
            output_bytes: 7,
        })
        .unwrap();
    assert!(matches!(
        pool.try_reserve(JobBudget {
            input_bytes: 5,
            output_bytes: 0
        }),
        Err(ReserveError::Busy)
    ));
    assert!(matches!(
        pool.try_reserve(JobBudget {
            input_bytes: 0,
            output_bytes: 6
        }),
        Err(ReserveError::Busy)
    ));
    let second = pool
        .try_reserve(JobBudget {
            input_bytes: 4,
            output_bytes: 5,
        })
        .unwrap();
    assert_eq!(
        pool.usage(),
        Usage {
            jobs: 2,
            input_bytes: 10,
            output_bytes: 12,
            ..Usage::default()
        }
    );
    drop(first);
    drop(second);
    assert_eq!(pool.usage(), Usage::default());
    assert!(matches!(
        pool.try_reserve(JobBudget {
            input_bytes: usize::MAX,
            output_bytes: 0
        }),
        Err(ReserveError::TooLarge)
    ));
    assert!(
        Executor::<()>::new(ExecutorConfig {
            workers: 0,
            ..config(1)
        })
        .is_err()
    );
}
