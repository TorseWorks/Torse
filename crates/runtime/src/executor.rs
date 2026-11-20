//! Bounded CPU work with reservation before owned input construction.
use std::collections::VecDeque;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use torse_types::{EvictionRank, MemoryClass, MemoryLease, MemoryPool};

/// Trusted host concurrency settings, independent of shared memory capacity.
/// Worker and queue counts stay fixed for the lifetime of an executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutorCapacity {
    pub workers: usize,
    /// Reservations, running work and unconsumed results share this count.
    pub max_jobs: usize,
}
impl ExecutorCapacity {
    fn validate(self) -> io::Result<()> {
        if self.workers == 0 || self.max_jobs == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid CPU executor capacity",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ExecutorConfig {
    pub workers: usize,
    /// Includes reservations, running work, and unconsumed results.
    pub max_jobs: usize,
    pub max_input_bytes: usize,
    /// Reserve peak working/output allocation, not just the final payload.
    pub max_output_bytes: usize,
}

impl ExecutorConfig {
    pub(crate) fn validate(self) -> io::Result<()> {
        ExecutorCapacity {
            workers: self.workers,
            max_jobs: self.max_jobs,
        }
        .validate()?;
        if self.max_input_bytes == 0 || self.max_output_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid CPU executor limits",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct JobBudget {
    pub input_bytes: usize,
    pub output_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Priority {
    Visible,
    Background,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReserveError {
    Busy,
    TooLarge,
    Stopped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobError {
    Cancelled,
    OutputLimit,
    Panicked,
    Failed(String),
}

pub struct JobOutput<T> {
    pub value: T,
    pub bytes: usize,
}

#[derive(Clone)]
pub struct Cancellation {
    local: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl Cancellation {
    pub fn is_cancelled(&self) -> bool {
        self.local.load(Ordering::Acquire) || self.stopped.load(Ordering::Acquire)
    }

    pub fn check(&self) -> Result<(), JobError> {
        if self.is_cancelled() {
            Err(JobError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    pub jobs: usize,
    pub queued: usize,
    pub running: usize,
    pub input_bytes: usize,
    pub output_bytes: usize,
}

#[derive(Default)]
struct Charges {
    jobs: AtomicUsize,
    running: AtomicUsize,
    input: AtomicUsize,
    output: AtomicUsize,
}

struct State<T> {
    token: Cancellation,
    result: Mutex<Option<Result<T, JobError>>>,
    budget: JobBudget,
    charges: Arc<Charges>,
    output_bytes: AtomicUsize,
    memory: Mutex<Option<MemoryLease>>,
}

impl<T> Drop for State<T> {
    fn drop(&mut self) {
        // Release only after the value and all captured task input are dropped.
        drop(self.result.get_mut().expect("job result lock").take());
        self.charges
            .input
            .fetch_sub(self.budget.input_bytes, Ordering::AcqRel);
        self.charges
            .output
            .fetch_sub(self.budget.output_bytes, Ordering::AcqRel);
        self.charges.jobs.fetch_sub(1, Ordering::AcqRel);
    }
}

type Work<T> = Box<dyn FnOnce(&Cancellation) -> Result<JobOutput<T>, JobError> + Send>;
struct Task<T> {
    // Captured inputs drop before the state releases their charge.
    work: Work<T>,
    state: Arc<State<T>>,
    priority: Priority,
}

struct Queue<T> {
    tasks: VecDeque<Task<T>>,
    visible_burst: usize,
}

struct Shared<T> {
    queue: Mutex<Queue<T>>,
    ready: Condvar,
    stopped: Arc<AtomicBool>,
    charges: Arc<Charges>,
    capacity: ExecutorCapacity,
    byte_limits: Option<JobBudget>,
    memory: Option<MemoryPool>,
    _queue_memory: Option<MemoryLease>,
}

/// Trusted CPU tasks must check cancellation regularly and must not block on
/// other pool work, disk, or network. Drop cancels and joins this fixed pool.
pub struct Executor<T> {
    shared: Arc<Shared<T>>,
    workers: Vec<JoinHandle<()>>,
}

impl<T: Send + 'static> Executor<T> {
    pub fn new(config: ExecutorConfig) -> io::Result<Self> {
        Self::from_config(config, None)
    }
    /// Retain independent fixed input/output quotas in addition to the shared pool.
    pub fn with_memory(config: ExecutorConfig, memory: MemoryPool) -> io::Result<Self> {
        Self::from_config(config, Some(memory))
    }
    /// Use the shared pool as the only byte-capacity authority. Every reservation
    /// observes its current hard budget and class limit, including runtime changes.
    /// Input, scratch and result estimates remain required; queue metadata and
    /// cancelled work stay charged. Soft targets still require an owner controller.
    ///
    /// ```
    /// use torse_runtime::{Executor, ExecutorCapacity};
    /// use torse_types::{MemoryBudget, MemoryPool};
    /// let memory = MemoryPool::new(MemoryBudget::new(32 * 1024 * 1024));
    /// let cpu = Executor::<u64>::in_memory_pool(
    ///     ExecutorCapacity { workers: 2, max_jobs: 8 }, memory.clone(),
    /// ).unwrap();
    /// // All executors using this pool observe the new allowance on admission.
    /// memory.set_budget(MemoryBudget::new(64 * 1024 * 1024)).unwrap();
    /// drop(cpu);
    /// assert_eq!(memory.usage().total, 0);
    /// ```
    pub fn in_memory_pool(capacity: ExecutorCapacity, memory: MemoryPool) -> io::Result<Self> {
        capacity.validate()?;
        Self::build(capacity, None, Some(memory))
    }
    fn from_config(config: ExecutorConfig, memory: Option<MemoryPool>) -> io::Result<Self> {
        config.validate()?;
        Self::build(
            ExecutorCapacity {
                workers: config.workers,
                max_jobs: config.max_jobs,
            },
            Some(JobBudget {
                input_bytes: config.max_input_bytes,
                output_bytes: config.max_output_bytes,
            }),
            memory,
        )
    }
    fn build(
        capacity: ExecutorCapacity,
        byte_limits: Option<JobBudget>,
        memory: Option<MemoryPool>,
    ) -> io::Result<Self> {
        let queue_bytes = capacity
            .max_jobs
            .checked_mul(size_of::<Task<T>>())
            .ok_or_else(|| io::Error::other("executor queue size overflow"))?;
        let queue_memory = memory
            .as_ref()
            .map(|pool| {
                pool.reserve(
                    MemoryClass::Executor,
                    queue_bytes as u64,
                    EvictionRank::Pinned,
                )
            })
            .transpose()
            .map_err(io::Error::other)?;
        let mut tasks = VecDeque::new();
        tasks
            .try_reserve_exact(capacity.max_jobs)
            .map_err(io::Error::other)?;
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                tasks,
                visible_burst: 0,
            }),
            ready: Condvar::new(),
            stopped: Arc::new(AtomicBool::new(false)),
            charges: Arc::new(Charges::default()),
            capacity,
            byte_limits,
            memory,
            _queue_memory: queue_memory,
        });
        let mut executor = Self {
            shared,
            workers: Vec::new(),
        };
        for index in 0..capacity.workers {
            let state = Arc::clone(&executor.shared);
            executor.workers.push(
                thread::Builder::new()
                    .name(format!("torse-cpu-{index}"))
                    .spawn(move || worker(state))?,
            );
        }
        Ok(executor)
    }

    /// Reserve before copying input. Busy leaves the demand with the caller.
    /// Byte accounting is conservative and supplied by trusted task adapters.
    pub fn try_reserve(&self, budget: JobBudget) -> Result<Reservation<T>, ReserveError> {
        let shared = &self.shared;
        let _queue = shared.queue.lock().expect("CPU queue lock");
        if shared.stopped.load(Ordering::Acquire) {
            return Err(ReserveError::Stopped);
        }
        let charges = &shared.charges;
        if let Some(limits) = shared.byte_limits {
            if budget.input_bytes > limits.input_bytes || budget.output_bytes > limits.output_bytes
            {
                return Err(ReserveError::TooLarge);
            }
            if budget.input_bytes
                > limits
                    .input_bytes
                    .saturating_sub(charges.input.load(Ordering::Acquire))
                || budget.output_bytes
                    > limits
                        .output_bytes
                        .saturating_sub(charges.output.load(Ordering::Acquire))
            {
                return Err(ReserveError::Busy);
            }
        }
        if charges.jobs.load(Ordering::Acquire) >= shared.capacity.max_jobs {
            return Err(ReserveError::Busy);
        }
        // Admission is serialized, but releases can happen concurrently. A
        // conservative snapshot also protects accounting on narrower hosts.
        if charges
            .input
            .load(Ordering::Acquire)
            .checked_add(budget.input_bytes)
            .is_none()
            || charges
                .output
                .load(Ordering::Acquire)
                .checked_add(budget.output_bytes)
                .is_none()
        {
            return Err(ReserveError::TooLarge);
        }
        let bytes = budget
            .input_bytes
            .checked_add(budget.output_bytes)
            .and_then(|bytes| bytes.checked_add(size_of::<State<T>>()))
            .ok_or(ReserveError::TooLarge)?;
        let memory = shared
            .memory
            .as_ref()
            .map(|pool| {
                let limit = pool.budget();
                if bytes as u64 > limit.class_limit(MemoryClass::Executor)
                    || bytes as u64 > limit.total_bytes()
                {
                    return Err(ReserveError::TooLarge);
                }
                pool.reserve(MemoryClass::Executor, bytes as u64, EvictionRank::Pinned)
                    .map_err(|_| ReserveError::Busy)
            })
            .transpose()?;
        charges.jobs.fetch_add(1, Ordering::AcqRel);
        charges
            .input
            .fetch_add(budget.input_bytes, Ordering::AcqRel);
        charges
            .output
            .fetch_add(budget.output_bytes, Ordering::AcqRel);
        let state = Arc::new(State {
            token: Cancellation {
                local: Arc::new(AtomicBool::new(false)),
                stopped: Arc::clone(&shared.stopped),
            },
            result: Mutex::new(None),
            budget,
            charges: Arc::clone(charges),
            output_bytes: AtomicUsize::new(0),
            memory: Mutex::new(memory),
        });
        Ok(Reservation {
            state,
            shared: Arc::clone(shared),
        })
    }
}

impl<T> Executor<T> {
    pub fn usage(&self) -> Usage {
        let queue = self.shared.queue.lock().expect("CPU queue lock");
        let charges = &self.shared.charges;
        Usage {
            jobs: charges.jobs.load(Ordering::Acquire),
            queued: queue.tasks.len(),
            running: charges.running.load(Ordering::Acquire),
            input_bytes: charges.input.load(Ordering::Acquire),
            output_bytes: charges.output.load(Ordering::Acquire),
        }
    }
}

impl<T> Drop for Executor<T> {
    fn drop(&mut self) {
        let tasks = {
            let mut queue = self.shared.queue.lock().expect("CPU queue lock");
            self.shared.stopped.store(true, Ordering::Release);
            std::mem::take(&mut queue.tasks)
        };
        for task in tasks {
            *task.state.result.lock().expect("job result lock") = Some(Err(JobError::Cancelled));
        }
        self.shared.ready.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

/// Non-cloneable lease; dropping an unsubmitted reservation releases capacity.
pub struct Reservation<T> {
    state: Arc<State<T>>,
    shared: Arc<Shared<T>>,
}

impl<T: Send + 'static> Reservation<T> {
    pub fn submit(
        self,
        priority: Priority,
        work: impl FnOnce(&Cancellation) -> Result<JobOutput<T>, JobError> + Send + 'static,
    ) -> Job<T> {
        let job = Job {
            state: Arc::clone(&self.state),
            shared: Arc::clone(&self.shared),
        };
        let mut queue = self.shared.queue.lock().expect("CPU queue lock");
        if self.shared.stopped.load(Ordering::Acquire) {
            *self.state.result.lock().expect("job result lock") = Some(Err(JobError::Cancelled));
        } else {
            queue.tasks.push_back(Task {
                work: Box::new(work),
                state: Arc::clone(&self.state),
                priority,
            });
            self.shared.ready.notify_one();
        }
        job
    }
}

pub struct Job<T> {
    state: Arc<State<T>>,
    shared: Arc<Shared<T>>,
}

impl<T> Job<T> {
    /// Move a finished payload's existing reservation into its resident class.
    /// A full destination class leaves the result available for a later retry.
    #[allow(clippy::type_complexity)]
    pub fn try_take_charged(
        &mut self,
        class: MemoryClass,
        rank: EvictionRank,
    ) -> Result<Option<Result<(T, Option<MemoryLease>), JobError>>, ReserveError> {
        let mut result = self.state.result.lock().expect("job result lock");
        if result.is_none() {
            return Ok(None);
        }
        if self.state.token.is_cancelled() {
            return Ok(result.take().map(|_| Err(JobError::Cancelled)));
        }
        let lease = if result.as_ref().is_some_and(Result::is_ok) {
            self.state
                .memory
                .lock()
                .expect("job memory lock")
                .as_mut()
                .map(|lease| {
                    let bytes = self.state.output_bytes.load(Ordering::Acquire) as u64;
                    let budget = lease.pool().budget();
                    if bytes > budget.class_limit(class).min(budget.total_bytes()) {
                        return Err(ReserveError::TooLarge);
                    }
                    lease
                        .split(class, bytes, rank)
                        .map_err(|_| ReserveError::Busy)
                })
                .transpose()?
        } else {
            None
        };
        Ok(result
            .take()
            .map(|result| result.map(|value| (value, lease))))
    }
    /// Nonblocking. The caller owns and budgets a taken result separately.
    pub fn try_take(&mut self) -> Option<Result<T, JobError>> {
        self.state
            .result
            .lock()
            .expect("job result lock")
            .take()
            .map(|result| {
                if self.state.token.is_cancelled() {
                    Err(JobError::Cancelled)
                } else {
                    result
                }
            })
    }

    /// Queued input is dropped immediately. Running input remains charged until
    /// the worker observes cancellation and drops it. Late output is discarded.
    /// A retained handle keeps its reservation, even after cancellation.
    pub fn cancel(&self) {
        self.state.token.local.store(true, Ordering::Release);
        let removed = {
            let mut queue = self.shared.queue.lock().expect("CPU queue lock");
            queue
                .tasks
                .iter()
                .position(|t| Arc::ptr_eq(&t.state, &self.state))
                .and_then(|index| queue.tasks.remove(index))
        };
        if removed.is_some() {
            *self.state.result.lock().expect("job result lock") = Some(Err(JobError::Cancelled));
        }
        drop(removed);
    }
}

impl<T> Drop for Job<T> {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn worker<T: Send + 'static>(shared: Arc<Shared<T>>) {
    loop {
        let task = {
            let mut queue = shared.queue.lock().expect("CPU queue lock");
            while queue.tasks.is_empty() && !shared.stopped.load(Ordering::Acquire) {
                queue = shared.ready.wait(queue).expect("CPU queue wait");
            }
            if shared.stopped.load(Ordering::Acquire) {
                return;
            }
            // Eight visible jobs at most before servicing waiting background work.
            let prefer = if queue.visible_burst < 8 {
                Priority::Visible
            } else {
                Priority::Background
            };
            let index = queue
                .tasks
                .iter()
                .position(|t| t.priority == prefer)
                .unwrap_or(0);
            let task = queue.tasks.remove(index).expect("queued task");
            queue.visible_burst = if task.priority == Priority::Visible {
                (queue.visible_burst + 1).min(8)
            } else {
                0
            };
            shared.charges.running.fetch_add(1, Ordering::AcqRel);
            task
        };
        let Task { work, state, .. } = task;
        let result = if state.token.is_cancelled() {
            drop(work);
            Err(JobError::Cancelled)
        } else {
            catch_unwind(AssertUnwindSafe(|| work(&state.token)))
                .unwrap_or(Err(JobError::Panicked))
                .and_then(|output| {
                    if output.bytes > state.budget.output_bytes {
                        Err(JobError::OutputLimit)
                    } else {
                        state.output_bytes.store(output.bytes, Ordering::Release);
                        Ok(output.value)
                    }
                })
        };
        let result = if state.token.is_cancelled() {
            Err(JobError::Cancelled)
        } else {
            result
        };
        *state.result.lock().expect("job result lock") = Some(result);
        shared.charges.running.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
#[path = "executor_tests.rs"]
mod tests;
