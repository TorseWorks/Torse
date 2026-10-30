//! Terrain scene for the client: load, generate, mesh, and expose vertices.
//!
//! The scene owns an authoritative world through [`WorldAuthority`]. Sections in
//! a square radius are generated deterministically and meshed once per terrain
//! revision. A mesh is rebuilt only when its section or one of its neighbours
//! changed, which is the rendering half of the revision contract the mesher
//! already enforces.

use std::collections::BTreeMap;
use std::sync::Arc;

use torse_content::{Face, Shape, compile, m0_fixture_pack};
use torse_render::{MeshData, MeshSample, RenderBlock, RenderBox, build_mesh_optimized};
use torse_sim::WorldAuthority;
use torse_sim::action::{ActionOrigin, ActionRequest, ActionService, ActionTarget, ActorContext};
use torse_sim::replica::{ReplicaChanges, ReplicaEntity, ReplicaSnapshot};
use torse_sim::{OrderingKey, Phase, block_actions};
use torse_types::{
    BlockPos, Compatibility, ContentKey, LocalPos, SectionPos, TransactionId, WorldEpoch, WorldId,
};
use torse_world::{
    BlockStateId, BlockStateRegistry, ComponentFields, ComponentSchemas, ComponentSet,
    ComponentValue, Position, Section, SnapshotCells, SnapshotSection, StateEntry, TerrainQuery,
    WorldBounds,
};
use torse_world_services::{GenerationParams, HillTerrain, SectionGenerator};

use crate::pick::{PickHit, PickOutcome, cast_ray};
use crate::texture::{Palette, TexturePack};

mod background;
pub use background::BackgroundStats;
use background::{Background, MeshStamp};

pub fn background_config() -> torse_runtime::ExecutorConfig {
    background::default_config()
}

fn scene_epoch() -> Result<WorldEpoch, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
        .map(|n| WorldEpoch(u128::from(n)))
        .map_err(|_| "scene epoch exhausted".to_owned())
}

/// One vertex uploaded to the GPU.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vertex {
    /// World-space position.
    pub position: [f32; 3],
    /// Face normal.
    pub normal: [f32; 3],
    /// Local texture coordinates within the face.
    pub uv: [f32; 2],
    /// Material slot looked up in the client material table.
    pub material: u32,
    /// Optional second material slot, or `u32::MAX` when absent.
    pub overlay_material: u32,
    /// Position tint for the base material.
    pub tint: [f32; 3],
    /// Position tint for the overlay material.
    pub overlay_tint: [f32; 3],
    /// Negative selects directional shading; otherwise resolved brightness.
    pub light: f32,
}

impl Vertex {
    /// Bytes per interleaved vertex.
    pub const STRIDE: usize = 68;

    fn write(&self, out: &mut Vec<u8>) {
        for value in self
            .position
            .iter()
            .chain(self.normal.iter())
            .chain(self.uv.iter())
        {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&self.material.to_le_bytes());
        out.extend_from_slice(&self.overlay_material.to_le_bytes());
        for value in self.tint.iter().chain(self.overlay_tint.iter()) {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&self.light.to_le_bytes());
    }
}

#[derive(Clone, Copy)]
struct StateMaterials {
    base: [u16; 6],
    overlays: [Option<u16>; 6],
}

/// Terrain scene bound to one authoritative world.
pub struct Scene {
    authority: WorldAuthority,
    service: ActionService,
    principal: ContentKey,
    edit_counter: u64,
    walker: Option<torse_sim::player::WalkerState>,
    items: Option<items::Stock>,
    generator: Option<HillTerrain>,
    /// Data-driven terrain used when the scene comes from a content pack.
    layered: Option<Arc<crate::game::LayeredTerrain>>,
    side: u32,
    radius: i64,
    /// Cached mesh plus the dependency revisions it was built against;
    /// a neighbour edit is detected by validating this stored job.
    meshes: BTreeMap<SectionPos, (MeshStamp, MeshData)>,
    sections: Vec<SectionPos>,
    stream_interest: Option<(SectionPos, i64, i64)>,
    epoch: WorldEpoch,
    asset_generation: u64,
    next_load_generation: u64,
    load_generations: BTreeMap<SectionPos, u64>,
    background: Option<Background>,
    palette: Palette,
    /// Material slot per block state and face.
    state_materials: BTreeMap<BlockStateId, StateMaterials>,
    /// Blocks a pack scene may place and break; `None` keeps the fixture keys.
    placeable: Option<Vec<ContentKey>>,
    /// Vertical section layers the stream keeps resident.
    layers: i64,
    tint_provider: Option<Arc<dyn torse_assets::texture::PositionTintProvider>>,
    tint_provider_generation: u64,
    memory: Option<memory::Memory>,
    startup_notice: Option<String>,
    lighting: Option<lighting::Lighting>,
    surfaces: surfaces::Previews,
}
mod borders;
mod items;
mod lighting;
mod memory;
mod models;
mod surfaces;

/// One content pack for every client mode, matching the server demo assembly
/// so the handshake compatibility and the block colours agree everywhere.
fn content_pack() -> torse_content::ContentPack {
    let mut pack = m0_fixture_pack();
    block_actions::register_content(&mut pack);
    torse_inventory::register_content(&mut pack);
    torse_items::register_content(&mut pack);
    pack
}

/// Build the registry, schemas, and handshake compatibility from the shared pack.
fn compile_content() -> Result<(BlockStateRegistry, ComponentSchemas, Compatibility), String> {
    let content =
        compile(content_pack()).map_err(|errors| format!("content failed: {errors:?}"))?;
    let registry =
        BlockStateRegistry::build(&content).map_err(|error| format!("registry failed: {error}"))?;
    let schemas = ComponentSchemas::build(&content);
    let catalog = torse_items::fixture_catalog(&registry)
        .map_err(|error| format!("item rules: {error:?}"))?
        .with_world_drops();
    let compatibility = catalog.bind_compatibility(Compatibility {
        authority_schema: 4,
        rules_digest: content.authority_digest,
        content_digest: content.presentation_digest,
    });
    Ok((registry, schemas, compatibility))
}

/// Build the palette and the per-state material slots.
///
/// Every block state resolves six material slots once at startup; meshing then
/// copies the slots into each render block without touching content keys.
fn material_tables(
    registry: &BlockStateRegistry,
    pack: &TexturePack,
) -> Result<(Palette, BTreeMap<BlockStateId, StateMaterials>), String> {
    let palette = Palette::build(registry, pack)?;
    let mut states = BTreeMap::new();
    for index in 0..registry.state_count() {
        let state = BlockStateId(index as u16);
        let Some(definition) = registry.definition(state) else {
            continue;
        };
        let profile = &definition.render_profile;
        let mut materials = StateMaterials {
            base: [0; 6],
            overlays: [None; 6],
        };
        for face in Face::ALL {
            let material = profile.face_materials.resolve(face, &profile.material);
            materials.base[face.index()] = palette
                .slot_for(material, profile.alpha_mode)
                .ok_or_else(|| format!("material `{material}` is missing from the palette"))?;
            materials.overlays[face.index()] = profile
                .face_overlays
                .get(face)
                .map(|overlay| {
                    palette
                        .slot_for(&overlay.material, overlay.alpha_mode)
                        .ok_or_else(|| {
                            format!(
                                "overlay material `{}` is missing from the palette",
                                overlay.material
                            )
                        })
                })
                .transpose()?;
        }
        states.insert(state, materials);
    }
    Ok((palette, states))
}

impl Scene {
    /// Hide the camera's bound body without granting writes to the mirror.
    pub fn view_player(&mut self, id: torse_types::EntityId) {
        self.walker = self
            .authority
            .entities()
            .handle_of(id)
            .map(torse_sim::player::WalkerState::new);
    }
    /// Build a scene with a seeded hill terrain and the base game texture pack.
    pub fn new(radius: i64) -> Result<Self, String> {
        Self::with_pack(radius, crate::texture::base_pack()?)
    }

    /// Build a scene from one authorized replica baseline.
    ///
    /// The network client has no generator: sections and entities come from
    /// the server projection only, and `load_around` keeps the subscribed
    /// section list as-is.
    pub fn from_snapshot(snapshot: &ReplicaSnapshot) -> Result<Self, String> {
        Self::from_snapshot_with_pack(snapshot, crate::texture::base_pack()?)
    }

    /// Build a scene with an explicit texture pack.
    pub fn with_pack(radius: i64, pack: TexturePack) -> Result<Self, String> {
        if !(0..=16).contains(&radius) {
            return Err("scene radius must be 0..=16".to_owned());
        }
        let (registry, schemas, compatibility) = compile_content()?;
        let (palette, state_materials) = material_tables(&registry, &pack)?;
        let stone = registry
            .default_state(&key("base:stone"))
            .ok_or_else(|| "missing base:stone".to_owned())?;
        let side = 16;
        let bounds = WorldBounds::new(0, 64).map_err(|error| error.to_string())?;
        let service = ActionService::bind(
            block_actions::descriptors(),
            block_actions::handlers(),
            &schemas,
        )
        .map_err(|error| format!("actions failed: {error}"))?;
        let authority =
            WorldAuthority::new(WorldId(1), registry, schemas, side, bounds, compatibility)
                .map_err(|error| error.to_string())?;
        let params = GenerationParams::new(side, bounds, stone, BlockStateId::EMPTY)
            .map_err(|error| error.to_string())?;
        let generator = HillTerrain::new(params, 7, 7, 9);
        let mut scene = Self {
            authority,
            service,
            principal: key("client:player"),
            edit_counter: 0,
            walker: None,
            items: None,
            generator: Some(generator),
            layered: None,
            side,
            radius,
            meshes: BTreeMap::new(),
            lighting: None,
            surfaces: surfaces::Previews::default(),
            sections: Vec::new(),
            stream_interest: None,
            epoch: scene_epoch()?,
            asset_generation: palette.asset_generation(),
            next_load_generation: 1,
            load_generations: BTreeMap::new(),
            background: None,
            palette,
            state_materials,
            placeable: None,
            layers: 1,
            tint_provider: None,
            tint_provider_generation: 0,
            memory: None,
            startup_notice: None,
        };
        scene.load_around(SectionPos::ZERO)?;
        Ok(scene)
    }

    /// Build a network scene with an explicit texture pack.
    pub fn from_snapshot_with_pack(
        snapshot: &ReplicaSnapshot,
        pack: TexturePack,
    ) -> Result<Self, String> {
        let (registry, schemas, compatibility) = compile_content()?;
        let (palette, state_materials) = material_tables(&registry, &pack)?;
        let service = ActionService::bind(
            block_actions::descriptors(),
            block_actions::handlers(),
            &schemas,
        )
        .map_err(|error| format!("actions failed: {error}"))?;
        let authority = WorldAuthority::new(
            WorldId(1),
            registry,
            schemas,
            snapshot.side,
            snapshot.bounds,
            compatibility,
        )
        .map_err(|error| error.to_string())?;
        let mut scene = Self {
            authority,
            service,
            principal: key("client:player"),
            edit_counter: 0,
            walker: None,
            items: None,
            generator: None,
            layered: None,
            side: snapshot.side,
            radius: 0,
            meshes: BTreeMap::new(),
            lighting: None,
            surfaces: surfaces::Previews::default(),
            sections: Vec::new(),
            stream_interest: None,
            epoch: scene_epoch()?,
            asset_generation: palette.asset_generation(),
            next_load_generation: 1,
            load_generations: BTreeMap::new(),
            background: None,
            palette,
            state_materials,
            placeable: None,
            layers: 1,
            tint_provider: None,
            tint_provider_generation: 0,
            memory: None,
            startup_notice: None,
        };
        scene.install_snapshot(snapshot)?;
        Ok(scene)
    }

