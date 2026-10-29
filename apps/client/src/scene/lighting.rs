//! Incremental derived lighting. The scene thread alone validates and publishes.
use super::*;
use std::cell::RefCell;
use torse_types::{Binding, EvictionRank, MemoryClass, MemoryLease, MemoryPool};
use torse_world_services::lighting::{LightJob, LightLimits, LightSection};
mod preview;
pub(super) const PREVIEW_BYTES: u64 = 2 * 1024 * 1024;

const WORK_BYTES: u64 = 16 * 1024 * 1024;
const CACHE_BYTES: usize = 64 * 1024 * 1024;
const LIMITS: LightLimits = LightLimits {
    cells: 1_000_000,
    dependencies: 1024,
    frontier: 1_000_000,
    bytes: WORK_BYTES,
};

pub(super) struct Inputs {
    binding: Binding,
    content: torse_types::Digest,
    side: u32,
    bounds: WorldBounds,
    wanted: Vec<SectionPos>,
    observations: Vec<(SectionPos, Option<torse_types::Revision>, bool, Option<u64>)>,
}
impl Inputs {
    pub fn changed_sections(&self, next: &Self) -> Option<Vec<SectionPos>> {
        if self.binding != next.binding
            || self.content != next.content
            || self.side != next.side
            || self.bounds != next.bounds
        {
            return None;
        }
        let mut changed = Vec::new();
        let (mut a, mut b) = (0, 0);
        while a < self.observations.len() || b < next.observations.len() {
            match (self.observations.get(a), next.observations.get(b)) {
                (Some(old), Some(new)) if old.0 == new.0 => {
                    if old != new {
                        changed.push(old.0);
                    }
                    a += 1;
                    b += 1;
                }
                (Some(old), Some(new)) if old.0 < new.0 => {
                    changed.push(old.0);
                    a += 1;
                }
                (Some(_), Some(new)) | (None, Some(new)) => {
                    changed.push(new.0);
                    b += 1;
                }
                (Some(old), None) => {
                    changed.push(old.0);
                    a += 1;
                }
                (None, None) => break,
            }
        }
        Some(changed)
    }
    fn observation(
        scene: &Scene,
        pos: SectionPos,
    ) -> (SectionPos, Option<torse_types::Revision>, bool, Option<u64>) {
        let store = scene.authority.store();
        (
            pos,
            store.revision(pos),
            matches!(
                store.phase(pos),
                Some(torse_world::SectionPhase::Ready | torse_world::SectionPhase::Active)
            ),
            scene.load_generations.get(&pos).copied(),
        )
    }
    pub fn capture(binding: Binding, scene: &Scene) -> Self {
        Self {
            binding,
            content: scene.authority.registry().authority_digest(),
            side: scene.side,
            bounds: scene.authority.store().bounds(),
            wanted: scene.sections.clone(),
            observations: scene
                .authority
                .store()
                .positions()
                .map(|pos| Self::observation(scene, pos))
                .collect(),
        }
    }
    pub fn matches(&self, binding: Binding, scene: &Scene) -> bool {
        self.binding == binding
            && self.content == scene.authority.registry().authority_digest()
            && self.side == scene.side
            && self.bounds == scene.authority.store().bounds()
            && self.wanted == scene.sections
            && self.observations.len() == scene.authority.store().len()
            && self
                .observations
                .iter()
                .zip(scene.authority.store().positions())
                .all(|(old, pos)| *old == Self::observation(scene, pos))
    }
}

