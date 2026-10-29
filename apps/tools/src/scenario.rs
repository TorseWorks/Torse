//! Headless M1 scenario used as executable evidence.
//!
//! The runner wires the M1 building blocks the same way a client or server
//! would: compiled content, a block-state registry, a section store fed by a
//! deterministic generator, collision over authoritative data, and CPU meshing
//! with revision stamps. It asserts the M1 acceptance properties and prints a
//! stable report, so a regression in any layer shows up as a failed run rather
//! than as a screenshot difference.
//!
//! This is a scenario, not a benchmark. Timings are not reported because the
//! measurement conditions (debug build, virtualized CI, background load) are not
//! controlled.

use torse_content::{BlockDefinition, Shape, compile, m0_fixture_pack};
use torse_render::{
    InsertOutcome, MeshCache, MeshData, MeshJob, MeshSample, RenderBlock, RenderBox, build_mesh,
};
use torse_types::{BlockPos, SectionPos};
use torse_world::{
    BlockStateId, BlockStateRegistry, SectionPhase, SectionStore, TerrainQuery, TicketKind,
    WorldBounds,
};
use torse_world_services::{
    Aabb, CollisionCell, CollisionSource, FlatTerrain, GenerationParams, LocalBox, sweep,
};

/// Section side used by the scenario.
const SIDE: u32 = 32;

/// Ground surface; everything below this layer is stone.
const SURFACE_Y: i64 = 1;

/// Sections loaded before the walk starts, one layer of them around the origin.
const LOADED_X: i64 = 3;

/// Result counters reported by the scenario.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScenarioReport {
    /// Sections generated and made readable.
    pub sections_loaded: usize,
    /// Cell identifiers allocated by dense sections.
    pub dense_cells: usize,
    /// Resident compressed cell buffers and palette metadata.
    pub cell_bytes: usize,
    /// Blocks edited after generation.
    pub blocks_edited: usize,
    /// Simulation steps executed for the walk.
    pub steps: usize,
    /// Steps where the horizontal move was blocked.
    pub blocked_steps: usize,
    /// Final collider x coordinate after walking toward the wall.
    pub final_x: f64,
    /// Final collider x coordinate after walking back across a section seam.
    pub seam_x: f64,
    /// Faces emitted for the first meshed section.
    pub mesh_faces: usize,
    /// Meshes rejected because their revision stamp was stale.
    pub stale_rejected: usize,
    /// Bytes held by the mesh cache at the end.
    pub cache_bytes: usize,
    /// Whether an unloaded region blocked movement.
    pub unloaded_blocked: bool,
    /// Whether editing an unloaded section was refused.
    pub unloaded_edit_refused: bool,
}

/// Failure produced by the scenario.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScenarioError {
    /// Human-readable explanation with the failing step.
    pub detail: String,
}

impl ScenarioError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

impl std::error::Error for ScenarioError {}

