//! Owned generation/mesh inputs. Only the scene thread installs their results.
use std::collections::{BTreeMap, BTreeSet};

use torse_render::{Face, MeshBuildError, MeshJob, build_mesh_optimized_bounded};
use torse_runtime::{
    Executor, ExecutorConfig, Job, JobBudget, JobError, JobOutput, Priority, ReserveError,
};
use torse_types::Revision;
use torse_world::{SectionPhase, SectionStore};
use torse_world_services::SectionGenerator;

use super::*;

const TASK_CAP: usize = 8;

fn worker_count(parallelism: usize) -> usize {
    parallelism.saturating_sub(1).clamp(1, 64)
}

pub(super) fn default_config() -> ExecutorConfig {
    ExecutorConfig {
        workers: worker_count(std::thread::available_parallelism().map_or(1, |count| count.get())),
        max_jobs: TASK_CAP,
        max_input_bytes: 8 * 1024 * 1024,
        max_output_bytes: 32 * 1024 * 1024,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MeshStamp {
    pub terrain: MeshJob,
    epoch: WorldEpoch,
    assets: u64,
    tint_provider_generation: u64,
    tint_provider_revision: u64,
    loads: [Option<u64>; 7],
    visual: Option<VisualStamp>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VisualStamp {
    lighting: [[u64; 2]; 7],
    neighbors: [Option<(Revision, u64)>; 27],
}

fn ready_revision(store: &SectionStore, pos: SectionPos) -> Option<Revision> {
    matches!(
        store.phase(pos),
        Some(SectionPhase::Ready | SectionPhase::Active)
    )
    .then(|| store.revision(pos))
    .flatten()
}

impl MeshStamp {
    pub fn same_surface_input(&self, other: &Self) -> bool {
        self.terrain == other.terrain
            && self.epoch == other.epoch
            && self.assets == other.assets
            && self.loads == other.loads
    }
    pub fn capture(scene: &Scene, pos: SectionPos) -> Self {
        let mut loads = [None; 7];
        let positions =
            std::iter::once(Some(pos)).chain(Face::ALL.map(|face| face.neighbor_section(pos)));
        for (slot, pos) in loads.iter_mut().zip(positions) {
            let Some(pos) = pos else {
                continue;
            };
            if ready_revision(scene.authority.store(), pos).is_some() {
                *slot = scene.load_generations.get(&pos).copied();
            }
        }
        Self {
            terrain: MeshJob::capture(scene.side, pos, |p| {
                ready_revision(scene.authority.store(), p)
            }),
            epoch: scene.epoch,
            assets: scene.asset_generation,
            tint_provider_generation: scene.tint_provider_generation,
            tint_provider_revision: scene
                .tint_provider
                .as_ref()
                .map_or(0, |provider| provider.revision(pos)),
            loads,
            visual: scene.lighting.as_ref().map(|lighting| VisualStamp {
                lighting: std::array::from_fn(|index| {
                    let neighbor = if index == 0 {
                        Some(pos)
                    } else {
                        Face::ALL[index - 1].neighbor_section(pos)
                    };
                    neighbor.map_or([0; 2], |p| lighting.stamp(p))
                }),
                neighbors: std::array::from_fn(|index| {
                    let neighbor = SectionPos::new(
                        pos.x.checked_add((index % 3) as i64 - 1)?,
                        pos.y.checked_add((index / 9) as i64 - 1)?,
                        pos.z.checked_add(((index / 3) % 3) as i64 - 1)?,
                    );
                    Some((
                        ready_revision(scene.authority.store(), neighbor)?,
                        scene.load_generations.get(&neighbor).copied().unwrap_or(0),
                    ))
                }),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BackgroundStats {
    pub admitted: u64,
    pub cancelled: u64,
    pub stale: u64,
    pub backpressure: u64,
    pub peak_jobs: usize,
    pub peak_input_bytes: usize,
    pub peak_output_bytes: usize,
}

enum Output {
    Generated(Section),
    Meshed(MeshData),
}

struct Generation {
    epoch: WorldEpoch,
    load: u64,
    version: u32,
    job: Job<Output>,
}

struct Meshing {
    stamp: MeshStamp,
    job: Job<Output>,
}

struct Scan {
    inputs: lighting::Inputs,
    assets: u64,
    tint: u64,
    light: u64,
    meshes: usize,
}

pub(super) struct Background {
    pool: Executor<Output>,
    generating: BTreeMap<SectionPos, Generation>,
    meshing: BTreeMap<SectionPos, Meshing>,
    scanned: Option<Scan>,
    dirty: BTreeSet<SectionPos>,
    stats: BackgroundStats,
    config: ExecutorConfig,
}

impl Background {
    pub fn new(config: ExecutorConfig) -> Result<Self, String> {
        Ok(Self {
            pool: Executor::new(config).map_err(|e| e.to_string())?,
            generating: BTreeMap::new(),
            meshing: BTreeMap::new(),
            scanned: None,
            dirty: BTreeSet::new(),
            stats: BackgroundStats::default(),
            config,
        })
    }
    pub fn replacement_with_memory(
        &self,
        memory: torse_types::MemoryPool,
    ) -> Result<Self, torse_types::Fault> {
        if self.pool.usage().jobs != 0 || !self.generating.is_empty() || !self.meshing.is_empty() {
            return Err(torse_types::Fault::invalid_data(
                "background memory requires an idle executor",
            ));
        }
        Self::with_memory(self.config, memory)
    }
    pub fn with_memory(
        config: ExecutorConfig,
        memory: torse_types::MemoryPool,
    ) -> Result<Self, torse_types::Fault> {
        Ok(Self {
            pool: Executor::with_memory(config, memory)
                .map_err(|error| torse_types::Fault::resource_limit(error.to_string()))?,
            generating: BTreeMap::new(),
            meshing: BTreeMap::new(),
            scanned: None,
            dirty: BTreeSet::new(),
            stats: BackgroundStats::default(),
            config,
        })
    }

    pub fn stats(&self) -> BackgroundStats {
        self.stats
    }

    fn sample_usage(&mut self) {
        let usage = self.pool.usage();
        self.stats.peak_jobs = self.stats.peak_jobs.max(usage.jobs);
        self.stats.peak_input_bytes = self.stats.peak_input_bytes.max(usage.input_bytes);
        self.stats.peak_output_bytes = self.stats.peak_output_bytes.max(usage.output_bytes);
    }

    fn reserve(
        &mut self,
        budget: JobBudget,
    ) -> Result<Option<torse_runtime::Reservation<Output>>, String> {
        match self.pool.try_reserve(budget) {
            Ok(lease) => {
                self.stats.admitted += 1;
                self.sample_usage();
                Ok(Some(lease))
            }
            Err(ReserveError::Busy) => {
                self.stats.backpressure += 1;
                Ok(None)
            }
            Err(error) => Err(format!("background admission failed: {error:?}")),
        }
    }

    pub fn stream(
        &mut self,
        scene: &mut Scene,
        wanted: Vec<SectionPos>,
        budget: usize,
    ) -> Result<(), String> {
        if scene.sections != wanted {
            let membership: std::collections::BTreeSet<_> = wanted.iter().copied().collect();
            self.generating.retain(|pos, _| {
                let keep = membership.contains(pos);
                if !keep {
                    scene.load_generations.remove(pos);
                    self.stats.cancelled += 1;
                }
                keep
            });
            for pos in scene.sections.clone() {
                if !membership.contains(&pos) {
                    if scene.authority.store().phase(pos).is_some() {
                        scene
                            .authority
                            .store_mut()
                            .unload(pos)
                            .map_err(|e| e.to_string())?;
                    }
                    scene.forget_mesh(&pos);
                    scene.load_generations.remove(&pos);
                }
            }
            scene.sections = wanted;
        }
        self.cancel_old_meshes(scene);
        let generator = *scene.generator.as_ref().expect("local generator");
        let version = generator.generation_version();
        let mut completed = 0;
        for pos in scene.sections.clone() {
            if completed >= budget {
                break;
            }
            let Some(task) = self.generating.get_mut(&pos) else {
                continue;
            };
            let Some(result) = task.job.try_take() else {
                continue;
            };
            completed += 1;
            let task = self.generating.remove(&pos).expect("pending generation");
            if task.epoch != scene.epoch
                || task.version != version
                || scene.load_generations.get(&pos) != Some(&task.load)
                || scene.authority.store().phase(pos).is_some()
            {
                self.stats.stale += 1;
                continue;
            }
            match result {
                Ok(Output::Generated(section)) => {
                    if !scene.install_generated(pos, section)? {
                        self.stats.backpressure += 1;
                        return Ok(());
                    }
                }
                Err(JobError::Cancelled) => {}
                other => return Err(format!("generation failed: {}", output_error(other))),
            }
        }
        let mut admitted = 0;
        for pos in scene.sections.clone() {
            if admitted >= budget {
                break;
            }
            if scene.authority.store().phase(pos).is_some() || self.generating.contains_key(&pos) {
                continue;
            }
            let side = scene.side as usize;
            let task_budget = JobBudget {
                input_bytes: size_of::<GenerationParams>() + size_of::<HillTerrain>(),
                output_bytes: size_of::<Section>()
                    + side.pow(3) * size_of::<BlockStateId>()
                    + side.pow(2) * size_of::<i64>(),
            };
            let Some(lease) = self.reserve(task_budget)? else {
                break;
            };
            let load = scene.assign_load_generation(pos)?;
            let job = lease.submit(Priority::Background, move |cancel| {
                let section = generator
                    .generate_cancellable(pos, || cancel.is_cancelled())
                    .map_err(|e| JobError::Failed(e.to_string()))?
                    .ok_or(JobError::Cancelled)?;
                let bytes = size_of::<Section>() + section.allocated_bytes();
                Ok(JobOutput {
                    value: Output::Generated(section),
                    bytes,
                })
            });
            self.generating.insert(
                pos,
                Generation {
                    epoch: scene.epoch,
                    load,
                    version,
                    job,
                },
            );
            admitted += 1;
        }
        Ok(())
    }

    fn cancel_old_meshes(&mut self, scene: &Scene) {
        self.meshing.retain(|pos, task| {
            let keep =
                scene.sections.contains(pos) && task.stamp == MeshStamp::capture(scene, *pos);
            if !keep {
                self.stats.cancelled += 1;
            }
            keep
        });
    }

    pub fn refresh(&mut self, scene: &mut Scene, budget: usize) -> Result<Vec<SectionPos>, String> {
        let mut rebuilt = Vec::new();
        // Check late results before cancelling tasks whose dependencies changed.
        for pos in self.meshing.keys().copied().collect::<Vec<_>>() {
            if rebuilt.len() >= budget {
                break;
            }
            let task = self.meshing.get_mut(&pos).expect("pending mesh");
            if !scene.sections.contains(&pos) || task.stamp != MeshStamp::capture(scene, pos) {
                if let Some(result) = task.job.try_take() {
                    drop(result);
                    self.meshing.remove(&pos);
                    self.stats.stale += 1;
                }
                continue;
            }
            let result = match task.job.try_take_charged(
                torse_types::MemoryClass::Mesh,
                torse_types::EvictionRank::Mesh { distance_sq: 1 },
            ) {
                Ok(Some(result)) => result,
                Ok(None) => continue,
                Err(ReserveError::Busy) => {
                    self.stats.backpressure += 1;
                    let center = scene
                        .memory
                        .as_ref()
                        .map_or(SectionPos::ZERO, |memory| memory.center);
                    scene.evict_one_mesh(center);
                    continue;
                }
                Err(error) => return Err(format!("mesh memory transfer failed: {error:?}")),
            };
            let task = self.meshing.remove(&pos).expect("pending mesh");
            if !scene.sections.contains(&pos) || task.stamp != MeshStamp::capture(scene, pos) {
                self.stats.stale += 1;
                continue;
            }
            match result {
                Ok((Output::Meshed(mesh), lease)) => {
                    scene.install_mesh(pos, task.stamp, mesh, lease);
                    self.dirty.remove(&pos);
                    rebuilt.push(pos);
                }
                Err(JobError::Cancelled) => {}
                Err(error) => return Err(format!("meshing failed: {error:?}")),
                Ok((other, lease)) => {
                    let error = output_error(Ok(other));
                    drop(lease);
                    return Err(format!("meshing failed: {error}"));
                }
            }
        }
        self.cancel_old_meshes(scene);
        let binding = torse_types::Binding {
            world: scene.authority.world(),
            epoch: scene.epoch,
            compatibility: scene.authority.coordinator().compatibility(),
        };
        let light = scene
            .lighting
            .as_ref()
            .map_or(0, |lighting| lighting.generation);
        // Dynamic tint providers expose per-section revisions, so they must
        // retain the normal scan even when terrain and lighting are stable.
        if self.meshing.is_empty()
            && self.dirty.is_empty()
            && scene.tint_provider.is_none()
            && self.scanned.as_ref().is_some_and(|scan| {
                scan.assets == scene.asset_generation
                    && scan.tint == scene.tint_provider_generation
                    && scan.light == light
                    && scan.meshes == scene.meshes.len()
                    && scan.inputs.matches(binding, scene)
            })
        {
            return Ok(rebuilt);
        }
        let inputs = lighting::Inputs::capture(binding, scene);
        let membership: BTreeSet<_> = scene.sections.iter().copied().collect();
        self.dirty.retain(|pos| membership.contains(pos));
        let changed = self
            .scanned
            .as_ref()
            .and_then(|scan| scan.inputs.changed_sections(&inputs));
        let all = self.scanned.as_ref().is_none_or(|scan| {
            scan.assets != scene.asset_generation || scan.tint != scene.tint_provider_generation
        }) || scene.tint_provider.is_some()
            || changed.is_none();
        if all {
            self.dirty.extend(scene.sections.iter().copied());
        } else {
            let mut changed = changed.unwrap_or_default();
            if let Some(lighting) = &scene.lighting {
                changed.extend(
                    lighting.changed_sections(self.scanned.as_ref().map_or(0, |scan| scan.light)),
                );
            }
            for pos in changed {
                for y in -1..=1 {
                    for z in -1..=1 {
                        for x in -1..=1 {
                            if let Some(p) = pos
                                .x
                                .checked_add(x)
                                .zip(pos.y.checked_add(y))
                                .zip(pos.z.checked_add(z))
                                .map(|((x, y), z)| SectionPos::new(x, y, z))
                                && membership.contains(&p)
                            {
                                self.dirty.insert(p);
                            }
                        }
                    }
                }
            }
        }
        self.dirty.extend(
            scene
                .sections
                .iter()
                .copied()
                .filter(|pos| !scene.meshes.contains_key(pos)),
        );
        let mut admitted = 0;
        for pos in scene.sections.clone() {
            if admitted >= budget {
                break;
            }
            if !self.dirty.contains(&pos) || ready_revision(scene.authority.store(), pos).is_none()
            {
                continue;
            }
            if self.meshing.contains_key(&pos) {
                continue;
            }
            let stamp = MeshStamp::capture(scene, pos);
            if scene
                .meshes
                .get(&pos)
                .is_some_and(|(previous, _)| previous == &stamp)
            {
                self.dirty.remove(&pos);
                continue;
            }
            let side = scene.side as usize;
            let max_faces = side.pow(3) * 6;
            // Geometry, two per-vertex tint channels, indices, and two slots.
            let face_bytes = 268;
            let scratch_bytes = torse_render::occupancy_scratch_bytes(scene.side);
            let task_budget = JobBudget {
                input_bytes: (side + 2).pow(3) * size_of::<MeshSample>() + size_of::<MeshInput>(),
                output_bytes: size_of::<MeshData>()
                    + max_faces * face_bytes * 2
                    + 8192
                    + scratch_bytes as usize,
            };
            let scratch = match scene.reserve_memory(
                torse_types::MemoryClass::Mesh,
                scratch_bytes,
                torse_types::EvictionRank::Pinned,
            ) {
                Ok(scratch) => scratch,
                Err(error) if error.code == torse_types::FaultCode::ResourceLimit => {
                    self.stats.backpressure += 1;
                    break;
                }
                Err(error) => return Err(error.to_string()),
            };
            let Some(lease) = self.reserve(task_budget)? else {
                break;
            };
            // Reservation precedes the only owned input copy.
            let input = MeshInput::capture(scene, pos)?;
            let job = lease.submit(Priority::Visible, move |cancel| {
                // Queued and running work own the scratch lease, including
                // while cancellation waits for the worker to finish.
                let _scratch = scratch;
                let mesh = build_mesh_optimized_bounded(
                    input.side,
                    pos,
                    |p| input.sample(p),
                    || cancel.is_cancelled(),
                    max_faces,
                )
                .map_err(|e| {
                    if e == MeshBuildError::Cancelled {
                        JobError::Cancelled
                    } else {
                        JobError::OutputLimit
                    }
                })?;
                let bytes = super::memory::mesh_bytes(&mesh);
                Ok(JobOutput {
                    value: Output::Meshed(mesh),
                    bytes,
                })
            });
            self.meshing.insert(pos, Meshing { stamp, job });
            admitted += 1;
        }
        self.scanned = Some(Scan {
            inputs,
            assets: scene.asset_generation,
            tint: scene.tint_provider_generation,
            light,
            meshes: scene.meshes.len(),
        });
        Ok(rebuilt)
    }
}

fn output_error(result: Result<Output, JobError>) -> String {
    match result {
        Err(e) => format!("{e:?}"),
        Ok(_) => "unexpected CPU result type".to_owned(),
    }
}

struct MeshInput {
    side: u32,
    origin: BlockPos,
    cells: Vec<MeshSample>,
}

impl MeshInput {
    fn capture(scene: &Scene, pos: SectionPos) -> Result<Self, String> {
        let origin = pos
            .to_block(LocalPos { x: 0, y: 0, z: 0 }, scene.side)
            .map_err(|e| e.to_string())?;
        let extent = scene.side as usize + 2;
        let mut cells = Vec::with_capacity(extent.pow(3));
        let view = scene.lighting.as_ref().map(|lighting| lighting.view(scene));
        for y in -1..=i64::from(scene.side) {
            for z in -1..=i64::from(scene.side) {
                for x in -1..=i64::from(scene.side) {
                    cells.push(match origin.checked_add(x, y, z) {
                        Ok(p) => sample_visual(scene, view.as_ref(), p),
                        Err(_) => MeshSample::OutOfBounds,
                    });
                }
            }
        }
        Ok(Self {
            side: scene.side,
            origin,
            cells,
        })
    }

    fn sample(&self, pos: BlockPos) -> MeshSample {
        let extent = self.side as usize + 2;
        let x = (pos.x - self.origin.x + 1) as usize;
        let y = (pos.y - self.origin.y + 1) as usize;
        let z = (pos.z - self.origin.z + 1) as usize;
        self.cells[x + extent * (z + extent * y)]
    }
}

#[cfg(test)]
#[path = "background_tests.rs"]
mod tests;
