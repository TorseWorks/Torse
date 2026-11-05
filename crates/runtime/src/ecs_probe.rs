//! Standalone, instrumented ECS correctness probe, separate from authority state.
//! Permissions, transactions, persistence, and authority scheduling are not modeled.

use std::collections::HashSet;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use bevy_ecs::prelude::*;
use bevy_ecs::schedule::{ExecutorKind, Schedule};
use bevy_tasks::{ComputeTaskPool, TaskPoolBuilder};

#[derive(Component)]
struct ProbeIndex(usize);
#[derive(Component)]
struct Position(u64);
#[derive(Component)]
struct Velocity(u64);
#[derive(Component)]
struct Health(u32);

#[derive(Resource, Default)]
struct Observation {
    threads: Mutex<HashSet<ThreadId>>,
    active: AtomicUsize,
    peak: AtomicUsize,
}

impl Observation {
    fn enter(&self) -> SystemVisit<'_> {
        self.threads
            .lock()
            .expect("probe observation poisoned")
            .insert(thread::current().id());
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        SystemVisit(self)
    }
}

struct SystemVisit<'a>(&'a Observation);
impl Drop for SystemVisit<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn integrate(mut query: Query<(&mut Position, &Velocity)>, observation: Res<Observation>) {
    let _visit = observation.enter();
    for (mut position, velocity) in &mut query {
        position.0 = position.0.wrapping_add(velocity.0);
    }
}

fn regenerate(mut query: Query<&mut Health>, observation: Res<Observation>) {
    let _visit = observation.enter();
    for mut health in &mut query {
        health.0 = health.0.saturating_add(1);
    }
}

/// Explicit execution mode for the isolated probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EcsProbeExecutor {
    /// Run both systems on the calling thread.
    SingleThreaded,
    /// Schedule disjoint systems on Bevy's process-wide compute pool.
    MultiThreaded,
}

/// Observations from a probe run; timing includes observation overhead.
#[derive(Clone, Copy, Debug)]
pub struct EcsProbeReport {
    /// Number of synthetic entities.
    pub entities: usize,
    /// Number of schedule executions.
    pub ticks: u32,
    /// Selected scheduler, independent of observed thread participation.
    pub executor: EcsProbeExecutor,
    /// Existing pool size, or zero for the serial run.
    pub pool_threads: usize,
    /// Distinct threads that actually executed systems, including the caller.
    pub observed_threads: usize,
    /// Distinct system threads other than the caller, not worker indices.
    pub observed_non_caller_threads: usize,
    /// Largest observed number of overlapping system bodies, at most two.
    pub peak_concurrent_systems: usize,
    /// Instrumented schedule time; setup and result verification are excluded.
    pub elapsed: Duration,
    /// Diagnostic sum, not a substitute for per-entity verification.
    pub checksum: u64,
    /// Entities whose full result differs from the analytical reference.
    pub mismatched_entities: usize,
}

#[derive(Debug, Eq, PartialEq)]
struct EntityResult {
    index: usize,
    position: u64,
    velocity: u64,
    health: u32,
}

/// Run the explicit serial reference, preserving the original entry point.
pub fn run_ecs_probe(entities: usize, ticks: u32) -> EcsProbeReport {
    run_ecs_probe_with_executor(entities, ticks, EcsProbeExecutor::SingleThreaded)
}

/// Run and verify an isolated ECS workload with an explicit scheduler.
///
/// A multi-threaded run initializes Bevy's global compute pool with two workers
/// if absent, and otherwise reuses its existing size without reconfiguring it.
/// Pool lifetime is process-wide. Observations can include the calling thread;
/// neither multiple threads nor overlapping systems are guaranteed per run.
/// This is a correctness smoke probe, not a throughput benchmark.
pub fn run_ecs_probe_with_executor(
    entities: usize,
    ticks: u32,
    executor: EcsProbeExecutor,
) -> EcsProbeReport {
    run_probe(entities, ticks, executor).0
}