struct Pending {
    target: SectionPos,
    kind: usize,
    job: Option<LightJob>,
    result: Option<LightSection>,
    _memory: Option<MemoryLease>,
}
struct Cached {
    // Presentation keeps the previous complete image while a replacement is
    // calculated. This is never used by authoritative lighting queries.
    data: LightSection,
    levels: Box<[u8]>,
    current: bool,
    load: Option<u64>,
    _memory: Option<MemoryLease>,
    generation: u64,
}
impl Cached {
    fn bytes(&self) -> usize {
        self.data.allocated_bytes() + self.levels.len() + 256
    }
}
pub(super) struct Lighting {
    binding: Binding,
    entries: [BTreeMap<SectionPos, Cached>; 2],
    cache_bytes: [usize; 2],
    pending: Option<Pending>,
    urgent: [Option<SectionPos>; 27],
    memory: Option<MemoryPool>,
    inputs: Option<Inputs>,
    pub generation: u64,
}
impl Lighting {
    pub fn changed_sections(&self, since: u64) -> impl Iterator<Item = SectionPos> + '_ {
        self.entries.iter().flat_map(move |entries| {
            entries
                .iter()
                .filter_map(move |(pos, entry)| (entry.generation > since).then_some(*pos))
        })
    }
    pub fn new(binding: Binding, memory: Option<MemoryPool>) -> Result<Self, String> {
        Ok(Self {
            binding,
            entries: [BTreeMap::new(), BTreeMap::new()],
            cache_bytes: [0; 2],
            pending: None,
            urgent: [None; 27],
            memory,
            inputs: None,
            generation: 0,
        })
    }
    fn advance_generation(&mut self) -> Result<(), String> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or("visual lighting generation exhausted")?;
        Ok(())
    }
    pub fn step(&mut self, scene: &Scene, work: usize) -> Result<(), String> {
        let store = scene.authority.store();
        let registry = scene.authority.registry();
        if !self
            .inputs
            .as_ref()
            .is_some_and(|inputs| inputs.matches(self.binding, scene))
        {
            let next = Inputs::capture(self.binding, scene);
            let changed = self
                .inputs
                .as_ref()
                .and_then(|old| old.changed_sections(&next));
            let halo = u64::from(torse_world_services::lighting::MAX_LIGHT)
                .div_ceil(u64::from(scene.side));
            let wanted: std::collections::BTreeSet<_> = scene.sections.iter().copied().collect();
            for kind in 0..2 {
                let removed: Vec<_> = self.entries[kind]
                    .iter()
                    .filter_map(|(pos, entry)| {
                        (!wanted.contains(pos)
                            || entry.load != scene.load_generations.get(pos).copied())
                        .then_some(*pos)
                    })
                    .collect();
                for pos in removed {
                    self.advance_generation()?;
                    if let Some(old) = self.entries[kind].remove(&pos) {
                        self.cache_bytes[kind] -= old.bytes();
                    }
                }
                // Every source lies in this conservative transport footprint.
                // Invalidate by the changed source set; unchanged caches need
                // no repeated traversal of their overlapping dependency lists.
                // Reloads invalidate even if a source reuses its old revision.
                for (target, entry) in &mut self.entries[kind] {
                    if changed.as_ref().is_none_or(|changed| {
                        changed.iter().any(|pos| {
                            pos.x.abs_diff(target.x) <= halo
                                && pos.z.abs_diff(target.z) <= halo
                                && (kind == 1 || pos.y.abs_diff(target.y) <= halo)
                        })
                    }) {
                        entry.current = false;
                    }
                }
            }
            self.inputs = Some(next);
        }
        if let Some(pending) = self.pending.as_mut() {
            if !scene.sections.contains(&pending.target) {
                self.pending = None;
                return Ok(());
            }
            if let Some(job) = pending.job.as_mut() {
                match job.poll(self.binding, store, registry, work) {
                    Ok(false) => return Ok(()),
                    Err(_) => {
                        self.pending = None;
                        return Ok(());
                    }
                    Ok(true) => {
                        pending.result = Some(
                            pending
                                .job
                                .take()
                                .expect("active light job")
                                .finish(self.binding, store, registry)
                                .map_err(|e| e.to_string())?,
                        );
                    }
                }
            }
            let result = pending.result.as_ref().expect("resolved light result");
            if !result.validate(self.binding, store, registry) {
                self.pending = None;
                return Ok(());
            }
            let previous_bytes = self.entries[pending.kind]
                .get(&pending.target)
                .map_or(0, Cached::bytes);
            let result_bytes = result.allocated_bytes() + result.levels().len() + 256;
            let bytes = self.cache_bytes[pending.kind] - previous_bytes + result_bytes;
            if bytes > CACHE_BYTES {
                return Ok(());
            }
            let lease = match self
                .memory
                .as_ref()
                .map(|pool| {
                    pool.reserve(MemoryClass::Mesh, result_bytes as u64, EvictionRank::Pinned)
                })
                .transpose()
            {
                Ok(lease) => lease,
                Err(_) => return Ok(()),
            };
            let changed = self.entries[pending.kind].get(&pending.target).map_or_else(
                || {
                    pending.kind == 1
                        || result
                            .levels()
                            .iter()
                            .any(|v| *v != if pending.kind == 1 { 15 } else { 0 })
                },
                |old| old.levels.as_ref() != result.levels(),
            );
            let generation = if changed {
                self.advance_generation()?;
                self.generation
            } else {
                self.entries[pending.kind]
                    .get(&pending.target)
                    .map_or(0, |old| old.generation)
            };
            let pending = self.pending.take().expect("light publication");
            self.cache_bytes[pending.kind] = bytes;
            self.entries[pending.kind].insert(
                pending.target,
                Cached {
                    levels: pending
                        .result
                        .as_ref()
                        .expect("resolved light result")
                        .levels()
                        .into(),
                    data: pending.result.expect("resolved light result"),
                    current: true,
                    load: scene.load_generations.get(&pending.target).copied(),
                    _memory: lease,
                    generation,
                },
            );
            return Ok(());
        }
        for target in self
            .urgent
            .iter()
            .flatten()
            .filter(|target| scene.sections.contains(target))
            .chain(scene.sections.iter())
        {
            if store.revision(*target).is_none()
                || !matches!(
                    store.phase(*target),
                    Some(torse_world::SectionPhase::Ready | torse_world::SectionPhase::Active)
                )
            {
                continue;
            }
            for kind in 0..2 {
                if self.entries[kind]
                    .get(target)
                    .is_some_and(|entry| entry.current)
                {
                    continue;
                }
                let lease = match self
                    .memory
                    .as_ref()
                    .map(|pool| {
                        pool.reserve(MemoryClass::Executor, WORK_BYTES, EvictionRank::Pinned)
                    })
                    .transpose()
                {
                    Ok(lease) => lease,
                    Err(_) => return Ok(()),
                };
                let job = if kind == 0 {
                    LightJob::prepare(self.binding, store, registry, *target, LIMITS)
                } else {
                    LightJob::prepare_sky(self.binding, store, registry, *target, LIMITS)
                };
                let job = match job {
                    Ok(job) => job,
                    Err(_) => return Ok(()),
                };
                self.pending = Some(Pending {
                    target: *target,
                    kind,
                    job: Some(job),
                    result: None,
                    _memory: lease,
                });
                return Ok(());
            }
        }
        Ok(())
    }
    pub fn complete(&self, scene: &Scene) -> bool {
        self.pending.is_none()
            && scene.sections.iter().all(|pos| {
                scene.authority.store().revision(*pos).is_some()
                    && self
                        .entries
                        .iter()
                        .all(|entries| entries.get(pos).is_some_and(|entry| entry.current))
            })
    }
    pub fn view<'a>(&'a self, scene: &Scene) -> View<'a> {
        View {
            grids: [&self.entries[0], &self.entries[1]],
            side: scene.side,
            bounds: scene.authority.store().bounds(),
            direct_sky: RefCell::new(BTreeMap::new()),
        }
    }
    pub fn stamp(&self, target: SectionPos) -> [u64; 2] {
        std::array::from_fn(|kind| {
            self.entries[kind]
                .get(&target)
                .map_or(0, |entry| entry.generation)
        })
    }
}
pub(super) struct View<'a> {
    grids: [&'a BTreeMap<SectionPos, Cached>; 2],
    side: u32,
    bounds: WorldBounds,
    // One immutable mesh snapshot shares bounded fallback column samples.
    direct_sky: RefCell<BTreeMap<(i64, i64), Vec<u8>>>,
}
impl View<'_> {
    fn sky(&self, scene: &Scene, pos: BlockPos) -> u8 {
        if let Some(level) = self.light(1, pos) {
            return level;
        }
        // Missing derived data must not imply daylight underground. This
        // presentation fallback only admits direct sky through known cells;
        // exact jobs remain responsible for lateral propagation and queries.
        let Ok(height) =
            usize::try_from(i128::from(self.bounds.max_y) - i128::from(self.bounds.min_y))
        else {
            return 0;
        };
        if height > 256 {
            return 0;
        }
        let mut columns = self.direct_sky.borrow_mut();
        let column = (pos.x, pos.z);
        if !columns.contains_key(&column) {
            if columns.len() == 512 {
                return 0;
            }
            let mut levels = vec![0; height];
            let mut level = 15u8;
            for index in (0..height).rev() {
                let y = self.bounds.min_y + index as i64;
                let absorption = match scene
                    .authority
                    .store()
                    .query(BlockPos::new(pos.x, y, pos.z))
                {
                    TerrainQuery::Ready(state) if state.is_empty() => 0,
                    TerrainQuery::Ready(state) => scene
                        .authority
                        .registry()
                        .definition(state)
                        .map_or(15, |definition| {
                            (definition.light_transport.absorption * 15.0)
                                .ceil()
                                .clamp(0.0, 15.0) as u8
                        }),
                    _ => 15,
                };
                level = level.saturating_sub(absorption);
                levels[index] = level;
                if level == 0 {
                    break;
                }
            }
            columns.insert(column, levels);
        }
        columns[&column][(pos.y - self.bounds.min_y) as usize]
    }
    fn light(&self, kind: usize, pos: BlockPos) -> Option<u8> {
        if !self.bounds.contains_y(pos.y) {
            return Some(if kind == 1 && pos.y >= self.bounds.max_y {
                15
            } else {
                0
            });
        }
        let Ok((section, local)) = pos.to_section(self.side) else {
            return None;
        };
        self.grids[kind]
            .get(&section)
            .map(|entry| entry.levels[torse_world::linear_index(self.side, local)])
    }
    pub fn shade(&self, scene: &Scene, pos: BlockPos, block: &mut RenderBlock) {
        if block.cross {
            // Both sides of thin foliage share the open cell's illumination.
            // Opaque neighboring cells do not describe either diagonal plane.
            let light = (f32::from(self.light(0, pos).unwrap_or(0)) / 15.0)
                .max(f32::from(self.sky(scene, pos)) / 15.0)
                .max(0.08);
            block.face_shades = Some([[(light * 255.0).round() as u8; 4]; 6]);
            return;
        }
        let mut faces = [[255; 4]; 6];
        for face in torse_render::Face::ALL {
            let normal = face.normal();
            let axis = normal.iter().position(|n| *n != 0.0).expect("axis face");
            let (a, b) = match axis {
                0 => (1, 2),
                1 => (0, 2),
                _ => (0, 1),
            };
            // Sample just outside the actual face plane. An inset slab face
            // opens into air in its own cell, not into the opaque cell below
            // or above it. Keep the offset local to avoid large-coordinate
            // rounding when locating the sample cell.
            let bounds = block.bounds.unwrap_or(RenderBox::FULL_CUBE);
            let plane = if normal[axis] < 0.0 {
                bounds.min[axis]
            } else {
                bounds.max[axis]
            };
            let mut offset = [0; 3];
            offset[axis] = (f64::from(plane) + f64::from(normal[axis]) * 1e-4).floor() as i64;
            let [dx, dy, dz] = offset;
            let Ok(outside) = pos.checked_add(dx, dy, dz) else {
                continue;
            };
            if block.bounds == Some(RenderBox::FULL_CUBE)
                && !block.cross
                && let TerrainQuery::Ready(state) = scene.authority.store().query(outside)
                && scene.authority.registry().occlusion_mask(state) == 0x3f
            {
                continue;
            }
            let emission = self
                .light(0, pos)
                .unwrap_or(0)
                .max(self.light(0, outside).unwrap_or(0));
            let sky = self.sky(scene, outside);
            let directional = match face {
                torse_render::Face::PosY => 1.0,
                torse_render::Face::NegY => 0.5,
                torse_render::Face::NegX | torse_render::Face::PosX => 0.6,
                _ => 0.8,
            };
            // Match face_quad winding so shared corners sample the same cells.
            let corners = match face {
                torse_render::Face::NegX => [[0, 0], [0, 1], [1, 1], [1, 0]],
                torse_render::Face::PosX => [[0, 1], [0, 0], [1, 0], [1, 1]],
                torse_render::Face::NegY => [[0, 0], [1, 0], [1, 1], [0, 1]],
                torse_render::Face::PosY => [[0, 1], [1, 1], [1, 0], [0, 0]],
                torse_render::Face::NegZ => [[1, 0], [0, 0], [0, 1], [1, 1]],
                torse_render::Face::PosZ => [[0, 0], [1, 0], [1, 1], [0, 1]],
            };
            for (corner, [u, v]) in corners.into_iter().enumerate() {
                let mut da = [0i64; 3];
                da[a] = if u == 0 { -1 } else { 1 };
                let mut db = [0i64; 3];
                db[b] = if v == 0 { -1 } else { 1 };
                let covered = |offset: [i64; 3]| {
                    let Ok(p) = outside.checked_add(offset[0], offset[1], offset[2]) else {
                        return true;
                    };
                    match scene.authority.store().query(p) {
                        TerrainQuery::Ready(state) => {
                            scene.authority.registry().occlusion_mask(state) == 0x3f
                        }
                        TerrainQuery::NotLoaded => true,
                        TerrainQuery::OutOfBounds => false,
                    }
                };
                let sa = covered(da);
                let sb = covered(db);
                let occlusion = if sa && sb {
                    3
                } else {
                    u8::from(sa)
                        + u8::from(sb)
                        + u8::from(covered(std::array::from_fn(|i| da[i] + db[i])))
                };
                let ao = 1.0 - f32::from(occlusion) * 0.15;
                let light = (f32::from(emission) / 15.0)
                    .max(f32::from(sky) / 15.0 * directional)
                    .max(0.08)
                    * ao;
                faces[face.index() as usize][corner] = (light * 255.0).round() as u8;
            }
        }
        block.face_shades = Some(faces);
    }
}