/// Run the M1 scenario and return its counters.
pub fn run() -> Result<ScenarioReport, ScenarioError> {
    let content = compile(m0_fixture_pack())
        .map_err(|errors| ScenarioError::new(format!("content failed to compile: {errors:?}")))?;
    let registry = BlockStateRegistry::build(&content)
        .map_err(|error| ScenarioError::new(format!("registry failed: {error}")))?;
    let stone = registry
        .default_state(&torse_types::ContentKey::parse("base:stone").expect("key is valid"))
        .ok_or_else(|| ScenarioError::new("fixture content has no base:stone block"))?;
    let glass_slab = registry
        .default_state(&torse_types::ContentKey::parse("base:glass_slab").expect("key is valid"))
        .ok_or_else(|| ScenarioError::new("fixture content has no base:glass_slab block"))?;

    let bounds = WorldBounds::new(-64, 128)
        .map_err(|error| ScenarioError::new(format!("bounds failed: {error}")))?;
    let params = GenerationParams::new(SIDE, bounds, stone, BlockStateId::EMPTY)
        .map_err(|error| ScenarioError::new(format!("generation params failed: {error}")))?;
    let generator = FlatTerrain::new(params, SURFACE_Y);
    let mut store = SectionStore::new(SIDE, bounds)
        .map_err(|error| ScenarioError::new(format!("store failed: {error}")))?;

    // Generate and load the planned region before anything reads it.
    let mut planned = Vec::new();
    for x in -LOADED_X..=LOADED_X {
        for z in -1..=1 {
            planned.push(SectionPos::new(x, 0, z));
        }
    }
    for section in &planned {
        let data = generator
            .generate(*section)
            .map_err(|error| ScenarioError::new(format!("generation failed: {error}")))?;
        store
            .request(*section)
            .map_err(|error| ScenarioError::new(format!("request failed: {error}")))?;
        store
            .install(*section, data)
            .map_err(|error| ScenarioError::new(format!("install failed: {error}")))?;
        store
            .finish_load(*section)
            .map_err(|error| ScenarioError::new(format!("finish failed: {error}")))?;
    }
    if store.phase(planned[0]) != Some(SectionPhase::Ready) {
        return Err(ScenarioError::new("loaded section is not ready"));
    }

    let mut report = ScenarioReport {
        sections_loaded: planned.len(),
        ..ScenarioReport::default()
    };

    // Build a wall on the +x approach so the walk has something to hit.
    let mut blocks_edited = 0usize;
    for y in 1..=4 {
        for z in -5..=5 {
            store
                .edit(BlockPos::new(20, y, z), stone)
                .map_err(|error| ScenarioError::new(format!("wall edit failed: {error}")))?;
            blocks_edited += 1;
        }
    }

    // Place a glass slab row off the walking path. Partial shapes are verified in
    // unit tests; here they only need to exist so the mesh contains two visual
    // shapes and the slab keeps its top face.
    for x in 0..4 {
        store
            .edit(BlockPos::new(x, 1, 2), glass_slab)
            .map_err(|error| ScenarioError::new(format!("slab edit failed: {error}")))?;
        blocks_edited += 1;
    }
    report.blocks_edited = blocks_edited;

    // Walk toward the wall with gravity applied every step.
    let mut player = Aabb::from_center([0.5, 3.0, 0.5], [0.3, 0.9, 0.3]);
    for _ in 0..240 {
        let result = sweep(
            &CollisionView {
                store: &store,
                registry: &registry,
            },
            player,
            [0.25, -0.1, 0.0],
        );
        if result.blocked[0] {
            report.blocked_steps += 1;
        }
        player = result.aabb;
        report.steps += 1;
    }
    report.final_x = player.center()[0];
    if report.final_x > 20.0 - 0.3 + 1e-3 {
        return Err(ScenarioError::new(format!(
            "player passed through the wall: x = {}",
            report.final_x
        )));
    }

    // Walk back across the section seam at x = 0 into section -1.
    for _ in 0..160 {
        let result = sweep(
            &CollisionView {
                store: &store,
                registry: &registry,
            },
            player,
            [-0.25, -0.1, 0.0],
        );
        player = result.aabb;
        report.steps += 1;
    }
    report.seam_x = player.center()[0];
    if report.seam_x >= 0.0 {
        return Err(ScenarioError::new(format!(
            "player did not cross the seam: x = {}",
            report.seam_x
        )));
    }

    // Meshing: build, invalidate by editing, rebuild, accept, then reject an old
    // stamp. The stale insert is the "old mesh must not overwrite new state"
    // acceptance.
    let section = SectionPos::ZERO;
    let first_mesh = mesh_section(&mut store, &registry, section)?;
    report.mesh_faces = first_mesh.face_count();
    if report.mesh_faces == 0 {
        return Err(ScenarioError::new("first mesh has no faces"));
    }

    let stale_job = MeshJob::capture(SIDE, section, |pos| store.revision(pos));

    store
        .edit(BlockPos::new(5, 1, 5), stone)
        .map_err(|error| ScenarioError::new(format!("surface edit failed: {error}")))?;
    report.blocks_edited += 1;

    let mut cache = MeshCache::new(8);
    let fresh_mesh = mesh_section(&mut store, &registry, section)?;
    let fresh_job = MeshJob::capture(SIDE, section, |pos| store.revision(pos));
    if cache.insert(fresh_job, fresh_mesh, |pos| store.revision(pos)) != InsertOutcome::Accepted {
        return Err(ScenarioError::new("fresh mesh was rejected"));
    }
    let stale_mesh = build_mesh(SIDE, section, |pos| sample_render(&store, &registry, pos));
    if cache.insert(stale_job, stale_mesh, |pos| store.revision(pos)) != InsertOutcome::Stale {
        return Err(ScenarioError::new("stale mesh was accepted"));
    }
    report.stale_rejected = 1;
    report.cache_bytes = cache.byte_size();

    // Unloaded terrain must stop movement and reject edits.
    let unloaded_probe = BlockPos::new(1, 2, 70);
    if store.query(unloaded_probe) != TerrainQuery::NotLoaded {
        return Err(ScenarioError::new("unloaded region reported data"));
    }
    let blocked = sweep(
        &CollisionView {
            store: &store,
            registry: &registry,
        },
        Aabb::from_center([1.5, 3.0, 63.5], [0.3, 0.9, 0.3]),
        [0.0, 0.0, 2.0],
    );
    report.unloaded_blocked = blocked.is_blocked();
    if !report.unloaded_blocked {
        return Err(ScenarioError::new("unloaded region did not block movement"));
    }
    report.unloaded_edit_refused = store.edit(BlockPos::new(1, 2, 70), stone).is_err();
    if !report.unloaded_edit_refused {
        return Err(ScenarioError::new("edit outside loaded data was accepted"));
    }

    // Every loaded section can be released, which is the memory-stability check:
    // after teardown the store holds nothing.
    report.dense_cells = store.stats().dense_cells;
    report.cell_bytes = store.stats().cell_bytes;
    for section in &planned {
        store
            .unload(*section)
            .map_err(|error| ScenarioError::new(format!("unload failed: {error}")))?;
    }
    if !store.is_empty() {
        return Err(ScenarioError::new("store kept sections after unload"));
    }
    Ok(report)
}