fn run_probe(
    entities: usize,
    ticks: u32,
    executor: EcsProbeExecutor,
) -> (EcsProbeReport, Vec<EntityResult>) {
    let caller = thread::current().id();
    let pool_threads = match executor {
        EcsProbeExecutor::SingleThreaded => 0,
        EcsProbeExecutor::MultiThreaded => ComputeTaskPool::get_or_init(|| {
            TaskPoolBuilder::new()
                .num_threads(2)
                .thread_name("ecs-probe".into())
                .build()
        })
        .thread_num(),
    };
    let mut world = World::new();
    world.insert_resource(Observation::default());
    for index in 0..entities {
        world.spawn((
            ProbeIndex(index),
            Position(index as u64),
            Velocity((index % 7 + 1) as u64),
            Health(100),
        ));
    }
    let mut schedule = Schedule::default();
    schedule.set_executor_kind(match executor {
        EcsProbeExecutor::SingleThreaded => ExecutorKind::SingleThreaded,
        EcsProbeExecutor::MultiThreaded => ExecutorKind::MultiThreaded,
    });
    schedule.add_systems((integrate, regenerate));
    schedule
        .initialize(&mut world)
        .expect("fixed probe schedule is valid");
    let start = Instant::now();
    for _ in 0..ticks {
        schedule.run(&mut world);
    }
    let elapsed = start.elapsed();
    let observation = world.resource::<Observation>();
    let threads = observation
        .threads
        .lock()
        .expect("probe observation poisoned");
    let observed_threads = threads.len();
    let observed_non_caller_threads = threads.iter().filter(|id| **id != caller).count();
    let peak_concurrent_systems = observation.peak.load(Ordering::SeqCst);
    drop(threads);
    let mut query = world.query::<(&ProbeIndex, &Position, &Velocity, &Health)>();
    let mut results: Vec<_> = query
        .iter(&world)
        .map(|(index, position, velocity, health)| EntityResult {
            index: index.0,
            position: position.0,
            velocity: velocity.0,
            health: health.0,
        })
        .collect();
    results.sort_unstable_by_key(|result| result.index);
    let mismatched_entities = count_mismatches(&results, entities, ticks);
    let checksum = results.iter().fold(0_u64, |sum, result| {
        sum.wrapping_add(result.position)
            .wrapping_add(u64::from(result.health))
    });
    (
        EcsProbeReport {
            entities,
            ticks,
            executor,
            pool_threads,
            observed_threads,
            observed_non_caller_threads,
            peak_concurrent_systems,
            elapsed,
            checksum,
            mismatched_entities,
        },
        results,
    )
}

fn count_mismatches(results: &[EntityResult], entities: usize, ticks: u32) -> usize {
    results
        .iter()
        .enumerate()
        .filter(|(index, result)| **result != expected_result(*index, ticks))
        .count()
        + entities.abs_diff(results.len())
}

fn expected_result(index: usize, ticks: u32) -> EntityResult {
    let velocity = (index % 7 + 1) as u64;
    EntityResult {
        index,
        velocity,
        position: (index as u64).wrapping_add(velocity.wrapping_mul(u64::from(ticks))),
        health: 100_u32.saturating_add(ticks),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_detects_equal_sum_corruption_and_missing_entities() {
        let mut results: Vec<_> = (0..4).map(|index| expected_result(index, 3)).collect();
        results[0].position += 1;
        results[1].position -= 1;
        assert_eq!(count_mismatches(&results, 4, 3), 2);
        results[0] = expected_result(0, 3);
        results[1] = expected_result(1, 3);
        results[2].velocity += 1;
        assert_eq!(count_mismatches(&results, 4, 3), 1);
        results[2] = expected_result(2, 3);
        results.pop();
        assert_eq!(count_mismatches(&results, 4, 3), 1);
        results[1].index = 0;
        assert_eq!(count_mismatches(&results, 4, 3), 2);
    }

    #[test]
    fn both_executors_match_every_entity_and_reference() {
        for entities in [0, 1, 64, 4096, 65_536] {
            for ticks in [0, 1, 4] {
                let (serial, expected) =
                    run_probe(entities, ticks, EcsProbeExecutor::SingleThreaded);
                let (parallel, actual) =
                    run_probe(entities, ticks, EcsProbeExecutor::MultiThreaded);
                assert_eq!(actual, expected, "entities={entities}, ticks={ticks}");
                assert_eq!(serial.mismatched_entities, 0);
                assert_eq!(parallel.mismatched_entities, 0);
                assert_eq!(serial.observed_non_caller_threads, 0);
                assert_eq!(serial.observed_threads, usize::from(ticks != 0));
                assert_eq!(serial.peak_concurrent_systems, usize::from(ticks != 0));
                assert!(parallel.pool_threads > 0);
                if ticks == 0 {
                    assert_eq!(parallel.observed_threads, 0);
                    assert_eq!(parallel.peak_concurrent_systems, 0);
                } else {
                    assert!(parallel.observed_threads > 0);
                    assert!((1..=2).contains(&parallel.peak_concurrent_systems));
                }
            }
        }
    }

    #[test]
    fn original_entry_point_is_an_explicit_serial_reference() {
        let report = run_ecs_probe(128, 4);
        assert_eq!(report.executor, EcsProbeExecutor::SingleThreaded);
        assert_eq!(report.pool_threads, 0);
        assert_eq!(report.mismatched_entities, 0);
        let expected: u64 = (0..128)
            .map(|index| {
                let result = expected_result(index, 4);
                result.position + u64::from(result.health)
            })
            .sum();
        assert_eq!(report.checksum, expected);
    }
}