impl Scene {
    // The caller reserves PREVIEW_BYTES before committing an edit. Preview
    // propagation is disposable presentation; exact jobs still validate the
    // committed terrain and replace it with a complete result.
    pub(super) fn preview_lighting(&mut self, positions: &[BlockPos]) -> Result<(), String> {
        if positions.is_empty() {
            return Ok(());
        }
        if let Some(mut lighting) = self.lighting.take() {
            // Local edits get exact relighting before unrelated streaming work.
            // The fixed neighborhood cannot grow with edit count or view range.
            lighting.urgent.fill(None);
            if let Some(center) = positions
                .first()
                .and_then(|p| p.to_section(self.side).ok())
                .map(|p| p.0)
            {
                let mut index = 0;
                for y in -1..=1 {
                    for z in -1..=1 {
                        for x in -1..=1 {
                            lighting.urgent[index] = center
                                .x
                                .checked_add(x)
                                .zip(center.y.checked_add(y))
                                .zip(center.z.checked_add(z))
                                .map(|((x, y), z)| SectionPos::new(x, y, z));
                            index += 1;
                        }
                    }
                }
                lighting.urgent.swap(0, 13);
            }
            lighting.pending = None;
            let result = lighting.preview_opened(self, positions);
            self.lighting = Some(lighting);
            result?;
        }
        Ok(())
    }