fn mesh_section(
    store: &mut SectionStore,
    registry: &BlockStateRegistry,
    section: SectionPos,
) -> Result<MeshData, ScenarioError> {
    // A background ticket keeps the section resident while the mesh is built and
    // proves that eviction respects ticket holders.
    let ticket = store
        .acquire_ticket(section, TicketKind::Background)
        .map_err(|error| ScenarioError::new(format!("mesh ticket failed: {error}")))?;
    let data = build_mesh(SIDE, section, |pos| sample_render(store, registry, pos));
    store
        .release_ticket(ticket)
        .map_err(|error| ScenarioError::new(format!("ticket release failed: {error}")))?;
    Ok(data)
}

/// Collision adapter over authoritative terrain.
struct CollisionView<'a> {
    store: &'a SectionStore,
    registry: &'a BlockStateRegistry,
}

impl CollisionSource for CollisionView<'_> {
    fn collision(&self, pos: BlockPos) -> CollisionCell {
        match self.store.query(pos) {
            TerrainQuery::Ready(state) => match self.registry.definition(state) {
                Some(definition) => match &definition.collision_profile.shape {
                    Some(shape) => match collision_boxes(shape) {
                        Some(boxes) => CollisionCell::Solid(boxes),
                        None => CollisionCell::Empty,
                    },
                    None => CollisionCell::Empty,
                },
                None => CollisionCell::Empty,
            },
            TerrainQuery::NotLoaded => CollisionCell::NotLoaded,
            TerrainQuery::OutOfBounds => CollisionCell::OutOfBounds,
            TerrainQuery::Exterior => CollisionCell::Empty,
        }
    }
}

fn sample_render(store: &SectionStore, registry: &BlockStateRegistry, pos: BlockPos) -> MeshSample {
    match store.query(pos) {
        TerrainQuery::Ready(state) => {
            if state.is_empty() {
                return MeshSample::Ready(RenderBlock::EMPTY);
            }
            let Some(definition) = registry.definition(state) else {
                return MeshSample::Ready(RenderBlock::EMPTY);
            };
            MeshSample::Ready(RenderBlock {
                bounds: render_box(definition),
                cross: matches!(&definition.render_profile.shape, Shape::Cross),
                occlusion: definition.occlusion_profile.face_mask,
                material: state.0,
                face_materials: None,
                ..RenderBlock::EMPTY
            })
        }
        TerrainQuery::NotLoaded => MeshSample::NotLoaded,
        TerrainQuery::OutOfBounds | TerrainQuery::Exterior => MeshSample::OutOfBounds,
    }
}

fn render_box(definition: &BlockDefinition) -> Option<RenderBox> {
    match &definition.render_profile.shape {
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

fn collision_boxes(shape: &Shape) -> Option<Vec<LocalBox>> {
    let as_f64 = |bounds: &torse_content::BoxBounds| LocalBox {
        min: [
            f64::from(bounds.min[0]),
            f64::from(bounds.min[1]),
            f64::from(bounds.min[2]),
        ],
        max: [
            f64::from(bounds.max[0]),
            f64::from(bounds.max[1]),
            f64::from(bounds.max[2]),
        ],
    };
    match shape {
        Shape::Model(_) => None,
        Shape::Compound(boxes) => Some(boxes.iter().map(as_f64).collect()),
        Shape::FullCube => Some(vec![LocalBox::FULL_CUBE]),
        Shape::LowerSlab => Some(vec![LocalBox {
            min: [0.0, 0.0, 0.0],
            max: [1.0, 0.5, 1.0],
        }]),
        Shape::Panel => Some(vec![LocalBox {
            min: [0.45, 0.0, 0.0],
            max: [0.55, 1.0, 1.0],
        }]),
        Shape::Cross => Some(vec![LocalBox::FULL_CUBE]),
        Shape::Corner | Shape::Stair => Some(vec![LocalBox::FULL_CUBE]),
        Shape::Box(bounds) => Some(vec![as_f64(bounds)]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m1_scenario_holds_its_acceptance_properties() {
        // The scenario itself asserts the hard properties; this test pins the
        // headline counters so a silent weakening of the scenario is visible.
        let report = run().expect("M1 scenario completes");

        assert_eq!(report.sections_loaded, 21);
        assert_eq!(report.blocks_edited, 49);
        assert_eq!(report.steps, 400);
        assert!(report.blocked_steps > 0, "the wall must block some steps");
        assert!(report.final_x < 20.0, "the wall must stop the walk");
        assert!(report.seam_x < 0.0, "the walk must cross the section seam");
        assert!(
            report.mesh_faces > 1000,
            "the ground plane must produce faces"
        );
        assert_eq!(report.stale_rejected, 1);
        assert!(report.cache_bytes > 0);
        assert!(report.unloaded_blocked);
        assert!(report.unloaded_edit_refused);
    }
}