    /// Material palette used by the terrain shader.
    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    /// Preview limitations shown when the window opens this scene.
    pub fn startup_notice(&self) -> Option<&str> {
        self.startup_notice.as_deref()
    }

    /// Replace the location-dependent tint data used while building meshes.
    #[allow(dead_code)]
    pub fn set_position_tint_provider(
        &mut self,
        provider: Option<Arc<dyn torse_assets::texture::PositionTintProvider>>,
    ) -> Result<(), String> {
        let generation = self
            .tint_provider_generation
            .checked_add(1)
            .ok_or("position tint provider generation exhausted")?;
        self.tint_provider = provider;
        self.tint_provider_generation = generation;
        self.meshes.clear();
        Ok(())
    }

    /// Whether this scene mirrors a server projection instead of generating.
    pub const fn is_network(&self) -> bool {
        self.generator.is_none() && self.layered.is_none()
    }

    /// Blocks a pack scene may place and break, when the scene came from a pack.
    pub fn placeable_blocks(&self) -> Option<&[ContentKey]> {
        self.placeable.as_deref()
    }

    /// Handshake compatibility of the shared content pack.
    pub fn compatibility() -> Result<Compatibility, String> {
        Ok(compile_content()?.2)
    }

    /// Translate one replica baseline into local sections and entities.
    fn install_snapshot(&mut self, snapshot: &ReplicaSnapshot) -> Result<(), String> {
        let symbols = self.resolve_states(&snapshot.states)?;
        for section in &snapshot.sections {
            self.install_replica_section(section, &symbols)?;
        }
        self.sections = snapshot.sections.iter().map(|s| s.pos).collect();
        for entity in &snapshot.entities {
            self.install_replica_entity(entity)?;
        }
        Ok(())
    }

    /// Map the snapshot's runtime state table into local registry IDs.
    fn resolve_states(
        &self,
        states: &[StateEntry],
    ) -> Result<BTreeMap<BlockStateId, BlockStateId>, String> {
        let mut symbols = BTreeMap::new();
        symbols.insert(BlockStateId::EMPTY, BlockStateId::EMPTY);
        for entry in states {
            let local = registry_state(self.authority.registry(), &entry.block, &entry.values)?;
            symbols.insert(entry.id, local);
        }
        Ok(symbols)
    }

    /// Install or replace one section from replica cells.
    fn install_replica_section(
        &mut self,
        section: &SnapshotSection,
        symbols: &BTreeMap<BlockStateId, BlockStateId>,
    ) -> Result<(), String> {
        let side = self.side;
        let mut built = Section::new(side, BlockStateId::EMPTY)
            .map_err(|error| format!("section failed: {error}"))?;
        let write = |built: &mut Section, index: usize, state: BlockStateId| {
            let side = side as usize;
            let x = (index % side) as u32;
            let z = ((index / side) % side) as u32;
            let y = (index / (side * side)) as u32;
            built
                .set(LocalPos { x, y, z }, state)
                .map_err(|error| format!("cell failed: {error}"))
        };
        match &section.cells {
            SnapshotCells::Uniform(state) => {
                let local = *symbols.get(state).ok_or("unknown replica state")?;
                if !local.is_empty() {
                    // `Section::new` fills every cell; only real blocks need
                    // per-cell writes.
                    for index in 0..(side as usize).pow(3) {
                        write(&mut built, index, local)?;
                    }
                }
            }
            SnapshotCells::Dense(cells) => {
                if cells.len() != (side as usize).pow(3) {
                    return Err("dense section has the wrong length".to_owned());
                }
                for (index, state) in cells.iter().enumerate() {
                    let local = *symbols.get(state).ok_or("unknown replica state")?;
                    if !local.is_empty() {
                        write(&mut built, index, local)?;
                    }
                }
            }
        }
        let store = self.authority.store_mut();
        let _ = store.unload(section.pos);
        store
            .request(section.pos)
            .map_err(|error| error.to_string())?;
        store
            .install(section.pos, built)
            .map_err(|error| error.to_string())?;
        store
            .finish_load(section.pos)
            .map_err(|error| error.to_string())?;
        // A reinstalled section can reuse the previous revision number, so the
        // cached mesh must go or the change would render as a no-op.
        self.forget_mesh(&section.pos);
        self.assign_load_generation(section.pos)?;
        Ok(())
    }

    /// Insert or replace one entity from a replica record, keeping its
    /// persistent identity.
    fn install_replica_entity(&mut self, entity: &ReplicaEntity) -> Result<(), String> {
        if let Some(handle) = self.authority.entities().handle_of(entity.id) {
            let _ = self.authority.entities_mut().despawn(handle);
        }
        let position = Position::from_bits(entity.position_bits);
        let mut components = ComponentSet::new();
        let schemas = self.authority.schemas().clone();
        for (schema, fields) in &entity.components {
            components
                .insert(&schemas, schema, fields.clone())
                .map_err(|error| format!("component failed: {error}"))?;
        }
        self.authority
            .entities_mut()
            .insert_restored(entity.id, position, components)
            .map_err(|error| format!("entity failed: {error}"))?;
        Ok(())
    }

    /// Apply one replica increment: replaced sections and entities, removals.
    #[cfg(test)]
    pub fn apply_changes(&mut self, changes: &ReplicaChanges) -> Result<(), String> {
        if !self.try_apply_changes(changes)? {
            return Err("world memory admission deferred".to_owned());
        }
        Ok(())
    }
    fn apply_changes_unbudgeted(&mut self, changes: &ReplicaChanges) -> Result<(), String> {
        let symbols = self.resolve_states(&changes.states)?;
        for pos in &changes.removed_sections {
            let _ = self.authority.store_mut().unload(*pos);
            self.forget_mesh(pos);
            self.load_generations.remove(pos);
            self.sections.retain(|item| item != pos);
        }
        for section in &changes.sections {
            self.install_replica_section(section, &symbols)?;
            if !self.sections.contains(&section.pos) {
                self.sections.push(section.pos);
            }
        }
        for id in &changes.removed_entities {
            if let Some(handle) = self.authority.entities().handle_of(*id) {
                let _ = self.authority.entities_mut().despawn(handle);
            }
        }
        for entity in &changes.entities {
            self.install_replica_entity(entity)?;
        }
        Ok(())
    }

    /// Authoritative world.
    pub const fn authority(&self) -> &WorldAuthority {
        &self.authority
    }

    /// Shape of one cell for picking: empty, or the block's render bounds.
    /// `None` stops at unknown cells. Outside the vertical world bounds is
    /// known empty space for picking, while edits still enforce those bounds.
    fn shape_at(&self, pos: BlockPos) -> Option<Option<RenderBox>> {
        match self.authority.store().query(pos) {
            TerrainQuery::Ready(state) => {
                if state.is_empty() {
                    return Some(None);
                }
                Some(
                    self.authority
                        .registry()
                        .selection_shape_ref(state)
                        .and_then(shape_box),
                )
            }
            TerrainQuery::NotLoaded => None,
            TerrainQuery::OutOfBounds | TerrainQuery::Exterior => Some(None),
        }
    }

    /// Build the selection outline edges for one hit, following the block's
    /// render bounds: a slab outlines its lower half, a plant its panel.
    pub fn outline_bytes(&self, hit: &PickHit) -> Vec<u8> {
        let state = match self.authority.store().query(hit.block) {
            TerrainQuery::Ready(state) if !state.is_empty() => state,
            _ => return Vec::new(),
        };
        // The selection shape follows state values: an upper slab outlines
        // its top half, a lower one its bottom half.
        let Some(selection) = self.authority.registry().selection_shape(state) else {
            return Vec::new();
        };
        let Some(bounds) = shape_box(&selection) else {
            return Vec::new();
        };
        const INFLATE: f32 = 0.004;
        let min = [
            hit.block.x as f32 + bounds.min[0] - INFLATE,
            hit.block.y as f32 + bounds.min[1] - INFLATE,
            hit.block.z as f32 + bounds.min[2] - INFLATE,
        ];
        let max = [
            hit.block.x as f32 + bounds.max[0] + INFLATE,
            hit.block.y as f32 + bounds.max[1] + INFLATE,
            hit.block.z as f32 + bounds.max[2] + INFLATE,
        ];
        let corner = |bits: u32| {
            [
                if bits & 1 != 0 { max[0] } else { min[0] },
                if bits & 2 != 0 { max[1] } else { min[1] },
                if bits & 4 != 0 { max[2] } else { min[2] },
            ]
        };
        const EDGES: [(u32, u32); 12] = [
            (0, 1),
            (2, 3),
            (4, 5),
            (6, 7),
            (0, 2),
            (1, 3),
            (4, 6),
            (5, 7),
            (0, 4),
            (1, 5),
            (2, 6),
            (3, 7),
        ];
        let mut vertices = Vec::with_capacity(EDGES.len() * 2);
        for (from, to) in EDGES {
            for position in [corner(from), corner(to)] {
                vertices.push(Vertex {
                    position,
                    normal: [0.0, 1.0, 0.0],
                    uv: [0.0, 0.0],
                    material: 0,
                    overlay_material: u32::MAX,
                    tint: [0.05; 3],
                    overlay_tint: [1.0; 3],
                    light: -1.0,
                });
            }
        }
        Scene::vertex_bytes(&vertices)
    }

    /// Cast a ray through loaded terrain and report the first solid cell.
    pub fn pick(&self, origin: [f32; 3], direction: [f32; 3], max_distance: f32) -> PickOutcome {
        cast_ray(
            &mut |pos| self.shape_at(pos),
            origin,
            direction,
            max_distance,
        )
    }

    /// Resolve the same content-driven placement for local and network edits.
    pub fn placement_at(
        &self,
        hit: &PickHit,
        held: &ContentKey,
        direction: [f32; 3],
    ) -> Option<block_actions::placement::PlacementIntent> {
        let current = self.authority.store().query(hit.block).ready()?;
        block_actions::placement::resolve_view(
            self.authority.registry(),
            held,
            current,
            block_actions::placement::PlacementHit {
                block: hit.block,
                face: hit.face,
                local_height: hit.entry[1] - hit.block.y as f32,
            },
            [
                hit.entry[0] - hit.block.x as f32,
                hit.entry[1] - hit.block.y as f32,
                hit.entry[2] - hit.block.z as f32,
            ]
            .map(|v| v.clamp(0.0, 1.0)),
            direction,
        )
    }