    pub fn enable_lighting(&mut self) -> Result<(), String> {
        if self.lighting.is_none() {
            // Direct skylight needs the columns above the visible terrain.
            // The built-in generator owns this finite vertical extent.
            if self.generator.is_some() {
                let bounds = self.authority.store().bounds();
                self.layers =
                    ((bounds.max_y - bounds.min_y) as u64).div_ceil(u64::from(self.side)) as i64;
                self.resize_visual_control().map_err(|e| e.to_string())?;
                if self.background.is_none() {
                    self.load_around(SectionPos::ZERO)?;
                }
            }
            let binding = Binding {
                world: self.authority.world(),
                epoch: self.epoch,
                compatibility: self.authority.coordinator().compatibility(),
            };
            self.lighting = Some(Lighting::new(
                binding,
                self.memory.as_ref().map(|m| m.pool.clone()),
            )?);
            let positions: Vec<_> = self.meshes.keys().copied().collect();
            for pos in positions {
                self.forget_mesh(&pos);
            }
        }
        Ok(())
    }
    pub fn refresh_lighting(&mut self, work: usize) -> Result<bool, String> {
        let Some(mut lighting) = self.lighting.take() else {
            return Ok(true);
        };
        if lighting.binding.epoch != self.epoch {
            let binding = Binding {
                world: self.authority.world(),
                epoch: self.epoch,
                compatibility: self.authority.coordinator().compatibility(),
            };
            lighting = Lighting::new(binding, self.memory.as_ref().map(|m| m.pool.clone()))?;
        }
        let result = lighting.step(self, work);
        let complete = lighting.complete(self);
        self.lighting = Some(lighting);
        result.map(|()| complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scene() -> Scene {
        let mut scene = Scene::new(0).unwrap();
        let mut pack = content_pack();
        for block in &mut pack.blocks {
            if block.value.id == key("base:stone") {
                block.value.light_transport.absorption = 1.0;
            }
            if block.value.id == key("base:glass") {
                block.value.light_transport.emission = 1.0;
                block.value.light_transport.absorption = 0.0;
            }
        }
        let content = compile(pack).unwrap();
        let registry = BlockStateRegistry::build(&content).unwrap();
        let (palette, materials) =
            material_tables(&registry, &crate::texture::base_pack().unwrap()).unwrap();
        scene.authority = WorldAuthority::new(
            WorldId(3),
            registry,
            ComponentSchemas::build(&content),
            16,
            WorldBounds::new(0, 16).unwrap(),
            Compatibility {
                authority_schema: 4,
                rules_digest: content.authority_digest,
                content_digest: content.presentation_digest,
            },
        )
        .unwrap();
        scene.palette = palette;
        scene.state_materials = materials;
        scene.generator = None;
        scene.load_generations.clear();
        scene.sections = vec![SectionPos::ZERO, SectionPos::new(1, 0, 0)];
        for x in -1..=2 {
            for z in -1..=1 {
                assert!(
                    scene
                        .install_generated(SectionPos::new(x, 0, z), Section::empty(16).unwrap())
                        .unwrap()
                );
            }
        }
        scene
    }
    fn resolve(scene: &mut Scene) {
        for _ in 0..200 {
            if scene.refresh_lighting(16_384).unwrap() {
                return;
            }
        }
        panic!("derived light did not finish");
    }
    #[test]
    fn first_full_sky_grid_invalidates_a_previously_conservative_preview() {
        let mut scene = Scene::new(0).unwrap();
        scene.generator = None;
        scene
            .authority
            .store_mut()
            .unload(SectionPos::ZERO)
            .unwrap();
        scene
            .install_generated(SectionPos::ZERO, Section::empty(16).unwrap())
            .unwrap();
        let plant = scene
            .authority
            .registry()
            .default_state(&key("base:plant"))
            .unwrap();
        scene
            .authority
            .store_mut()
            .edit(BlockPos::new(8, 8, 8), plant)
            .unwrap();
        scene.enable_lighting().unwrap();
        scene.refresh_meshes();
        assert!(
            scene.meshes[&SectionPos::ZERO]
                .1
                .shades
                .iter()
                .all(|v| *v < 0.2)
        );
        for y in 1..4 {
            scene
                .install_generated(SectionPos::new(0, y, 0), Section::empty(16).unwrap())
                .unwrap();
        }
        resolve(&mut scene);
        assert!(scene.lighting.as_ref().unwrap().stamp(SectionPos::ZERO)[1] > 0);
        assert_eq!(scene.refresh_meshes(), [SectionPos::ZERO]);
        assert!(
            scene.meshes[&SectionPos::ZERO]
                .1
                .shades
                .iter()
                .all(|v| *v > 0.8)
        );
    }
    #[test]
    fn uncomputed_neighbor_does_not_turn_a_sealed_tunnel_into_daylight() {
        let mut scene = scene();
        let stone = scene
            .authority
            .registry()
            .default_state(&key("base:stone"))
            .unwrap();
        for x in -16..48 {
            for z in -16..32 {
                scene
                    .authority
                    .store_mut()
                    .edit(BlockPos::new(x, 12, z), stone)
                    .unwrap();
            }
        }
        for x in [15, 16] {
            for y in [1, 13] {
                scene
                    .authority
                    .store_mut()
                    .edit(BlockPos::new(x, y, 8), stone)
                    .unwrap();
            }
        }
        scene.enable_lighting().unwrap();
        resolve(&mut scene);
        // A newly visible neighboring section has terrain before its light job finishes.
        scene.lighting.as_mut().unwrap().entries[1].remove(&SectionPos::new(1, 0, 0));
        for x in [15, 16] {
            let underground = block(&scene, BlockPos::new(x, 1, 8)).face_shades.unwrap()[4];
            assert!(
                underground.iter().all(|value| *value < 40),
                "x={x}: {underground:?}"
            );
            let outdoors = block(&scene, BlockPos::new(x, 13, 8)).face_shades.unwrap()[4];
            assert!(
                outdoors.iter().all(|value| *value > 100),
                "x={x}: {outdoors:?}"
            );
        }
    }
    #[test]
    fn crossed_foliage_samples_its_open_cell_equally_for_both_planes() {
        let mut scene = scene();
        let stone = scene
            .authority
            .registry()
            .default_state(&key("base:stone"))
            .unwrap();
        let plant = scene
            .authority
            .registry()
            .default_state(&key("base:plant"))
            .unwrap();
        for x in [15, 16] {
            scene
                .authority
                .store_mut()
                .edit(BlockPos::new(x, 8, 8), plant)
                .unwrap();
            scene
                .authority
                .store_mut()
                .edit(BlockPos::new(x - 1, 8, 8), stone)
                .unwrap();
        }
        // Restore the first plant overwritten by the second plant's neighbor.
        scene
            .authority
            .store_mut()
            .edit(BlockPos::new(15, 8, 8), plant)
            .unwrap();
        scene
            .authority
            .store_mut()
            .edit(BlockPos::new(17, 8, 8), stone)
            .unwrap();
        scene.enable_lighting().unwrap();
        resolve(&mut scene);
        for x in [15, 16] {
            let sample = block(&scene, BlockPos::new(x, 8, 8));
            let shades = sample.face_shades.unwrap();
            assert_eq!(shades[0], shades[1]);
            assert!(
                shades[0].iter().all(|value| *value > 200),
                "x={x}: {shades:?}"
            );
        }
    }
    #[test]
    fn reloaded_external_halo_invalidates_reused_lighting_validation() {
        let mut scene = scene();
        let stone = scene
            .authority
            .registry()
            .default_state(&key("base:stone"))
            .unwrap();
        let glass = scene
            .authority
            .registry()
            .default_state(&key("base:glass"))
            .unwrap();
        for x in -16..48 {
            for z in -16..32 {
                scene
                    .authority
                    .store_mut()
                    .edit(BlockPos::new(x, 12, z), stone)
                    .unwrap();
            }
        }
        let wall = BlockPos::new(30, 8, 8);
        scene.authority.store_mut().edit(wall, stone).unwrap();
        scene.enable_lighting().unwrap();
        resolve(&mut scene);
        assert!(
            block(&scene, wall).face_shades.unwrap()[1]
                .iter()
                .all(|value| *value < 40)
        );
        for _ in 0..4 {
            assert!(scene.refresh_lighting(1).unwrap());
        }
        let external = SectionPos::new(2, 0, 0);
        scene.authority.store_mut().unload(external).unwrap();
        scene.load_generations.remove(&external);
        let mut section = Section::empty(16).unwrap();
        section.set(LocalPos { x: 0, y: 8, z: 8 }, glass).unwrap();
        scene.install_generated(external, section).unwrap();
        resolve(&mut scene);
        assert!(
            block(&scene, wall).face_shades.unwrap()[1]
                .iter()
                .all(|value| *value > 200)
        );
    }
    fn block(scene: &Scene, pos: BlockPos) -> RenderBlock {
        let view = scene.lighting.as_ref().unwrap().view(scene);
        let MeshSample::Ready(block) = sample_visual(scene, Some(&view), pos) else {
            panic!("loaded render sample");
        };
        block
    }
    #[test]
    fn emission_and_sky_reach_vertex_brightness_and_source_removal_rebuilds() {
        let mut scene = scene();
        let stone = scene
            .authority
            .registry()
            .default_state(&key("base:stone"))
            .unwrap();
        let glass = scene
            .authority
            .registry()
            .default_state(&key("base:glass"))
            .unwrap();
        for x in -16..48 {
            for z in -16..32 {
                scene
                    .authority
                    .store_mut()
                    .edit(BlockPos::new(x, 12, z), stone)
                    .unwrap();
            }
        }
        let source = BlockPos::new(8, 8, 8);
        let wall = BlockPos::new(9, 8, 8);
        scene.authority.store_mut().edit(source, glass).unwrap();
        scene.authority.store_mut().edit(wall, stone).unwrap();
        scene.enable_lighting().unwrap();
        resolve(&mut scene);
        let before = block(&scene, wall).face_shades.unwrap()[0];
        assert!(before.iter().all(|v| *v > 200));
        scene.refresh_meshes();
        let stamp = MeshStamp::capture(&scene, SectionPos::ZERO);
        scene
            .authority
            .store_mut()
            .edit(source, BlockStateId::EMPTY)
            .unwrap();
        resolve(&mut scene);
        assert_ne!(stamp, MeshStamp::capture(&scene, SectionPos::ZERO));
        let after = block(&scene, wall).face_shades.unwrap()[0];
        assert!(after.iter().all(|v| *v < 40));
        scene.refresh_meshes();
        assert!(
            scene
                .geometry()
                .0
                .iter()
                .any(|v| v.light >= 0.0 && v.light < 0.2)
        );
    }
    #[test]
    fn ambient_occlusion_shared_corners_agree_across_section_edges() {
        let mut scene = scene();
        let stone = scene
            .authority
            .registry()
            .default_state(&key("base:stone"))
            .unwrap();
        for p in [
            BlockPos::new(15, 7, 8),
            BlockPos::new(16, 7, 8),
            BlockPos::new(15, 8, 9),
        ] {
            scene.authority.store_mut().edit(p, stone).unwrap();
        }
        scene.enable_lighting().unwrap();
        resolve(&mut scene);
        let left = block(&scene, BlockPos::new(15, 7, 8)).face_shades.unwrap()[3];
        let right = block(&scene, BlockPos::new(16, 7, 8)).face_shades.unwrap()[3];
        assert_eq!(left[1], right[0]);
        assert_eq!(left[2], right[3]);
        assert!(left[1] < left[2]);
        assert!(left.iter().any(|value| *value != left[0]));
    }
    #[test]
    fn lighting_pressure_retries_stale_work_is_dropped_and_lifetimes_release() {
        let mut scene = scene();
        let pool = MemoryPool::new(torse_types::MemoryBudget::new(128 * 1024 * 1024));
        scene.attach_memory(pool.clone()).unwrap();
        scene.enable_lighting().unwrap();
        let held = pool
            .reserve(
                MemoryClass::Resource,
                pool.budget().total_bytes() - pool.usage().total - 1,
                EvictionRank::Pinned,
            )
            .unwrap();
        assert!(!scene.refresh_lighting(1).unwrap());
        assert!(scene.lighting.as_ref().unwrap().pending.is_none());
        drop(held);
        scene.refresh_lighting(1).unwrap();
        assert!(scene.lighting.as_ref().unwrap().pending.is_some());
        let stone = scene
            .authority
            .registry()
            .default_state(&key("base:stone"))
            .unwrap();
        scene
            .authority
            .store_mut()
            .edit(BlockPos::new(8, 8, 8), stone)
            .unwrap();
        scene.refresh_lighting(1).unwrap();
        assert!(scene.lighting.as_ref().unwrap().pending.is_none());
        resolve(&mut scene);
        scene.epoch = WorldEpoch(999);
        resolve(&mut scene);
        drop(scene);
        assert_eq!(pool.usage().total, 0);
    }

    #[test]
    fn publishing_distant_light_does_not_invalidate_an_unrelated_mesh() {
        let mut scene = scene();
        scene.enable_lighting().unwrap();
        resolve(&mut scene);
        let before = MeshStamp::capture(&scene, SectionPos::ZERO);
        for x in 7..=9 {
            for z in -1..=1 {
                assert!(
                    scene
                        .install_generated(SectionPos::new(x, 0, z), Section::empty(16).unwrap())
                        .unwrap()
                );
            }
        }
        scene.sections.push(SectionPos::new(8, 0, 0));
        resolve(&mut scene);
        assert_eq!(before, MeshStamp::capture(&scene, SectionPos::ZERO));
    }

    #[test]
    fn invalidation_keeps_presented_brightness_until_replacement_is_ready() {
        let mut scene = scene();
        let stone = scene
            .authority
            .registry()
            .default_state(&key("base:stone"))
            .unwrap();
        let wall = BlockPos::new(8, 8, 8);
        scene.authority.store_mut().edit(wall, stone).unwrap();
        scene.enable_lighting().unwrap();
        resolve(&mut scene);
        let before = block(&scene, wall).face_shades.unwrap();
        let generation = scene.lighting.as_ref().unwrap().stamp(SectionPos::ZERO);
        // Invalidate the observation without changing the wall's local AO.
        scene
            .authority
            .store_mut()
            .edit(BlockPos::new(1, 1, 1), stone)
            .unwrap();
        assert!(!scene.refresh_lighting(1).unwrap());
        assert!(!scene.lighting.as_ref().unwrap().entries[1][&SectionPos::ZERO].current);
        assert_eq!(block(&scene, wall).face_shades.unwrap(), before);
        assert_eq!(
            scene.lighting.as_ref().unwrap().stamp(SectionPos::ZERO),
            generation
        );
        resolve(&mut scene);
        assert_eq!(block(&scene, wall).face_shades.unwrap(), before);
    }

    #[test]
    fn identical_relit_pixels_do_not_invalidate_meshes() {
        let mut scene = scene();
        scene.enable_lighting().unwrap();
        resolve(&mut scene);
        let generation = scene.lighting.as_ref().unwrap().generation;
        let revision = scene.authority.store().revision(SectionPos::ZERO).unwrap();
        let stone = scene
            .authority
            .registry()
            .default_state(&key("base:stone"))
            .unwrap();
        scene
            .authority
            .store_mut()
            .edit(BlockPos::new(1, 1, 1), stone)
            .unwrap();
        scene
            .authority
            .store_mut()
            .edit(BlockPos::new(1, 1, 1), BlockStateId::EMPTY)
            .unwrap();
        assert_ne!(
            scene.authority.store().revision(SectionPos::ZERO).unwrap(),
            revision
        );
        resolve(&mut scene);
        assert_eq!(scene.lighting.as_ref().unwrap().generation, generation);
    }

    #[test]
    fn uncomputed_light_uses_directional_presentation_instead_of_black() {
        let mut scene = scene();
        let stone = scene
            .authority
            .registry()
            .default_state(&key("base:stone"))
            .unwrap();
        let wall = BlockPos::new(8, 8, 8);
        scene.authority.store_mut().edit(wall, stone).unwrap();
        scene.enable_lighting().unwrap();
        let view = scene.lighting.as_ref().unwrap().view(&scene);
        assert_eq!(view.light(1, BlockPos::new(8, -1, 8)), Some(0));
        assert_eq!(view.light(1, BlockPos::new(8, 16, 8)), Some(15));
        let top =
            block(&scene, wall).face_shades.unwrap()[torse_render::Face::PosY.index() as usize];
        assert!(top.iter().all(|v| *v == 255));
    }

    #[test]
    fn inset_slab_faces_sample_the_air_gap_in_their_own_cell() {
        for (half, face, neighbor_y) in [
            (0, torse_render::Face::PosY, 9),
            (1, torse_render::Face::NegY, 7),
        ] {
            let mut scene = scene();
            let stone = scene
                .authority
                .registry()
                .default_state(&key("base:stone"))
                .unwrap();
            let slab = scene
                .authority
                .registry()
                .state_of(&key("base:glass_slab"), &[half])
                .unwrap();
            // Cover both sides of the section boundary with the same case.
            for x in [15, 16] {
                scene
                    .authority
                    .store_mut()
                    .edit(BlockPos::new(x, 8, 8), slab)
                    .unwrap();
                scene
                    .authority
                    .store_mut()
                    .edit(BlockPos::new(x, neighbor_y, 8), stone)
                    .unwrap();
            }
            scene.enable_lighting().unwrap();
            resolve(&mut scene);
            for x in [15, 16] {
                let shades = block(&scene, BlockPos::new(x, 8, 8)).face_shades.unwrap();
                assert!(
                    shades[face.index() as usize].iter().all(|v| *v > 80),
                    "half={half}: {shades:?}"
                );
            }
        }
    }

    #[test]
    fn breaking_repeatedly_exposes_lit_faces_before_exact_relighting() {
        for start_x in [8, 14] {
            let mut scene = scene();
            let stone = scene
                .authority
                .registry()
                .default_state(&key("base:stone"))
                .unwrap();
            for x in start_x..=start_x + 3 {
                scene
                    .authority
                    .store_mut()
                    .edit(BlockPos::new(x, 8, 8), stone)
                    .unwrap();
            }
            scene.enable_lighting().unwrap();
            resolve(&mut scene);
            // Do not run another lighting job between these edits. The old
            // resolved grids contain zero in every originally solid cell.
            for x in start_x..start_x + 3 {
                let pos = BlockPos::new(x, 8, 8);
                scene.apply_edit(false, &key("base:stone"), pos, 0).unwrap();
                let shades = block(&scene, BlockPos::new(x + 1, 8, 8))
                    .face_shades
                    .unwrap();
                assert!(shades[0].iter().all(|v| *v > 80), "x={x}: {shades:?}");
            }
            resolve(&mut scene);
            let shades = block(&scene, BlockPos::new(start_x + 3, 8, 8))
                .face_shades
                .unwrap();
            assert!(shades[0].iter().all(|v| *v > 80));
        }
    }

    #[test]
    fn breaking_in_a_dark_sealed_space_does_not_invent_light() {
        let mut scene = scene();
        let stone = scene
            .authority
            .registry()
            .default_state(&key("base:stone"))
            .unwrap();
        for x in -16..48 {
            for z in -16..32 {
                scene
                    .authority
                    .store_mut()
                    .edit(BlockPos::new(x, 12, z), stone)
                    .unwrap();
            }
        }
        for x in [8, 9] {
            scene
                .authority
                .store_mut()
                .edit(BlockPos::new(x, 8, 8), stone)
                .unwrap();
        }
        scene.enable_lighting().unwrap();
        resolve(&mut scene);
        scene
            .apply_edit(false, &key("base:stone"), BlockPos::new(8, 8, 8), 0)
            .unwrap();
        assert!(
            block(&scene, BlockPos::new(9, 8, 8)).face_shades.unwrap()[0]
                .iter()
                .all(|v| *v < 40)
        );
    }

    #[test]
    fn sealing_the_only_opening_darkens_immediately_and_reopening_restores_light() {
        for start_x in [6, 14] {
            let mut scene = scene();
            let stone = scene
                .authority
                .registry()
                .default_state(&key("base:stone"))
                .unwrap();
            for x in start_x..=start_x + 4 {
                for y in 5..=10 {
                    for z in 5..=9 {
                        if x == start_x || x == start_x + 4 || y == 5 || y == 10 || z == 5 || z == 9
                        {
                            scene
                                .authority
                                .store_mut()
                                .edit(BlockPos::new(x, y, z), stone)
                                .unwrap();
                        }
                    }
                }
            }
            let hole = BlockPos::new(start_x + 2, 10, 7);
            let inside = BlockPos::new(start_x + 2, 8, 7);
            scene
                .authority
                .store_mut()
                .edit(hole, BlockStateId::EMPTY)
                .unwrap();
            scene.enable_lighting().unwrap();
            resolve(&mut scene);
            assert!(
                scene
                    .lighting
                    .as_ref()
                    .unwrap()
                    .view(&scene)
                    .light(1, inside)
                    .unwrap()
                    > 0
            );
            scene.apply_edit(true, &key("base:stone"), hole, 0).unwrap();
            assert_eq!(
                scene
                    .lighting
                    .as_ref()
                    .unwrap()
                    .view(&scene)
                    .light(1, inside),
                Some(0)
            );
            resolve(&mut scene);
            assert_eq!(
                scene
                    .lighting
                    .as_ref()
                    .unwrap()
                    .view(&scene)
                    .light(1, inside),
                Some(0)
            );
            scene
                .apply_edit(false, &key("base:stone"), hole, 0)
                .unwrap();
            assert!(
                scene
                    .lighting
                    .as_ref()
                    .unwrap()
                    .view(&scene)
                    .light(1, inside)
                    .unwrap()
                    > 0
            );
            let second = BlockPos::new(start_x + 1, 10, 7);
            scene
                .apply_edit(false, &key("base:stone"), second, 0)
                .unwrap();
            scene.apply_edit(true, &key("base:stone"), hole, 0).unwrap();
            assert!(
                scene
                    .lighting
                    .as_ref()
                    .unwrap()
                    .view(&scene)
                    .light(1, inside)
                    .unwrap()
                    > 0
            );
            scene
                .apply_edit(true, &key("base:stone"), second, 0)
                .unwrap();
            assert_eq!(
                scene
                    .lighting
                    .as_ref()
                    .unwrap()
                    .view(&scene)
                    .light(1, inside),
                Some(0)
            );
        }
    }

    #[test]
    fn reloaded_emitter_invalidates_cached_emission() {
        let mut scene = scene();
        let external = SectionPos::new(2, 0, 0);
        let sample = BlockPos::new(31, 8, 8);
        let glass = scene
            .authority
            .registry()
            .default_state(&key("base:glass"))
            .unwrap();
        let stone = scene
            .authority
            .registry()
            .default_state(&key("base:stone"))
            .unwrap();
        scene
            .authority
            .store_mut()
            .edit(BlockPos::new(47, 0, 15), stone)
            .unwrap();
        scene.enable_lighting().unwrap();
        resolve(&mut scene);
        assert_eq!(
            scene
                .lighting
                .as_ref()
                .unwrap()
                .view(&scene)
                .light(0, sample),
            Some(0)
        );
        let revision = scene.authority.store().revision(external);
        scene.authority.store_mut().unload(external).unwrap();
        scene.load_generations.remove(&external);
        scene
            .install_generated(external, Section::new(16, glass).unwrap())
            .unwrap();
        assert_ne!(scene.authority.store().revision(external), revision);
        resolve(&mut scene);
        assert!(
            scene
                .lighting
                .as_ref()
                .unwrap()
                .view(&scene)
                .light(0, sample)
                .unwrap()
                > 0
        );
    }

    #[test]
    fn opening_the_world_ceiling_uses_exterior_skylight() {
        let mut scene = scene();
        let stone = scene
            .authority
            .registry()
            .default_state(&key("base:stone"))
            .unwrap();
        let pos = BlockPos::new(8, 15, 8);
        scene.authority.store_mut().edit(pos, stone).unwrap();
        scene.enable_lighting().unwrap();
        resolve(&mut scene);
        scene.apply_edit(false, &key("base:stone"), pos, 0).unwrap();
        assert_eq!(
            scene.lighting.as_ref().unwrap().view(&scene).light(1, pos),
            Some(15)
        );
    }

    #[test]
    fn edit_preview_budget_rejection_preserves_terrain_and_lighting() {
        let mut scene = scene();
        let pool = MemoryPool::new(torse_types::MemoryBudget::new(128 * 1024 * 1024));
        scene.attach_memory(pool.clone()).unwrap();
        let stone = scene
            .authority
            .registry()
            .default_state(&key("base:stone"))
            .unwrap();
        let pos = BlockPos::new(8, 8, 8);
        scene.authority.store_mut().edit(pos, stone).unwrap();
        scene.enable_lighting().unwrap();
        resolve(&mut scene);
        let generation = scene.lighting.as_ref().unwrap().generation;
        let held = pool
            .reserve(
                MemoryClass::Resource,
                pool.budget().total_bytes() - pool.usage().total - 1,
                EvictionRank::Pinned,
            )
            .unwrap();
        assert!(scene.apply_edit(false, &key("base:stone"), pos, 0).is_err());
        assert_eq!(
            scene.authority.store().query(pos),
            TerrainQuery::Ready(stone)
        );
        assert_eq!(scene.lighting.as_ref().unwrap().generation, generation);
        drop(held);
        scene.apply_edit(false, &key("base:stone"), pos, 0).unwrap();
        assert_eq!(
            scene.authority.store().query(pos),
            TerrainQuery::Ready(BlockStateId::EMPTY)
        );
        resolve(&mut scene);
        drop(scene);
        assert_eq!(pool.usage().total, 0);
    }
}
