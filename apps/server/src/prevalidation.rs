//! Bounded, optional preparation; journal and serial publication remain authoritative.
use super::*;
use crate::{HostCheckpointStore, Server};
use std::collections::BTreeSet;
use torse_runtime::{ExecutorConfig, JobBudget, JobOutput, run_scoped};
use torse_sim::{ComponentPrevalidation, MovementPrevalidation};
use torse_storage::CommitJournal;

/// Limits a joined window of trusted, read-only movement or component preparation.
#[derive(Clone, Copy, Debug)]
pub struct PrevalidationConfig {
    pub workers: usize,
    pub max_in_flight: usize,
    pub max_job_bytes: usize,
    pub min_cells: usize,
    pub components: bool,
    pub min_component_bytes: usize,
}
impl Default for PrevalidationConfig {
    fn default() -> Self {
        Self {
            workers: 4,
            max_in_flight: 16,
            max_job_bytes: 8 * 1024 * 1024,
            min_cells: 512,
            components: false,
            min_component_bytes: 16 * 1024,
        }
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub struct PrevalidationStats {
    pub batches: u64,
    pub prepared: u64,
    pub hits: u64,
    pub stale: u64,
    pub skipped: u64,
    pub peak_jobs: usize,
    pub peak_reserved_bytes: usize,
    /// Retained token charges; components include a conservative work allowance.
    pub peak_cached_bytes: usize,
}
#[derive(Default)]
pub(super) struct PrevalidationCache {
    table: Option<Arc<Vec<FrozenTask>>>,
    point: Option<ResumePoint>,
    start: usize,
    end: usize,
    entries: Vec<Option<Prevalidated>>,
    stats: PrevalidationStats,
    memory: Option<MemoryLease>,
}

enum Prevalidated {
    Movement(MovementPrevalidation),
    Components(ComponentPrevalidation),
}
impl Prevalidated {
    fn charge(&self) -> usize {
        match self {
            Self::Movement(t) => t.allocated_bytes(),
            Self::Components(t) => t.reserved_bytes(),
        }
    }
}

fn candidate(task: &FrozenTask, config: PrevalidationConfig) -> Option<BTreeSet<EntityId>> {
    if let Some(id) = movement_candidate(task, config) {
        return Some([id].into());
    }
    if !config.components || task.status != ReplyStatus::Committed {
        return None;
    }
    let group = task.transaction.as_ref()?;
    if group.members.is_empty()
        || group.members.iter().any(|m| {
            !m.presentation.is_empty()
                || !m.reads.is_empty()
                || !m.writes.is_empty()
                || !m.moves.is_empty()
                || !m.spawns.is_empty()
                || !m.despawns.is_empty()
        })
    {
        return None;
    }
    let estimate =
        ComponentPrevalidation::estimate(group.members.iter().flat_map(|m| &m.components))?;
    if estimate.payload_bytes < config.min_component_bytes
        || estimate.peak_bytes > config.max_job_bytes
    {
        return None;
    }
    let ids: BTreeSet<_> = group
        .members
        .iter()
        .flat_map(|m| m.components.iter().map(|e| e.id))
        .collect();
    (!ids.is_empty()).then_some(ids)
}

fn movement_candidate(task: &FrozenTask, config: PrevalidationConfig) -> Option<EntityId> {
    let group = task.transaction.as_ref()?;
    if task.status != ReplyStatus::Committed || group.members.len() != 1 {
        return None;
    }
    let m = &group.members[0];
    if m.moves.len() != 1
        || !m.presentation.is_empty()
        || !m.reads.is_empty()
        || !m.writes.is_empty()
        || !m.components.is_empty()
        || !m.spawns.is_empty()
        || !m.despawns.is_empty()
    {
        return None;
    }
    let movement = &m.moves[0];
    let half = movement.half.map(f64::from_bits);
    let delta = movement.delta.map(f64::from_bits);
    let mut cells = 1usize;
    for (half, delta) in half.into_iter().zip(delta) {
        if !half.is_finite()
            || half <= 0.0
            || half > torse_sim::movement::MAX_COLLIDER_HALF
            || !delta.is_finite()
            || delta.abs() > torse_sim::movement::MAX_MOVEMENT_DELTA
        {
            return None;
        }
        cells = cells.checked_mul((2.0 * half + delta.abs() + 4.0).ceil() as usize)?;
    }
    // Covers tracked terrain maps, witness vectors, output, and fixed scratch.
    let estimate = cells.checked_mul(256)?.checked_add(65536)?;
    (cells >= config.min_cells && estimate <= config.max_job_bytes).then_some(movement.id)
}

impl ExecutionHost {
    pub fn set_prevalidation(&mut self, config: Option<PrevalidationConfig>) -> Result<(), Fault> {
        if let Some(c) = config
            && (!(1..=64).contains(&c.workers)
                || !(1..=64).contains(&c.max_in_flight)
                || c.max_job_bytes == 0
                || c.max_job_bytes.checked_mul(c.max_in_flight).is_none())
        {
            return Err(Fault::invalid_data("invalid movement prevalidation limits"));
        }
        self.prevalidation = config;
        *self.prevalidated.borrow_mut() = PrevalidationCache::default();
        Ok(())
    }
    pub fn prevalidation_stats(&self) -> PrevalidationStats {
        self.prevalidated.borrow().stats
    }

    pub(super) fn fill_prevalidation(&self) {
        let Some(config) = self.prevalidation else {
            return;
        };
        let Some(work) = &self.runtime.work else {
            return;
        };
        let start = work.next as usize;
        let mut cache = self.prevalidated.borrow_mut();
        let same = cache.point == Some(self.resume_point())
            && cache
                .table
                .as_ref()
                .is_some_and(|t| Arc::ptr_eq(t, &work.tasks));
        if same && (cache.start..cache.end).contains(&start) {
            return;
        }
        cache.entries = Vec::new();
        cache.memory = None;
        cache.table = Some(Arc::clone(&work.tasks));
        cache.point = Some(self.resume_point());
        cache.start = start;
        cache.end = start + 1;
        let mut ids = BTreeSet::new();
        let mut count = 0;
        for task in work.tasks.iter().skip(start).take(config.max_in_flight) {
            let Some(targets) = candidate(task, config) else {
                break;
            };
            if targets.iter().any(|id| ids.contains(id)) {
                break;
            }
            ids.extend(targets);
            count += 1;
        }
        if count < 2 {
            cache.stats.skipped += 1;
            return;
        }
        let mut memory = if let Some(pool) = self.memory_pool() {
            match pool.reserve(
                MemoryClass::Executor,
                (count as u64).saturating_mul(config.max_job_bytes as u64),
                EvictionRank::Pinned,
            ) {
                Ok(lease) => Some(lease),
                Err(_) => {
                    cache.stats.skipped += 1;
                    return;
                }
            }
        } else {
            None
        };
        let world = self.world();
        let tasks = &work.tasks;
        let result = run_scoped(
            ExecutorConfig {
                workers: config.workers,
                max_jobs: count,
                max_input_bytes: count * size_of::<usize>(),
                max_output_bytes: count * config.max_job_bytes,
            },
            JobBudget {
                input_bytes: size_of::<usize>(),
                output_bytes: config.max_job_bytes,
            },
            count,
            |index| {
                let value = tasks[start + index]
                    .transaction
                    .as_ref()
                    .unwrap()
                    .restore(world)
                    .ok()
                    .and_then(|tx| {
                        if tx.members.iter().any(|m| !m.movements.is_empty()) {
                            world
                                .prevalidate_movement(&tx)
                                .ok()
                                .map(Prevalidated::Movement)
                        } else {
                            world
                                .prevalidate_components(&tx)
                                .ok()
                                .map(Prevalidated::Components)
                        }
                    });
                let bytes = value.as_ref().map_or(0, Prevalidated::charge);
                Ok(JobOutput { value, bytes })
            },
        );
        cache.end = start + count;
        cache.stats.batches += 1;
        cache.stats.peak_jobs = cache.stats.peak_jobs.max(count);
        cache.stats.peak_reserved_bytes = cache
            .stats
            .peak_reserved_bytes
            .max(count * config.max_job_bytes);
        if let Ok(results) = result {
            cache.entries = results.into_iter().map(|r| r.ok().flatten()).collect();
        }
        let bytes = cache
            .entries
            .iter()
            .flatten()
            .map(Prevalidated::charge)
            .sum();
        cache.stats.peak_cached_bytes = cache.stats.peak_cached_bytes.max(bytes);
        cache.stats.prepared += cache.entries.iter().flatten().count() as u64;
        let retained = bytes + cache.entries.capacity() * size_of::<Option<Prevalidated>>();
        if let Some(lease) = &mut memory
            && lease.resize(retained as u64).is_err()
        {
            cache.entries = Vec::new();
            return;
        }
        cache.memory = memory;
    }

    pub(super) fn cached_prevalidation(&self) -> Option<torse_sim::PreparedChange> {
        let work = self.runtime.work.as_ref()?;
        let index = work.next as usize;
        let mut cache = self.prevalidated.borrow_mut();
        if cache.point != Some(self.resume_point())
            || !cache
                .table
                .as_ref()
                .is_some_and(|t| Arc::ptr_eq(t, &work.tasks))
        {
            return None;
        }
        let offset = index.checked_sub(cache.start)?;
        let entry = cache.entries.get_mut(offset)?;
        let result = match entry.as_ref()? {
            Prevalidated::Movement(token) => self.world().refresh_movement(token),
            Prevalidated::Components(_) => {
                let Some(Prevalidated::Components(token)) = entry.take() else {
                    unreachable!()
                };
                // Transfer owned values instead of cloning a potentially large tree.
                // If journal reservation blocks, its next attempt prepares normally.
                self.world().refresh_components(token)
            }
        };
        match result {
            Ok(prepared) => {
                cache.stats.hits += 1;
                Some(prepared)
            }
            Err(_) => {
                cache.stats.stale += 1;
                cache.entries[offset] = None;
                None
            }
        }
    }
    pub(super) fn release_prevalidation(&self) {
        let mut cache = self.prevalidated.borrow_mut();
        let same = self.runtime.work.as_ref().filter(|w| {
            cache
                .table
                .as_ref()
                .is_some_and(|t| Arc::ptr_eq(t, &w.tasks))
        });
        if let Some(work) = same {
            let completed = (work.next as usize).saturating_sub(cache.start);
            for entry in cache.entries.iter_mut().take(completed) {
                *entry = None;
            }
        } else {
            cache.entries = Vec::new();
            cache.table = None;
            cache.point = None;
        }
        let retained = cache
            .entries
            .iter()
            .flatten()
            .map(Prevalidated::charge)
            .sum::<usize>()
            + cache.entries.capacity() * size_of::<Option<Prevalidated>>();
        if let Some(lease) = &mut cache.memory {
            let _ = lease.resize(retained as u64);
        }
        if retained == 0 {
            cache.memory = None;
        }
    }
}
impl<J: CommitJournal<ExecutionModel>, S: HostCheckpointStore<ExecutionModel>>
    Server<ExecutionHost, J, S>
{
    pub fn set_prevalidation(&mut self, config: Option<PrevalidationConfig>) -> Result<(), Fault> {
        self.authority()?;
        self.authority.set_prevalidation(config)
    }
}