    /// Spawn the walk-mode player entity at a world position.
    ///
    /// The windowed client is a trusted local host, so it installs the player
    /// directly through the entity store the same way the server bootstrap
    /// installs its fixtures; movement still goes through transactions.
    /// Server projections reject local walking until server movement exists.
    pub fn spawn_walker(&mut self, at: Position) -> Result<Position, String> {
        if self.is_network() {
            return Err("network mirrors cannot settle local movement".to_owned());
        }
        if self.walker.is_some() {
            return Err("walk mode already has an entity".to_owned());
        }
        // Correct a near-floor fly viewpoint without creating a buried body.
        // Unknown terrain is never searched through or treated as empty.
        let mut safe = None;
        for step in 0..=8 {
            let candidate = at.translated([0.0, f64::from(step) * 0.25, 0.0]);
            match torse_sim::movement::observe_body(
                self.authority.store(),
                self.authority.registry(),
                candidate,
                torse_sim::player::WALKER_HALF,
            ) {
                Ok(_) => {
                    safe = Some(candidate);
                    break;
                }
                Err(torse_types::RejectCode::Conflict) => continue,
                Err(error) => return Err(format!("walk entry rejected: {error:?}")),
            }
        }
        let at = safe.ok_or("walk entry has no clear body position within two blocks")?;
        let growth = self
            .reserve_memory(
                torse_types::MemoryClass::World,
                self.authority
                    .entities()
                    .accounted_bytes()
                    .saturating_add(16384),
                torse_types::EvictionRank::Pinned,
            )
            .map_err(|error| error.to_string())?;
        let handle = self
            .authority
            .entities_mut()
            .spawn(at)
            .map_err(|error| format!("spawn failed: {error}"))?;
        self.walker = Some(torse_sim::player::WalkerState::new(handle));
        self.reconcile_world_memory(growth)
            .map_err(|error| error.to_string())?;
        Ok(at)
    }

    /// Release a camera binding; local walk bodies belong to this mode only.
    pub fn stop_walker(&mut self) -> Result<(), String> {
        if let Some(walker) = self.walker.take()
            && !self.is_network()
        {
            self.authority.entities_mut().despawn(walker.handle);
            self.reconcile_world_memory(None)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Walker position, if walk mode has a live entity.
    pub fn walker_position(&self) -> Option<Position> {
        let walker = self.walker?;
        self.authority
            .entities()
            .get(walker.handle)
            .map(|entity| entity.position)
    }

    /// Integrate one walk tick through the shared player domain; see the
    /// movement contract. Rejections drop the tick and keep the entity at
    /// its committed position.
    pub fn step_walker(
        &mut self,
        wish: [f64; 3],
        jump: bool,
        dt: f32,
    ) -> Result<(Position, bool), String> {
        if self.is_network() {
            return Err("network mirrors cannot settle local movement".to_owned());
        }
        let Some(mut walker) = self.walker else {
            return Err("walk mode has no entity".to_owned());
        };
        let growth = match self.reserve_memory(
            torse_types::MemoryClass::World,
            self.authority
                .entities()
                .accounted_bytes()
                .saturating_add(16384),
            torse_types::EvictionRank::Pinned,
        ) {
            Ok(lease) => lease,
            Err(error) if error.code == torse_types::FaultCode::ResourceLimit => {
                return self
                    .walker_position()
                    .map(|position| (position, walker.grounded))
                    .ok_or_else(|| "walker entity is gone".to_owned());
            }
            Err(error) => return Err(error.to_string()),
        };
        self.edit_counter += 1;
        let counter = self.edit_counter;
        let outcome = torse_sim::player::walker_step(
            &mut self.authority,
            &mut walker,
            wish,
            jump,
            dt,
            &self.principal,
            TransactionId(u128::from(counter)),
        );
        self.walker = Some(walker);
        self.reconcile_world_memory(growth)
            .map_err(|error| error.to_string())?;
        match outcome {
            Ok(result) => Ok(result),
            Err(torse_sim::player::StepError::EntityGone) => {
                Err("walker entity is gone".to_owned())
            }
            Err(torse_sim::player::StepError::Rejected(code)) => {
                Err(format!("step rejected: {code:?}"))
            }
            Err(torse_sim::player::StepError::Fault(fault)) => {
                Err(format!("commit fault: {fault:?}"))
            }
        }
    }

    /// Plan and commit one place or break action through the action service.
    ///
    /// The windowed client is a trusted local host: it grants the player the
    /// edit permission and the single target cell for this request, then lets
    /// the service authorize, validate, and commit like any other adapter.
    /// The error string names the rejection, it is diagnostics only.
    pub fn apply_edit(
        &mut self,
        place: bool,
        block: &ContentKey,
        target: BlockPos,
        half: u32,
    ) -> Result<(), String> {
        let _preview_memory = {
            self.reserve_memory(
                torse_types::MemoryClass::Executor,
                lighting::PREVIEW_BYTES,
                torse_types::EvictionRank::Pinned,
            )
            .map_err(|error| error.to_string())?
        };
        let surfaces = self.prepare_surfaces(target)?;
        self.edit_counter += 1;
        let counter = self.edit_counter;
        let action = if place {
            key("world:action/place")
        } else {
            key("world:action/break")
        };
        let mut parameters = ComponentFields::new();
        if place {
            parameters.insert(
                key("world:field/block"),
                ComponentValue::Content(block.clone()),
            );
            // Half-type blocks take their half from the requested half.
            parameters.insert(key("world:field/half"), ComponentValue::U32(half));
        }
        let request = ActionRequest {
            action,
            api_version: 1,
            origin: ActionOrigin::Ui,
            target: ActionTarget::Block(target),
            parameters,
            transaction: TransactionId(u128::from(counter)),
            ordering: OrderingKey {
                phase: Phase::Interaction,
                rule_priority: 0,
                stable_task_key: counter,
                local_sequence: 0,
            },
        };
        let actor = ActorContext::new(self.principal.clone())
            .permission(key("world:permission/edit"))
            .target(ActionTarget::Block(target));
        let output = self
            .service
            .plan(&self.authority, &actor, Phase::Interaction, &request)
            .map_err(|code| format!("{code:?}"))?;
        let torse_sim::action::ActionOutput::Mutation(transaction) = output else {
            return Err("place and break must be mutations".to_owned());
        };
        let prepared = self
            .authority
            .prepare(&transaction)
            .map_err(|error| format!("{error:?}"))?;
        let growth = self
            .reserve_memory(
                torse_types::MemoryClass::World,
                self.authority.growth_reservation(&prepared),
                torse_types::EvictionRank::Pinned,
            )
            .map_err(|error| error.to_string())?;
        self.authority
            .publish(prepared)
            .map_err(|fault| format!("{fault:?}"))?;
        self.surfaces.record(surfaces);
        self.preview_lighting(&[target])?;
        self.reconcile_world_memory(growth)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Loaded section coordinates.
    pub fn sections(&self) -> &[SectionPos] {
        &self.sections
    }
    /// Change local streaming interest; mirrors retain their authorized scope.
    pub fn set_view_radius(&mut self, radius: i64) {
        if !self.is_network() {
            self.radius = radius.clamp(0, 16);
        }
    }

    /// Number of cached meshes.
    pub fn mesh_count(&self) -> usize {
        self.meshes.len()
    }

    /// Load sections inside the radius around a centre and unload the rest.
    ///
    /// Network scenes keep the server's subscribed sections; streaming would
    /// fabricate terrain the server does not have.
    pub fn load_around(&mut self, center: SectionPos) -> Result<(), String> {
        self.stream_around(center, usize::MAX)
    }

    /// Stream the radius window around `center`, generating at most `budget`
    /// missing sections this call.
    ///
    /// Splitting generation across frames keeps a crossing from stalling the
    /// frame; callers re-invoke until the window is complete.
    pub fn stream_around(&mut self, center: SectionPos, budget: usize) -> Result<(), String> {
        if let Some(memory) = &mut self.memory {
            memory.center = center;
        }
        if self.generator.is_none() && self.layered.is_none() {
            return Ok(());
        }
        let interest = (center, self.radius, self.layers);
        let wanted = if self.stream_interest == Some(interest) {
            self.sections.clone()
        } else {
            self.wanted_sections(center)?
        };
        if let Some(mut background) = self.background.take() {
            let result = background.stream(self, wanted, budget);
            self.background = Some(background);
            if result.is_ok() {
                self.stream_interest = Some(interest);
            }
            self.reconcile_world_memory(None)
                .map_err(|error| error.to_string())?;
            return result;
        }
        let mut generated = 0usize;
        for pos in &wanted {
            if self.authority.store().phase(*pos).is_none() {
                if generated >= budget {
                    break;
                }
                generated += 1;
                let work_bytes = 1_048_576u64
                    + u64::from(self.side).pow(3) * 16
                    + u64::from(self.side).pow(2) * 32;
                let _work = match self.reserve_memory(
                    torse_types::MemoryClass::Executor,
                    work_bytes,
                    torse_types::EvictionRank::Pinned,
                ) {
                    Ok(lease) => lease,
                    Err(error) if error.code == torse_types::FaultCode::ResourceLimit => break,
                    Err(error) => return Err(error.to_string()),
                };
                let section = match self.layered.as_ref() {
                    Some(terrain) => terrain.generate(*pos),
                    None => self
                        .generator
                        .as_ref()
                        .expect("generator checked")
                        .generate(*pos),
                }
                .map_err(|error| error.to_string())?;
                if !self.install_generated(*pos, section)? {
                    break;
                }
            }
        }
        let membership: std::collections::BTreeSet<_> = wanted.iter().copied().collect();
        for pos in self.sections.clone() {
            if !membership.contains(&pos) {
                if self.authority.store().phase(pos).is_some() {
                    self.authority
                        .store_mut()
                        .unload(pos)
                        .map_err(|e| e.to_string())?;
                }
                self.forget_mesh(&pos);
                self.load_generations.remove(&pos);
            }
        }
        self.sections = wanted;
        self.stream_interest = Some(interest);
        self.reconcile_world_memory(None)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn assign_load_generation(&mut self, pos: SectionPos) -> Result<u64, String> {
        let generation = self.next_load_generation;
        self.next_load_generation = generation
            .checked_add(1)
            .ok_or("load generation exhausted")?;
        self.load_generations.insert(pos, generation);
        Ok(generation)
    }

    fn install_generated(&mut self, pos: SectionPos, section: Section) -> Result<bool, String> {
        let bytes = size_of::<Section>() + section.allocated_bytes() + 8192;
        let growth = match self.reserve_memory(
            torse_types::MemoryClass::World,
            bytes as u64,
            torse_types::EvictionRank::Pinned,
        ) {
            Ok(lease) => lease,
            Err(error) if error.code == torse_types::FaultCode::ResourceLimit => return Ok(false),
            Err(error) => return Err(error.to_string()),
        };
        let store = self.authority.store_mut();
        store.request(pos).map_err(|e| e.to_string())?;
        store.install(pos, section).map_err(|e| e.to_string())?;
        store.finish_load(pos).map_err(|e| e.to_string())?;
        self.assign_load_generation(pos)?;
        self.reconcile_world_memory(growth)
            .map_err(|error| error.to_string())?;
        Ok(true)
    }

    fn wanted_sections(&self, center: SectionPos) -> Result<Vec<SectionPos>, String> {
        let mut wanted = Vec::new();
        for x in -self.radius..=self.radius {
            for z in -self.radius..=self.radius {
                let sx = center
                    .x
                    .checked_add(x)
                    .ok_or("streaming coordinate overflow")?;
                let sz = center
                    .z
                    .checked_add(z)
                    .ok_or("streaming coordinate overflow")?;
                for sy in 0..self.layers {
                    let pos = SectionPos::new(sx, sy, sz);
                    pos.to_block(LocalPos { x: 0, y: 0, z: 0 }, self.side)
                        .map_err(|error| error.to_string())?;
                    wanted.push(pos);
                }
            }
        }

        // Near columns first so the terrain under the camera arrives early.
        wanted.sort_by_key(|pos| {
            let dx = pos.x - center.x;
            let dz = pos.z - center.z;
            dx * dx + dz * dz
        });
        Ok(wanted)
    }

    /// Rebuild meshes whose section or neighbour revision changed.
    ///
    /// The cache holds the dependency job captured at build time; validating
    /// that stored job against the live store detects neighbour edits too.
    /// Capturing a fresh job here instead would compare the world with itself
    /// and never notice a neighbour change.
    ///
    /// Returns the rebuilt sections, so callers re-upload only what changed.
    pub fn refresh_meshes(&mut self) -> Vec<SectionPos> {
        self.refresh_meshes_bounded(usize::MAX)
    }

    fn refresh_meshes_bounded(&mut self, budget: usize) -> Vec<SectionPos> {
        let mut rebuilt = Vec::new();
        'sections: for pos in self.sections.clone() {
            if rebuilt.len() >= budget {
                break;
            }
            let unchanged = self
                .meshes
                .get(&pos)
                .is_some_and(|(job, _)| job == &MeshStamp::capture(self, pos));
            if unchanged {
                continue;
            }
            let job = MeshStamp::capture(self, pos);
            let Some(_revision) = job.terrain.section_revision else {
                continue;
            };
            let Ok(scratch) = self.reserve_memory(
                torse_types::MemoryClass::Mesh,
                torse_render::occupancy_scratch_bytes(self.side),
                torse_types::EvictionRank::Pinned,
            ) else {
                break;
            };
            let Ok(work) = self.reserve_memory(
                torse_types::MemoryClass::Executor,
                memory::mesh_work_bytes(self.side),
                torse_types::EvictionRank::Pinned,
            ) else {
                break;
            };
            let view = self.lighting.as_ref().map(|lighting| lighting.view(self));
            let mesh = build_mesh_optimized(self.side, pos, |query| {
                sample_visual(self, view.as_ref(), query)
            });
            drop(scratch);
            let lease = match work {
                Some(mut work) => loop {
                    match work.split(
                        torse_types::MemoryClass::Mesh,
                        memory::mesh_bytes(&mesh) as u64,
                        torse_types::EvictionRank::Mesh { distance_sq: 1 },
                    ) {
                        Ok(lease) => break Some(lease),
                        Err(_) if self.evict_one_mesh(pos) => continue,
                        Err(_) => break 'sections,
                    }
                },
                None => None,
            };
            self.install_mesh(pos, job, mesh, lease);
            rebuilt.push(pos);
        }
        rebuilt
    }

    /// Bootstrap only the starting section synchronously. Subsequent generation
    /// and all meshing use one shared bounded CPU pool in the windowed client.
    pub fn new_background(radius: i64) -> Result<Self, String> {
        if !(0..=16).contains(&radius) {
            return Err("scene radius must be 0..=16".to_owned());
        }
        let mut scene = Self::new(0)?;
        scene.radius = radius;
        scene.enable_background(background::default_config())?;
        Ok(scene)
    }

    /// Build a scene from a data-driven content pack directory.
    ///
    /// The directory holds the atlas index, its image and a world description.
    /// Content is compiled here, so a game renders its own texture set without
    /// the client carrying game-specific code.
    pub fn new_game(radius: i64, directory: &std::path::Path) -> Result<Self, String> {
        if !(0..=16).contains(&radius) {
            return Err("scene radius must be 0..=16".to_owned());
        }
        let game = crate::game::GamePack::load(directory)?;
        let mut pack = game.content.clone();
        block_actions::register_content(&mut pack);
        torse_inventory::register_content(&mut pack);
        torse_items::register_content(&mut pack);
        let content = compile(pack).map_err(|errors| format!("content failed: {errors:?}"))?;
        let registry = BlockStateRegistry::build(&content)
            .map_err(|error| format!("registry failed: {error}"))?;
        let schemas = ComponentSchemas::build(&content);
        let compatibility = torse_items::block_catalog(&registry)
            .map_err(|error| format!("item rules: {error:?}"))?
            .with_world_drops()
            .bind_compatibility(Compatibility {
                authority_schema: 4,
                rules_digest: content.authority_digest,
                content_digest: content.presentation_digest,
            });
        let (palette, state_materials) = material_tables(&registry, &game.texture)?;
        let side = 16;
        let bounds = WorldBounds::new(0, 64).map_err(|error| error.to_string())?;
        let terrain = game.terrain(&registry, side, bounds)?;
        let service = ActionService::bind(
            block_actions::descriptors(),
            block_actions::handlers(),
            &schemas,
        )
        .map_err(|error| format!("actions failed: {error}"))?;
        let authority =
            WorldAuthority::new(WorldId(1), registry, schemas, side, bounds, compatibility)
                .map_err(|error| error.to_string())?;
        let mut scene = Self {
            authority,
            service,
            principal: key("client:player"),
            edit_counter: 0,
            walker: None,
            items: None,
            generator: None,
            layered: Some(Arc::new(terrain)),
            side,
            radius,
            meshes: BTreeMap::new(),
            lighting: None,
            surfaces: surfaces::Previews::default(),
            sections: Vec::new(),
            stream_interest: None,
            epoch: scene_epoch()?,
            asset_generation: palette.asset_generation(),
            next_load_generation: 1,
            load_generations: BTreeMap::new(),
            background: None,
            palette,
            state_materials,
            placeable: None,
            layers: 1,
            tint_provider: None,
            tint_provider_generation: 0,
            memory: None,
            startup_notice: game.startup_notice.clone(),
        };
        scene.placeable = Some(game.placeable_blocks());
        scene.layers = (game.height_limit() + i64::from(side) - 1) / i64::from(side);
        scene.layers = scene.layers.clamp(1, 4);
        scene.set_position_tint_provider(Some(game.tint_provider()))?;
        scene.load_around(SectionPos::ZERO)?;
        Ok(scene)
    }

    pub fn enable_background(
        &mut self,
        config: torse_runtime::ExecutorConfig,
    ) -> Result<(), String> {
        if self.layered.is_some() {
            return Err("background streaming needs the built-in generator".to_owned());
        }
        let next = if let Some(pool) = self.memory_pool() {
            Background::with_memory(config, pool.clone()).map_err(|error| error.to_string())?
        } else {
            Background::new(config)?
        };
        self.background = Some(next);
        Ok(())
    }

    pub fn refresh_visuals(&mut self, budget: usize) -> Result<Vec<SectionPos>, String> {
        if budget > 0 {
            self.refresh_lighting(16_384)?;
        }
        let Some(mut background) = self.background.take() else {
            return Ok(self.refresh_meshes_bounded(budget));
        };
        let result = background.refresh(self, budget);
        self.background = Some(background);
        result
    }

    pub fn background_stats(&self) -> BackgroundStats {
        self.background
            .as_ref()
            .map(|b| b.stats())
            .unwrap_or_default()
    }

    /// Interleaved vertices and indices of the entity boxes, world space.
    pub fn entity_geometry(&self) -> (Vec<Vertex>, Vec<u32>) {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        for (handle, entity) in self.authority.entities().iter() {
            if self.append_drop_model(entity, &mut vertices, &mut indices) {
                continue;
            }
            // The walker is the camera; drawing its body would fill the view.
            if Some(handle) == self.walker.map(|walker| walker.handle)
                || entity
                    .components
                    .component(&torse_world::block_instance_schema())
                    .is_some()
                || (entity
                    .components
                    .component(&torse_inventory::schema())
                    .is_some()
                    && matches!(
                        torse_world::EntityCollider::of(entity),
                        Ok(Some(torse_world::EntityCollider::Disabled))
                    ))
            {
                continue;
            }
            push_entity_box(
                &mut vertices,
                &mut indices,
                [
                    entity.position.x as f32,
                    entity.position.y as f32,
                    entity.position.z as f32,
                ],
                u32::from(self.palette.entity_slot()),
            );
        }
        (vertices, indices)
    }

    /// Interleaved vertices and indices of one cached section, world space.
    pub fn section_geometry_bytes(&self, pos: SectionPos) -> Option<usize> {
        let (_, mesh) = self.meshes.get(&pos)?;
        Some(mesh.positions.len() * size_of::<Vertex>() + mesh.indices.len() * size_of::<u32>())
    }

    /// Interleaved vertices and indices of one cached section, world space.
    pub fn section_geometry(&self, pos: SectionPos) -> Option<(Vec<Vertex>, Vec<u32>)> {
        let (_, mesh) = self.meshes.get(&pos)?;
        let offset = [
            pos.x as f32 * self.side as f32,
            pos.y as f32 * self.side as f32,
            pos.z as f32 * self.side as f32,
        ];
        let mut vertices = Vec::with_capacity(mesh.positions.len());
        let mut indices = Vec::with_capacity(mesh.indices.len());
        for (index, position) in mesh.positions.iter().enumerate() {
            vertices.push(Vertex {
                position: [
                    position[0] + offset[0],
                    position[1] + offset[1],
                    position[2] + offset[2],
                ],
                normal: mesh.normals[index],
                uv: mesh.uvs[index],
                material: u32::from(mesh.materials[index / 4]),
                overlay_material: if mesh.overlays[index / 4] != u16::MAX {
                    u32::from(mesh.overlays[index / 4])
                } else {
                    u32::MAX
                },
                tint: mesh.tints[index],
                overlay_tint: mesh.overlay_tints[index],
                light: mesh.shades[index],
            });
        }
        indices.extend(mesh.indices.iter().copied());
        Some((vertices, indices))
    }

    /// Face count of one cached mesh, for tests.
    #[cfg(test)]
    pub fn mesh_face_count(&self, pos: SectionPos) -> Option<usize> {
        self.meshes.get(&pos).map(|(_, mesh)| mesh.face_count())
    }

    /// Section containing a world-space position on the section layer.
    pub fn center_section(&self, position: [f32; 3]) -> SectionPos {
        // Floor before the cast: `as i64` truncates toward zero, which would
        // put a camera at -16.5 into section -1 instead of -2.
        let x = (position[0].floor() as i64).div_euclid(i64::from(self.side));
        let z = (position[2].floor() as i64).div_euclid(i64::from(self.side));
        SectionPos::new(x, 0, z)
    }

    /// Flatten every cached mesh into one interleaved vertex and index buffer,
    /// then append one coloured box per stored entity so mirrored bodies are
    /// visible even before real entity art exists.
    pub fn geometry(&self) -> (Vec<Vertex>, Vec<u32>, usize) {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        let mut faces = 0usize;
        for (pos, (_, mesh)) in &self.meshes {
            let offset = [
                pos.x as f32 * self.side as f32,
                pos.y as f32 * self.side as f32,
                pos.z as f32 * self.side as f32,
            ];
            let base = vertices.len() as u32;
            for (index, position) in mesh.positions.iter().enumerate() {
                vertices.push(Vertex {
                    position: [
                        position[0] + offset[0],
                        position[1] + offset[1],
                        position[2] + offset[2],
                    ],
                    normal: mesh.normals[index],
                    uv: mesh.uvs[index],
                    material: u32::from(mesh.materials[index / 4]),
                    overlay_material: if mesh.overlays[index / 4] != u16::MAX {
                        u32::from(mesh.overlays[index / 4])
                    } else {
                        u32::MAX
                    },
                    tint: mesh.tints[index],
                    overlay_tint: mesh.overlay_tints[index],
                    light: mesh.shades[index],
                });
            }
            indices.extend(mesh.indices.iter().map(|value| base + value));
            faces += mesh.face_count();
        }
        let (entity_vertices, entity_indices) = self.entity_geometry();
        let base = vertices.len() as u32;
        faces += entity_indices.len() / 6;
        vertices.extend(entity_vertices);
        indices.extend(entity_indices.into_iter().map(|i| i + base));
        (vertices, indices, faces)
    }

    /// Summary without allocating a second copy of the entire scene geometry.
    pub fn geometry_counts(&self) -> (usize, usize) {
        let (mut vertices, mut faces) =
            self.meshes
                .values()
                .fold((0, 0), |(vertices, faces), (_, mesh)| {
                    (vertices + mesh.positions.len(), faces + mesh.face_count())
                });
        for (handle, entity) in self.authority.entities().iter() {
            if Some(handle) != self.walker.map(|walker| walker.handle)
                && entity
                    .components
                    .component(&torse_world::block_instance_schema())
                    .is_none()
            {
                vertices += 24;
                faces += 6;
            }
        }
        (vertices, faces)
    }

    /// Cover growable vertex/index vectors and their simultaneous encoded copy.
    pub fn geometry_work_bytes(&self) -> u64 {
        let (vertices, faces) = self.geometry_counts();
        (vertices as u64)
            .saturating_mul(Vertex::STRIDE as u64 * 4)
            .saturating_add((faces as u64).saturating_mul(6 * 4 * 3))
            .saturating_add(8192)
    }

    /// Encode vertices into the interleaved GPU layout.
    pub fn vertex_bytes(vertices: &[Vertex]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(vertices.len() * Vertex::STRIDE);
        for vertex in vertices {
            vertex.write(&mut bytes);
        }
        bytes
    }

    /// Camera position standing on the terrain at the scene centre.
    pub fn eye(&self) -> [f32; 3] {
        let x = 8;
        let z = 8;
        for y in (0..64).rev() {
            if let TerrainQuery::Ready(state) = self.authority.store().query(BlockPos::new(x, y, z))
                && !state.is_empty()
            {
                return [x as f32 + 0.5, y as f32 + 2.4, z as f32 + 0.5];
            }
        }
        [8.5, 12.0, 8.5]
    }
}

/// Resolve stable content into a local runtime state ID.
fn registry_state(
    registry: &BlockStateRegistry,
    block: &ContentKey,
    values: &[u16],
) -> Result<BlockStateId, String> {
    registry
        .state_of(block, values)
        .ok_or_else(|| format!("unresolved replica content {block}"))
}

/// Append one boxy body for an entity; returns the face count added.
fn push_entity_box(
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
    center: [f32; 3],
    material: u32,
) -> usize {
    const HALF: f32 = 0.35;
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [HALF, -HALF, -HALF], [HALF, HALF, HALF]),
        ([-1.0, 0.0, 0.0], [-HALF, HALF, -HALF], [-HALF, -HALF, HALF]),
        ([0.0, 1.0, 0.0], [-HALF, HALF, -HALF], [HALF, HALF, HALF]),
        ([0.0, -1.0, 0.0], [-HALF, -HALF, HALF], [HALF, -HALF, -HALF]),
        ([0.0, 0.0, 1.0], [-HALF, -HALF, HALF], [HALF, HALF, HALF]),
        ([0.0, 0.0, -1.0], [-HALF, HALF, -HALF], [HALF, -HALF, -HALF]),
    ];
    let mut added = 0usize;
    for (normal, min_corner, max_corner) in faces {
        let (nx, ny, nz) = (normal[0], normal[1], normal[2]);
        let (lo, hi) = (min_corner, max_corner);
        let corners = [
            [lo[0], lo[1], lo[2]],
            [hi[0], lo[1], lo[2]],
            [hi[0], hi[1], lo[2]],
            [lo[0], hi[1], lo[2]],
        ];
        let base = vertices.len() as u32;
        for corner in corners {
            vertices.push(Vertex {
                position: [
                    center[0] + corner[0],
                    center[1] + corner[1],
                    center[2] + corner[2],
                ],
                normal: [nx, ny, nz],
                uv: [0.0, 0.0],
                material,
                overlay_material: u32::MAX,
                tint: [1.0; 3],
                overlay_tint: [1.0; 3],
                light: -1.0,
            });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        added += 2;
    }
    added
}

fn key(value: &str) -> ContentKey {
    ContentKey::parse(value).expect("fixture key is valid")
}

fn sample(
    store: &torse_world::SectionStore,
    registry: &BlockStateRegistry,
    state_materials: &BTreeMap<BlockStateId, StateMaterials>,
    palette: &Palette,
    tint_provider: Option<&dyn torse_assets::texture::PositionTintProvider>,
    pos: BlockPos,
) -> MeshSample {
    match store.query(pos) {
        TerrainQuery::Ready(state) => {
            if state.is_empty() {
                return MeshSample::Ready(RenderBlock::EMPTY);
            }
            // The per-state render shape follows state values: an upper
            // slab renders in the top half, a lower one in the bottom.
            let Some(shape) = registry.render_shape(state) else {
                return MeshSample::Ready(RenderBlock::EMPTY);
            };
            let materials = state_materials
                .get(&state)
                .copied()
                .unwrap_or(StateMaterials {
                    base: [0; 6],
                    overlays: [None; 6],
                });
            let face_tints = std::array::from_fn(|index| {
                position_tint(palette, materials.base[index], pos, tint_provider)
            });
            let face_overlay_tints = std::array::from_fn(|index| {
                materials.overlays[index].map_or([1.0; 3], |slot| {
                    position_tint(palette, slot, pos, tint_provider)
                })
            });
            MeshSample::Ready(RenderBlock {
                bounds: render_box(&shape),
                cross: matches!(shape, Shape::Cross),
                occlusion: registry.occlusion_mask(state),
                material: 0,
                face_materials: Some(materials.base),
                face_overlays: Some(materials.overlays),
                face_tints: Some(face_tints),
                face_overlay_tints: Some(face_overlay_tints),
                face_shades: None,
            })
        }
        TerrainQuery::NotLoaded => MeshSample::NotLoaded,
        TerrainQuery::OutOfBounds | TerrainQuery::Exterior => MeshSample::OutOfBounds,
    }
}

fn sample_visual(
    scene: &Scene,
    lighting: Option<&lighting::View<'_>>,
    pos: BlockPos,
) -> MeshSample {
    let mut value = sample(
        scene.authority.store(),
        scene.authority.registry(),
        &scene.state_materials,
        &scene.palette,
        scene.tint_provider.as_deref(),
        pos,
    );
    if let (Some(view), MeshSample::Ready(block)) = (lighting, &mut value)
        && block.bounds.is_some()
    {
        view.shade(scene, pos, block);
    }
    value
}

fn render_box(shape: &Shape) -> Option<RenderBox> {
    shape_box(shape)
}

fn shape_box(shape: &Shape) -> Option<RenderBox> {
    match shape {
        Shape::Model(_) | Shape::Compound(_) => None,
        Shape::FullCube => Some(RenderBox::FULL_CUBE),
        Shape::LowerSlab => Some(RenderBox {
            min: [0.0, 0.0, 0.0],
            max: [1.0, 0.5, 1.0],
        }),
        Shape::Panel => Some(RenderBox {
            min: [0.45, 0.0, 0.0],
            max: [0.55, 1.0, 1.0],
        }),
        Shape::Cross => Some(RenderBox::FULL_CUBE),
        Shape::Corner | Shape::Stair => Some(RenderBox::FULL_CUBE),
        Shape::Box(bounds) => Some(RenderBox {
            min: bounds.min,
            max: bounds.max,
        }),
    }
}

fn position_tint(
    palette: &Palette,
    slot: u16,
    position: BlockPos,
    provider: Option<&dyn torse_assets::texture::PositionTintProvider>,
) -> [f32; 3] {
    let (Some(source), Some(provider)) = (palette.tint_source(slot), provider) else {
        return [1.0; 3];
    };
    let color = provider.sample(source, position);
    torse_assets::texture::srgb_to_linear(color.map(|channel| f32::from(channel) / 255.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use torse_types::EntityId;

    #[test]
    fn foreground_visuals_honor_the_frame_budget_and_eventually_match_full_refresh() {
        let mut scene = Scene::new(1).unwrap();
        assert!(scene.background.is_none());
        assert!(scene.refresh_visuals(0).unwrap().is_empty());
        assert_eq!(scene.mesh_count(), 0);
        let sections = scene.sections.len();
        for expected in 1..=sections {
            assert_eq!(scene.refresh_visuals(1).unwrap().len(), 1);
            assert_eq!(scene.mesh_count(), expected);
        }
        assert!(scene.refresh_visuals(1).unwrap().is_empty());
        let incremental = scene.geometry();
        scene.meshes.clear();
        assert_eq!(scene.refresh_meshes().len(), sections);
        assert_eq!(scene.geometry(), incremental);
    }
    struct RegionTint(AtomicU64);

    impl torse_assets::texture::PositionTintProvider for RegionTint {
        fn sample(
            &self,
            _source: torse_assets::texture::TintSource,
            position: BlockPos,
        ) -> [u8; 3] {
            match self.0.load(Ordering::Relaxed) {
                0 if position.x < 8 => [255, 0, 0],
                0 => [0, 0, 255],
                _ => [0, 255, 0],
            }
        }

        fn revision(&self, _section: SectionPos) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn scene_generates_and_meshes_its_radius() {
        let mut scene = Scene::new(1).expect("scene builds");
        assert_eq!(scene.sections().len(), 9);
        assert_eq!(scene.refresh_meshes().len(), 9);
        assert_eq!(scene.mesh_count(), 9);
        // Nothing changed, so the second pass rebuilds nothing.
        assert_eq!(scene.refresh_meshes().len(), 0);
        let (vertices, indices, faces) = scene.geometry();
        assert!(!vertices.is_empty());
        assert!(!indices.is_empty());
        assert!(faces > 0);
        assert_eq!(indices.len() % 3, 0);
    }

    #[test]
    fn serial_visual_admission_honors_zero_and_one_section_budgets() {
        let mut scene = Scene::new(1).unwrap();
        assert!(scene.refresh_visuals(0).unwrap().is_empty());
        assert_eq!(scene.mesh_count(), 0);
        assert_eq!(scene.refresh_visuals(1).unwrap().len(), 1);
        assert_eq!(scene.mesh_count(), 1);
        assert_eq!(scene.refresh_visuals(1).unwrap().len(), 1);
        assert_eq!(scene.mesh_count(), 2);
    }

    #[test]
    fn neighbour_edit_invalidates_the_border_mesh() {
        let mut scene = Scene::new(1).expect("scene builds");
        scene.refresh_meshes();
        let stone = key("base:stone");
        // X sits in section (0,0,0) against the border, exposing a +x face
        // into neighbour (1,0,0).
        scene
            .apply_edit(true, &stone, BlockPos::new(15, 15, 1), 0)
            .expect("place X");
        scene.refresh_meshes();
        let faces_before = scene.mesh_face_count(SectionPos::ZERO).expect("cached");

        // Y lands in the neighbour and occludes that face; the border
        // section's own revision does not change.
        scene
            .apply_edit(true, &stone, BlockPos::new(16, 15, 1), 0)
            .expect("place Y");
        let rebuilt = scene.refresh_meshes();
        assert!(
            rebuilt.len() >= 2,
            "neighbour and border section must rebuild"
        );

        let faces_after = scene.mesh_face_count(SectionPos::ZERO).expect("cached");
        assert_eq!(
            faces_before - faces_after,
            1,
            "the occluded border face is gone"
        );
    }

    #[test]
    fn streaming_recenters_without_leaking_sections() {
        let mut scene = Scene::new(1).expect("scene builds");
        scene.refresh_meshes();
        let before = scene.geometry();
        assert!(!before.0.is_empty());

        scene.load_around(SectionPos::new(3, 0, 0)).expect("load");
        assert_eq!(scene.sections().len(), 9);
        // The unloaded column's meshes are gone; the new ones are built.
        assert!(!scene.refresh_meshes().is_empty());
        assert_eq!(scene.mesh_count(), 9);
        let after = scene.geometry();
        assert!(!after.0.is_empty());

        // Returning to the origin reloads the same deterministic terrain.
        scene.load_around(SectionPos::ZERO).expect("load back");
        scene.refresh_meshes();
        assert_eq!(scene.sections().len(), 9);
        assert_eq!(scene.mesh_count(), 9);
        assert_eq!(scene.geometry().0.len(), before.0.len());
    }

    #[test]
    fn scrolling_once_then_flying_keeps_streaming_bounded() {
        let mut scene = Scene::new(0).unwrap();
        let mut camera = crate::camera::FlyCamera::new(scene.eye());
        let mut input = crate::input::InputState::new();
        input.key(
            winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyW),
            winit::event::ElementState::Pressed,
        );
        input.wheel(winit::event::MouseScrollDelta::LineDelta(0.0, 1.0));
        let mut center = scene.center_section(camera.eye);
        let mut streamed = 0;
        for _ in 0..3600 {
            let (motion, speed) = input.take_input(camera.speed);
            camera.speed = speed;
            camera.advance(motion, 1.0 / 60.0);
            assert_eq!(camera.speed, 15.0);
            assert!(camera.eye.iter().all(|v| v.is_finite()));
            let next = scene.center_section(camera.eye);
            if next != center {
                scene.load_around(next).unwrap();
                center = next;
                streamed += 1;
                assert_eq!(scene.sections().len(), 1);
            }
        }
        assert!(
            streamed > 40,
            "exercise repeated section loading after the wheel event"
        );
    }

    #[test]
    fn streaming_rejects_overflow_before_changing_the_scene() {
        let mut scene = Scene::new(1).unwrap();
        let before = scene.sections().to_vec();
        for center in [
            SectionPos::new(i64::MAX, 0, 0),
            SectionPos::new(0, 0, i64::MIN),
        ] {
            assert!(scene.load_around(center).is_err());
            assert_eq!(scene.sections(), before);
        }
    }

    #[test]
    fn center_section_maps_positions_including_negatives() {
        let scene = Scene::new(0).expect("scene builds");
        assert_eq!(scene.center_section([0.0, 5.0, 0.0]), SectionPos::ZERO);
        assert_eq!(
            scene.center_section([17.5, 5.0, -1.0]),
            SectionPos::new(1, 0, -1)
        );
        assert_eq!(
            scene.center_section([-16.0, 5.0, -17.0]),
            SectionPos::new(-1, 0, -2)
        );
        // Non-integer negatives must floor, not truncate toward zero.
        assert_eq!(
            scene.center_section([-16.5, 5.0, -0.5]),
            SectionPos::new(-2, 0, -1)
        );
        assert_eq!(
            scene.center_section([-0.5, 5.0, 15.5]),
            SectionPos::new(-1, 0, 0)
        );
    }

    #[test]
    fn actual_palette_assets_stamp_the_scene() {
        let scene = Scene::new(1).unwrap();
        assert_eq!(scene.asset_generation, scene.palette.asset_generation());
        assert_eq!(
            scene.asset_generation,
            u64::from(crate::texture::MIP_LEVELS)
        );
        assert!(!scene.palette.mips().is_empty());
    }

    #[test]
    fn vertex_bytes_match_the_documented_stride() {
        let vertices = [Vertex {
            position: [1.0, 2.0, 3.0],
            normal: [0.0, 1.0, 0.0],
            uv: [0.25, 0.75],
            material: 3,
            overlay_material: u32::MAX,
            tint: [1.0; 3],
            overlay_tint: [1.0; 3],
            light: -1.0,
        }];
        let bytes = Scene::vertex_bytes(&vertices);
        assert_eq!(bytes.len(), Vertex::STRIDE);
        // Material and overlay slots follow the position, normal, and UV.
        assert_eq!(&bytes[32..36], &3u32.to_le_bytes());
        assert_eq!(&bytes[36..40], &u32::MAX.to_le_bytes());
    }

    #[test]
    fn position_tints_vary_by_region_and_rebuild_on_provider_revision() {
        let atlas = torse_assets::texture::Atlas::decode(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../games/base/assets/textures/atlas.png"
        )))
        .expect("atlas decodes");
        let pack = TexturePack::new(
            atlas,
            vec![torse_assets::texture::MaterialEntry {
                key: "base:material/stone",
                rect: [4, 4, 16, 16],
                tint_source: Some(torse_assets::texture::TintSource::Grass),
                ..torse_assets::texture::MaterialEntry::new("", [0; 4])
            }],
            [76, 4, 16, 16],
            [100, 4, 16, 16],
        )
        .expect("pack builds");
        let provider = Arc::new(RegionTint(AtomicU64::new(0)));
        let mut scene = Scene::with_pack(0, pack).expect("scene builds");
        scene
            .set_position_tint_provider(Some(provider.clone()))
            .expect("provider installs");
        assert_eq!(scene.refresh_meshes(), [SectionPos::ZERO]);

        let vertices = scene.geometry().0;
        assert!(vertices.iter().any(|vertex| vertex.tint == [1.0, 0.0, 0.0]));
        assert!(vertices.iter().any(|vertex| vertex.tint == [0.0, 0.0, 1.0]));

        provider.0.store(1, Ordering::Relaxed);
        assert_eq!(scene.refresh_meshes(), [SectionPos::ZERO]);
        let vertices = scene.geometry().0;
        assert!(vertices.iter().all(|vertex| vertex.tint == [0.0, 1.0, 0.0]));
    }

    fn query(scene: &Scene, pos: BlockPos) -> bool {
        matches!(
            scene.authority().store().query(pos),
            TerrainQuery::Ready(state) if !state.is_empty()
        )
    }

    /// Find a column whose surface leaves a loaded, empty cell above it.
    /// Only the `y = 0` section layer is loaded, so test cells must stay below
    /// 16.
    fn air_above_surface(scene: &Scene) -> BlockPos {
        for x in 0..12i64 {
            for z in 0..12i64 {
                for y in (0..14i64).rev() {
                    let pos = BlockPos::new(x, y, z);
                    if query(scene, pos) {
                        return BlockPos::new(x, y + 1, z);
                    }
                }
            }
        }
        panic!("no air cell found above the surface");
    }

    #[test]
    fn apply_edit_places_and_breaks_through_the_action_service() {
        let mut scene = Scene::new(1).expect("scene builds");
        let target = air_above_surface(&scene);
        let surface = BlockPos::new(target.x, target.y - 1, target.z);
        assert!(!query(&scene, target));
        assert!(query(&scene, surface));

        let stone = key("base:stone");
        scene
            .apply_edit(true, &stone, target, 0)
            .expect("place commits");
        assert!(query(&scene, target));

        // Placing into the now occupied cell is rejected without a commit.
        let again = scene.apply_edit(true, &stone, target, 0);
        assert!(again.is_err());

        scene
            .apply_edit(false, &stone, target, 0)
            .expect("break commits");
        assert!(!query(&scene, target));

        // Breaking air is a conflict, not a silent no-op.
        let empty = scene.apply_edit(false, &stone, target, 0);
        assert!(empty.is_err());
    }

    #[test]
    fn two_slabs_merge_into_one_full_block() {
        let mut scene = Scene::new(1).expect("scene builds");
        let target = air_above_surface(&scene);

        let slab = key("base:glass_slab");
        scene
            .apply_edit(true, &slab, target, 0)
            .expect("first slab");

        // Placing the same slab into the occupied cell merges the pair into
        // one full glass block, exactly what the top-face aim sends.
        scene
            .apply_edit(true, &slab, target, 1)
            .expect("second slab merges");
        let state = scene
            .authority()
            .store()
            .query(target)
            .ready()
            .expect("merged cell holds a block");
        assert_eq!(
            scene.authority().registry().definition_key(state),
            Some(&key("base:glass"))
        );

        // The full block completes no further; a third slab is a conflict.
        let third = scene.apply_edit(true, &slab, target, 0);
        assert!(third.is_err());
    }

    /// Render bounds of one cell's sampled block, in world space. This is the
    /// shape the mesher receives, straight from the per-state registry query.
    fn sampled_bounds(scene: &Scene, cell: BlockPos) -> (f32, f32) {
        match scene.authority.store().query(cell) {
            TerrainQuery::Ready(state) if !state.is_empty() => {
                let shape = scene
                    .authority
                    .registry()
                    .render_shape(state)
                    .expect("known state has a shape");
                let bounds = shape_box(&shape).expect("shape has bounds");
                (cell.y as f32 + bounds.min[1], cell.y as f32 + bounds.max[1])
            }
            _ => panic!("cell holds a shaped block"),
        }
    }

    #[test]
    fn placement_half_parameter_selects_the_cell_half() {
        let mut scene = Scene::new(1).expect("scene builds");
        scene.refresh_meshes();
        let target = air_above_surface(&scene);
        let glass = key("base:glass_slab");

        // half=1 places the upper half: the sampled bounds sit above the
        // midline.
        scene
            .apply_edit(true, &glass, target, 1)
            .expect("place upper");
        let (min_y, max_y) = sampled_bounds(&scene, target);
        assert!(
            (min_y - (target.y as f32 + 0.5)).abs() < 1e-4,
            "upper slab floor {min_y} must sit at the midline"
        );
        assert!((max_y - (target.y as f32 + 1.0)).abs() < 1e-4);
        scene.apply_edit(false, &glass, target, 0).expect("clear");

        // half=0 places the lower half: the bounds sit below the midline.
        scene
            .apply_edit(true, &glass, target, 0)
            .expect("place lower");
        let (min_y, max_y) = sampled_bounds(&scene, target);
        assert!((min_y - target.y as f32).abs() < 1e-4);
        assert!(
            (max_y - (target.y as f32 + 0.5)).abs() < 1e-4,
            "lower slab top {max_y} must sit at the midline"
        );
    }

    #[test]
    fn slab_and_plant_meshes_stay_within_their_shapes() {
        let mut scene = Scene::new(1).expect("scene builds");
        scene.refresh_meshes();
        let target = air_above_surface(&scene);

        let glass = key("base:glass_slab");
        scene
            .apply_edit(true, &glass, target, 0)
            .expect("place slab");
        let (slab_min_y, slab_max_y) = sampled_bounds(&scene, target);
        // A lower slab fills only the bottom half of the cell.
        assert!((slab_min_y - target.y as f32).abs() < 1e-4);
        assert!(
            (slab_max_y - (target.y as f32 + 0.5)).abs() < 1e-4,
            "slab top {slab_max_y} must sit at the cell midline"
        );

        let plant = key("base:plant");
        scene.apply_edit(false, &glass, target, 0).expect("clear");
        scene
            .apply_edit(true, &plant, target, 0)
            .expect("place plant");
        // Crossed quads span the cell; selection uses a separate tight box.
        let (plant_min_x, plant_max_x, _plant_min_y, plant_max_y) = {
            match scene.authority.store().query(target) {
                TerrainQuery::Ready(state) => {
                    let shape = scene
                        .authority
                        .registry()
                        .render_shape(state)
                        .expect("plant shape");
                    let bounds = shape_box(&shape).expect("plant bounds");
                    (
                        target.x as f32 + bounds.min[0],
                        target.x as f32 + bounds.max[0],
                        target.y as f32 + bounds.min[1],
                        target.y as f32 + bounds.max[1],
                    )
                }
                other => panic!("plant state: {other:?}"),
            }
        };
        assert!((plant_min_x - target.x as f32).abs() < 1e-4);
        assert!((plant_max_x - (target.x as f32 + 1.0)).abs() < 1e-4);
        assert!((plant_max_y - (target.y as f32 + 1.0)).abs() < 1e-4);
    }

    #[test]
    fn crossed_plant_uses_a_tight_pick_box_and_outline_without_blocking_walking() {
        let mut scene = Scene::new(0).unwrap();
        let plant = key("base:plant");
        let pos = BlockPos::new(8, 14, 8);
        scene.apply_edit(true, &plant, pos, 0).unwrap();
        scene
            .apply_edit(true, &key("base:stone"), BlockPos::new(10, 14, 8), 0)
            .unwrap();
        let state = scene.authority().store().query(pos).ready().unwrap();
        let registry = scene.authority().registry();
        assert_eq!(registry.render_shape(state), Some(Shape::Cross));
        assert!(registry.collision_shape(state).is_none());
        let hit = scene.pick([6.5, 14.5, 8.5], [1.0, 0.0, 0.0], 64.0);
        let PickOutcome::Hit(hit) = hit else {
            panic!("expected the plant");
        };
        assert_eq!(hit.block, pos);
        assert!((hit.entry[0] - 8.125).abs() < 1e-5);
        let bytes = scene.outline_bytes(&hit);
        assert_eq!(bytes.len(), 24 * Vertex::STRIDE);
        for vertex in bytes.chunks_exact(Vertex::STRIDE) {
            let x = f32::from_le_bytes(vertex[..4].try_into().unwrap());
            assert!((8.1209..=8.8791).contains(&x));
        }
        let outside = scene.pick([6.5, 14.5, 8.05], [1.0, 0.0, 0.0], 64.0);
        assert!(matches!(outside, PickOutcome::Hit(hit) if hit.block == BlockPos::new(10, 14, 8)));
        let at = Position::new(8.5, 14.4, 8.5);
        assert_eq!(scene.spawn_walker(at).unwrap(), at);
        let moved = scene.step_walker([1.0, 0.0, 0.0], false, 0.05).unwrap().0;
        assert!(moved.x > at.x);
    }

    #[test]
    fn apply_edit_rejects_out_of_bounds_targets() {
        let mut scene = Scene::new(1).expect("scene builds");
        let stone = key("base:stone");
        assert!(
            scene
                .apply_edit(true, &stone, BlockPos::new(0, 64, 0), 0)
                .is_err()
        );
        assert!(
            scene
                .apply_edit(true, &stone, BlockPos::new(0, -1, 0), 0)
                .is_err()
        );
        // Above the loaded layer the target is not loaded, which is also a
        // rejection rather than a silent drop.
        assert!(
            scene
                .apply_edit(true, &stone, BlockPos::new(0, 40, 0), 0)
                .is_err()
        );
    }

    #[test]
    fn pick_finds_the_surface_below_the_camera() {
        let scene = Scene::new(1).expect("scene builds");
        // Start inside the loaded layer so the ray can walk down to the hill.
        let outcome = scene.pick([8.5, 15.5, 8.5], [0.0, -1.0, 0.0], 128.0);
        let PickOutcome::Hit(hit) = outcome else {
            panic!("expected the terrain surface");
        };
        assert_eq!((hit.block.x, hit.block.z), (8, 8));
        assert!(hit.block.y < 16);
        assert!(hit.distance < 16.0);
    }

    #[test]
    fn pick_stops_at_unloaded_sections() {
        let scene = Scene::new(1).expect("scene builds");
        // Radius 1 leaves everything past 31 blocks unloaded on the x axis.
        let outcome = scene.pick([30.5, 40.0, 8.5], [1.0, 0.0, 0.0], 256.0);
        assert_eq!(outcome, PickOutcome::Miss);
    }

    #[test]
    fn picking_from_outside_vertical_bounds_keeps_unknown_sections_blocking() {
        let mut scene = Scene::new(0).unwrap();
        let stone = key("base:stone");
        scene
            .apply_edit(true, &stone, BlockPos::new(8, 14, 8), 0)
            .unwrap();
        // y=63 is inside this world's bounds but has not been loaded. A ray
        // starting above the world may pass outside space, never this gap.
        assert_eq!(
            scene.pick([8.5, 65.5, 8.5], [0.0, -1.0, 0.0], 64.0),
            PickOutcome::Miss
        );
        // Unknown horizontal sections also stop a ray before known terrain.
        assert_eq!(
            scene.pick([-1.5, 14.5, 8.5], [1.0, 0.0, 0.0], 64.0),
            PickOutcome::Miss
        );
        let below = scene.pick([8.5, -1.5, 8.5], [0.0, 1.0, 0.0], 64.0);
        let PickOutcome::Hit(hit) = below else {
            panic!("expected the lowest loaded block");
        };
        assert_eq!(hit.block, BlockPos::new(8, 0, 8));
        assert_eq!(hit.face, [0, -1, 0]);
    }

    #[test]
    fn replica_bounds_with_negative_top_allow_entry_from_above() {
        let mut snapshot = replica_snapshot();
        snapshot.bounds = WorldBounds::new(-32, -16).unwrap();
        snapshot.sections = vec![stone_layer_section(SectionPos::new(0, -2, 0))];
        snapshot.entities.clear();
        let scene = Scene::from_snapshot(&snapshot).unwrap();
        let outcome = scene.pick([8.5, -14.5, 8.5], [0.0, -1.0, 0.0], 64.0);
        let PickOutcome::Hit(hit) = outcome else {
            panic!("expected the mirrored floor");
        };
        assert_eq!(hit.block, BlockPos::new(8, -32, 8));
        assert_eq!(hit.face, [0, 1, 0]);
        assert!((hit.distance - 16.5).abs() < 1e-4);
        assert_eq!(
            scene.pick([8.5, -14.5, 8.5], [0.0, -1.0, 0.0], 16.0),
            PickOutcome::Miss
        );
    }

    fn stone_layer_section(pos: SectionPos) -> SnapshotSection {
        let side = 16u32;
        let mut cells = vec![BlockStateId::EMPTY; side.pow(3) as usize];
        for x in 0..side {
            for z in 0..side {
                let index = x as usize + side as usize * z as usize;
                cells[index] = BlockStateId(7);
            }
        }
        SnapshotSection {
            pos,
            cells: SnapshotCells::Dense(cells),
        }
    }

    fn replica_snapshot() -> ReplicaSnapshot {
        ReplicaSnapshot {
            side: 16,
            bounds: WorldBounds::new(0, 32).expect("bounds"),
            states: vec![StateEntry {
                id: BlockStateId(7),
                block: key("base:stone"),
                values: Vec::new(),
            }],
            sections: vec![
                stone_layer_section(SectionPos::ZERO),
                stone_layer_section(SectionPos::new(1, 0, 0)),
            ],
            entities: vec![ReplicaEntity {
                id: EntityId(9),
                position_bits: Position::new(2.5, 2.0, 2.0).to_bits(),
                components: BTreeMap::new(),
            }],
        }
    }

    #[test]
    fn network_scene_installs_a_replica_baseline() {
        let mut scene = Scene::from_snapshot(&replica_snapshot()).expect("scene");
        assert!(scene.is_network());
        // The symbol table resolved remote runtime ID 7 into local stone.
        assert!(query(&scene, BlockPos::new(3, 0, 4)));
        assert!(!query(&scene, BlockPos::new(3, 1, 4)));
        // The mirrored entity landed in the client entity store.
        assert_eq!(scene.authority().entities().len(), 1);
        let stored = scene.authority().entities().get(
            scene
                .authority()
                .entities()
                .handle_of(EntityId(9))
                .expect("entity"),
        );
        assert_eq!(stored.expect("entity").position.y, 2.0);
        // Meshes build from the mirrored sections alone.
        assert_eq!(scene.refresh_meshes().len(), 2);
        let (vertices, indices, _) = scene.geometry();
        assert!(!vertices.is_empty() && !indices.is_empty());
        // Streaming must not fabricate terrain around the mirror.
        scene.load_around(SectionPos::new(5, 0, 5)).expect("noop");
        assert_eq!(scene.sections().len(), 2);
    }

    #[test]
    fn both_geometry_paths_preserve_vertical_section_offsets() {
        let mut snapshot = replica_snapshot();
        snapshot.entities.clear();
        snapshot.sections = vec![SnapshotSection {
            pos: SectionPos::new(0, 1, 0),
            cells: SnapshotCells::Uniform(BlockStateId(7)),
        }];
        let mut scene = Scene::from_snapshot(&snapshot).unwrap();
        scene.refresh_meshes();
        let (vertices, _) = scene.section_geometry(SectionPos::new(0, 1, 0)).unwrap();
        assert!(vertices.iter().all(|v| v.position[1] >= 16.0));
        assert!(vertices.iter().any(|v| v.position[1] == 32.0));
        assert_eq!(scene.geometry().0, vertices);
    }

    #[test]
    fn network_scene_rejects_local_walking_without_changing_the_replica() {
        let mut scene = Scene::from_snapshot(&replica_snapshot()).expect("scene");
        let meta = torse_sim::streaming::page_meta(scene.authority());
        let before = scene.authority().capture_snapshot(meta).expect("snapshot");
        assert_eq!(
            scene.spawn_walker(Position::new(8.5, 12.0, 8.5)),
            Err("network mirrors cannot settle local movement".to_owned())
        );
        assert!(scene.walker.is_none());
        // Even a stale walker handle must not let local movement settle
        // against a server-owned entity.
        let handle = scene
            .authority()
            .entities()
            .handle_of(EntityId(9))
            .expect("entity");
        scene.walker = Some(torse_sim::player::WalkerState::new(handle));
        assert_eq!(
            scene.step_walker([0.0, 0.0, -1.0], true, 0.05),
            Err("network mirrors cannot settle local movement".to_owned())
        );
        assert_eq!(scene.edit_counter, 0);
        assert_eq!(
            scene.authority().capture_snapshot(meta).expect("snapshot"),
            before
        );
    }

    #[test]
    fn attached_block_instances_do_not_add_entity_placeholder_geometry() {
        let mut snapshot = replica_snapshot();
        let mut ordinary = Scene::from_snapshot(&snapshot).expect("ordinary scene");
        ordinary.refresh_meshes();
        let binding = torse_world::BlockInstance {
            pos: BlockPos::new(2, 0, 0),
            block: key("base:stone"),
        };
        snapshot.entities.push(ReplicaEntity {
            id: EntityId(10),
            position_bits: binding.position().to_bits(),
            components: [(torse_world::block_instance_schema(), binding.fields())].into(),
        });
        let mut attached = Scene::from_snapshot(&snapshot).expect("attached scene");
        attached.refresh_meshes();
        assert_eq!(attached.authority().entities().len(), 2);
        assert_eq!(attached.entity_geometry().1, ordinary.entity_geometry().1);
        assert_eq!(attached.geometry().1, ordinary.geometry().1);
        assert_eq!(attached.geometry().2, ordinary.geometry().2);
    }

    #[test]
    fn network_scene_applies_a_replica_increment() {
        let mut scene = Scene::from_snapshot(&replica_snapshot()).expect("scene");
        scene.refresh_meshes();
        // Carve one hole by replacing the section without its stone at x=3,z=4.
        let mut cells = vec![BlockStateId(7); 16usize.pow(3)];
        for y in 0..16usize {
            for x in 0..16usize {
                for z in 0..16usize {
                    let index = x + 16 * (z + 16 * y);
                    cells[index] = if y == 0 && x != 3 && z != 4 {
                        BlockStateId(7)
                    } else {
                        BlockStateId::EMPTY
                    };
                }
            }
        }
        let changes = ReplicaChanges {
            states: vec![StateEntry {
                id: BlockStateId(7),
                block: key("base:stone"),
                values: Vec::new(),
            }],
            sections: vec![SnapshotSection {
                pos: SectionPos::ZERO,
                cells: SnapshotCells::Dense(cells),
            }],
            removed_sections: Vec::new(),
            entities: Vec::new(),
            removed_entities: Vec::new(),
        };
        scene.apply_changes(&changes).expect("apply");
        assert!(!query(&scene, BlockPos::new(3, 0, 4)));
        assert!(query(&scene, BlockPos::new(5, 0, 5)));
        // The replaced section rebuilds despite matching revision numbers,
        // and the neighbour rebuilds because it observed the old revision.
        assert_eq!(scene.refresh_meshes().len(), 2);
    }

    #[test]
    fn stacked_slabs_keep_the_gap_plane() {
        let mut scene = Scene::new(1).expect("scene builds");
        scene.refresh_meshes();
        let base = air_above_surface(&scene);
        let glass = key("base:glass_slab");
        scene
            .apply_edit(true, &glass, base, 0)
            .expect("place lower");
        let upper = BlockPos::new(base.x, base.y + 1, base.z);
        scene
            .apply_edit(true, &glass, upper, 0)
            .expect("place upper");
        scene.refresh_meshes();

        // Same-material neighbours merge: no face lies flat on the shared
        // plane (side quads touch it with their bottom edge only).
        let boundary = base.y as f32 + 1.0;
        let (vertices, _, _) = scene.geometry();
        let flat_on_plane = vertices
            .iter()
            .filter(|v| {
                (v.position[1] - boundary).abs() < 1e-4
                    && v.normal[1].abs() > 0.5
                    && v.position[0] >= base.x as f32
                    && v.position[0] <= base.x as f32 + 1.0
                    && v.position[2] >= base.z as f32
                    && v.position[2] <= base.z as f32 + 1.0
            })
            .count();
        // The upper slab's bottom faces the half-block gap between the two
        // slabs, so it must render; the lower slab never reaches the plane.
        assert_eq!(
            flat_on_plane, 4,
            "the upper slab's bottom quad must face the gap"
        );
    }

    #[test]
    fn glass_keeps_its_face_against_a_different_transparent_block() {
        let mut scene = Scene::new(1).expect("scene builds");
        scene.refresh_meshes();
        let base = air_above_surface(&scene);
        let glass = key("base:glass_slab");
        let plant = key("base:plant");
        // Glass with a plant in the x-adjacent cell: different states keep
        // the shared face (the plant occludes nothing).
        scene
            .apply_edit(true, &glass, base, 0)
            .expect("place glass");
        let beside = BlockPos::new(base.x + 1, base.y, base.z);
        scene
            .apply_edit(true, &plant, beside, 0)
            .expect("place plant");
        scene.refresh_meshes();

        let plane = base.x as f32 + 1.0;
        let (vertices, _, _) = scene.geometry();
        let on_plane = vertices
            .iter()
            .filter(|v| {
                (v.position[0] - plane).abs() < 1e-4
                    && v.normal[0].abs() > 0.5
                    && v.position[2] >= base.z as f32
                    && v.position[2] <= base.z as f32 + 1.0
                    && v.position[1] >= base.y as f32
                    && v.position[1] <= base.y as f32 + 0.5
            })
            .count();
        assert!(on_plane > 0, "differing neighbours must keep the face");
    }

    #[test]
    fn walker_spawns_mid_air_walks_and_ends_standing() {
        let mut scene = Scene::new(3).expect("scene");
        scene
            .spawn_walker(Position::new(8.5, 13.3, 8.5))
            .expect("spawn");
        let mut final_position = Position::new(8.5, 13.3, 8.5);
        let mut grounded = false;
        for _ in 0..31 {
            let (position, landed) = scene
                .step_walker([0.0, 0.0, -1.0], false, 0.05)
                .expect("step");
            final_position = position;
            grounded = landed;
        }
        // Walked into negative z and ended standing on the terrain.
        assert!(final_position.z < 8.0, "moved to {final_position:?}");
        assert!(grounded);
        assert!(final_position.y > 0.0 && final_position.y < 16.0);
    }

    #[test]
    fn rejected_and_duplicate_walk_entry_leave_entity_counts_unchanged() {
        let mut scene = Scene::new(0).unwrap();
        for position in [
            Position::new(30.5, 12.0, 8.5),
            Position::new(8.5, 1.0, 8.5),
            Position::new(f64::NAN, 12.0, 8.5),
        ] {
            assert!(scene.spawn_walker(position).is_err());
            assert_eq!(scene.authority().entities().len(), 0);
            assert!(scene.walker.is_none());
        }
        scene.spawn_walker(Position::new(8.5, 13.0, 8.5)).unwrap();
        let handle = scene.walker.unwrap().handle;
        assert!(scene.spawn_walker(Position::new(8.5, 14.0, 8.5)).is_err());
        assert_eq!(scene.authority().entities().len(), 1);
        scene.stop_walker().unwrap();
        scene.stop_walker().unwrap();
        assert!(scene.authority().entities().get(handle).is_none());
        scene.spawn_walker(Position::new(8.5, 13.0, 8.5)).unwrap();
        assert_ne!(scene.walker.unwrap().handle, handle);
    }

    #[test]
    fn leaving_network_walk_only_clears_the_camera_binding() {
        let mut scene = Scene::from_snapshot(&replica_snapshot()).unwrap();
        let meta = torse_sim::streaming::page_meta(scene.authority());
        let before = scene.authority().capture_snapshot(meta).unwrap();
        scene.view_player(EntityId(9));
        assert!(scene.walker.is_some());
        scene.stop_walker().unwrap();
        assert!(scene.walker.is_none());
        assert_eq!(scene.authority().capture_snapshot(meta).unwrap(), before);
    }

    #[test]
    fn walker_falls_until_it_lands_and_stays() {
        let mut scene = Scene::new(1).expect("scene builds");
        scene
            .spawn_walker(Position::new(8.5, 12.0, 8.5))
            .expect("spawn");
        let mut settled = Position::new(8.5, 12.0, 8.5);
        for _ in 0..100 {
            let (position, grounded) = scene
                .step_walker([0.0, 0.0, 0.0], false, 0.05)
                .expect("step");
            settled = position;
            if grounded {
                break;
            }
        }
        let (_, grounded) = scene
            .step_walker([0.0, 0.0, 0.0], false, 0.05)
            .expect("settle");
        assert!(grounded, "must land within five seconds");
        // Standing on the surface: inside the loaded layer, x and z unchanged.
        assert!((settled.x - 8.5).abs() < 1e-9);
        assert!((settled.z - 8.5).abs() < 1e-9);
        assert!(settled.y > 0.0 && settled.y < 16.0);
    }

    #[test]
    fn walker_walks_forward_over_rolling_terrain() {
        let mut scene = Scene::new(1).expect("scene builds");
        scene
            .spawn_walker(Position::new(8.5, 12.0, 8.5))
            .expect("spawn");
        // Warm up until grounded.
        for _ in 0..100 {
            let (_, grounded) = scene
                .step_walker([0.0, 0.0, 0.0], false, 0.05)
                .expect("step");
            if grounded {
                break;
            }
        }
        let start = scene.walker_position().expect("walker");
        let mut final_position = start;
        for _ in 0..60 {
            final_position = scene
                .step_walker([0.0, 0.0, -1.0], false, 0.05)
                .expect("step")
                .0;
        }
        // Three seconds of walking moves along the surface until a terrain
        // step blocks the collider; hills stop it early, so only require real
        // progress plus a plausible standing height.
        assert!(
            start.z - final_position.z > 2.0,
            "moved {:?} to {:?}",
            start,
            final_position
        );
        assert!(final_position.y > 0.0 && final_position.y < 16.0);
        assert!((final_position.x - start.x).abs() < 1e-9);
    }

    #[test]
    fn walker_jumps_and_lands_back() {
        let mut scene = Scene::new(1).expect("scene builds");
        scene
            .spawn_walker(Position::new(8.5, 12.0, 8.5))
            .expect("spawn");
        for _ in 0..100 {
            let (_, grounded) = scene
                .step_walker([0.0, 0.0, 0.0], false, 0.05)
                .expect("step");
            if grounded {
                break;
            }
        }
        let ground = scene.walker_position().expect("walker").y;

        let (risen, grounded) = scene
            .step_walker([0.0, 0.0, 0.0], true, 0.05)
            .expect("jump tick");
        assert!(!grounded);
        assert!(risen.y > ground, "first jump tick must rise");

        let mut landed = risen;
        let mut on_ground = false;
        for _ in 0..100 {
            let (position, grounded) = scene
                .step_walker([0.0, 0.0, 0.0], false, 0.05)
                .expect("fall");
            landed = position;
            on_ground = grounded;
            if grounded {
                break;
            }
        }
        assert!(on_ground, "must land again");
        assert!((landed.y - ground).abs() < 0.5, "back at {landed:?}");
    }
}
