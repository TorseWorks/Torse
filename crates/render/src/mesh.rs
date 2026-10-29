//! CPU section meshing with revision-checked dependency stamps.
//!
//! Meshing reads authoritative terrain but never writes it. The mesher takes a
//! sampling closure instead of the world type, so this crate has no dependency on
//! `torse-world`: the application adapts a terrain view to [`MeshSample`], and
//! tests can feed synthetic data.
//!
//! Two rules come straight from the architecture documents:
//!
//! - A face is culled only when the neighbor's occlusion mask claims the face and
//!   the block actually touches that cell boundary. Conservative defaults must
//!   leave extra faces visible rather than delete a visible surface.
//! - Touching faces of one material form a single surface: a face whose
//!   cross-section fits inside the neighbor's same-material opposite face is
//!   dropped, and equal sections drop both sides. This is the explicit
//!   contact rule that removes coincident panes between glass neighbours.
//! - A full cube that occludes every face is marked in a per-row bitset. A face
//!   between two such cubes is omitted without sampling the neighbour again.
//!   Slabs, crossed shapes, partial occlusion, and missing samples stay on the
//!   per-face path. Budgeted callers reserve the temporary bitset in the shared
//!   mesh class and release it when construction ends.
//! - A mesh computed from older data must never replace a newer one. [`MeshJob`]
//!   records the section revision and the six neighbor revisions, and
//!   [`MeshCache::insert`] rejects results whose stamp no longer matches.
//!
//! Vertices are section-local. Moving a mesh to world space is the renderer's
//! job, which keeps a mesh valid as long as its own section does not change.

use std::collections::BTreeMap;

mod cubes;

use torse_types::{
    BlockPos, EvictionRank, Fault, MemoryChargeId, MemoryClass, MemoryLedger, MemoryResident,
    Revision, SectionPos,
};

/// The six cell faces in occlusion-mask order: `-x, +x, -y, +y, -z, +z`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Face {
    /// Negative x.
    NegX,
    /// Positive x.
    PosX,
    /// Negative y.
    NegY,
    /// Positive y.
    PosY,
    /// Negative z.
    NegZ,
    /// Positive z.
    PosZ,
}

impl Face {
    /// All faces in occlusion-mask order.
    pub const ALL: [Self; 6] = [
        Self::NegX,
        Self::PosX,
        Self::NegY,
        Self::PosY,
        Self::NegZ,
        Self::PosZ,
    ];

    /// Position of this face in a mask.
    pub const fn index(self) -> u32 {
        match self {
            Self::NegX => 0,
            Self::PosX => 1,
            Self::NegY => 2,
            Self::PosY => 3,
            Self::NegZ => 4,
            Self::PosZ => 5,
        }
    }

    /// Mask bit used by [`RenderBlock::occlusion`].
    pub const fn bit(self) -> u8 {
        1u8 << self.index()
    }

    /// The face on the opposite side of the cell.
    pub const fn opposite(self) -> Self {
        match self {
            Self::NegX => Self::PosX,
            Self::PosX => Self::NegX,
            Self::NegY => Self::PosY,
            Self::PosY => Self::NegY,
            Self::NegZ => Self::PosZ,
            Self::PosZ => Self::NegZ,
        }
    }

    /// Outward normal.
    pub const fn normal(self) -> [f32; 3] {
        match self {
            Self::NegX => [-1.0, 0.0, 0.0],
            Self::PosX => [1.0, 0.0, 0.0],
            Self::NegY => [0.0, -1.0, 0.0],
            Self::PosY => [0.0, 1.0, 0.0],
            Self::NegZ => [0.0, 0.0, -1.0],
            Self::PosZ => [0.0, 0.0, 1.0],
        }
    }

    /// Offset from the cell to the neighbor that would cover this face.
    pub const fn offset(self) -> (i64, i64, i64) {
        match self {
            Self::NegX => (-1, 0, 0),
            Self::PosX => (1, 0, 0),
            Self::NegY => (0, -1, 0),
            Self::PosY => (0, 1, 0),
            Self::NegZ => (0, 0, -1),
            Self::PosZ => (0, 0, 1),
        }
    }

    /// Neighbor section, or None when the coordinate cannot be represented.
    pub fn neighbor_section(self, section: SectionPos) -> Option<SectionPos> {
        let (x, y, z) = self.offset();
        Some(SectionPos::new(
            section.x.checked_add(x)?,
            section.y.checked_add(y)?,
            section.z.checked_add(z)?,
        ))
    }

    /// Whether this is a negative-axis face.
    const fn is_negative(self) -> bool {
        matches!(self, Self::NegX | Self::NegY | Self::NegZ)
    }

    /// Axis touched by this face.
    const fn axis(self) -> usize {
        match self {
            Self::NegX | Self::PosX => 0,
            Self::NegY | Self::PosY => 1,
            Self::NegZ | Self::PosZ => 2,
        }
    }
}

/// Axis-aligned visual bounds in the unit cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderBox {
    /// Lower corner components, each in `0.0..=1.0`.
    pub min: [f32; 3],
    /// Upper corner components, each in `0.0..=1.0`.
    pub max: [f32; 3],
}

impl RenderBox {
    /// Bounds covering the entire cell.
    pub const FULL_CUBE: Self = Self {
        min: [0.0, 0.0, 0.0],
        max: [1.0, 1.0, 1.0],
    };

    /// Return whether the box reaches the cell boundary on a face.
    fn touches(self, face: Face) -> bool {
        let axis = face.axis();
        if face.is_negative() {
            self.min[axis] <= 0.0
        } else {
            self.max[axis] >= 1.0
        }
    }
}

/// Visual description of one loaded cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderBlock {
    /// Geometry to emit, or `None` for a block with no visual.
    pub bounds: Option<RenderBox>,
    /// Emit two crossed, double-sided quads instead of the box bounds.
    pub cross: bool,
    /// Faces this block covers for neighbors, in mask order.
    pub occlusion: u8,
    /// Material identifier for faces without a per-face override.
    pub material: u16,
    /// Materials for the six faces in mask order; `None` uses `material`.
    pub face_materials: Option<[u16; 6]>,
    /// Optional overlay material per face; `u16::MAX` is never a valid slot.
    pub face_overlays: Option<[Option<u16>; 6]>,
    /// Linear tint per base face; `None` uses white.
    pub face_tints: Option<[[f32; 3]; 6]>,
    /// Linear tint per overlay face; `None` uses white.
    pub face_overlay_tints: Option<[[f32; 3]; 6]>,
    /// Resolved per-corner brightness in face-quad order, 0..=255.
    /// None retains the consumer's directional shading policy.
    pub face_shades: Option<[[u8; 4]; 6]>,
}

impl RenderBlock {
    /// Block with no visual; used for empty cells.
    pub const EMPTY: Self = Self {
        bounds: None,
        cross: false,
        occlusion: 0,
        material: 0,
        face_materials: None,
        face_overlays: None,
        face_tints: None,
        face_overlay_tints: None,
        face_shades: None,
    };

    /// Full cube using one material on every face.
    pub const fn uniform(material: u16) -> Self {
        Self {
            bounds: Some(RenderBox::FULL_CUBE),
            cross: false,
            occlusion: 0b0011_1111,
            material,
            face_materials: None,
            face_overlays: None,
            face_tints: None,
            face_overlay_tints: None,
            face_shades: None,
        }
    }

    /// Material of one face.
    pub fn material_for(&self, face: Face) -> u16 {
        match &self.face_materials {
            Some(materials) => materials[face.index() as usize],
            None => self.material,
        }
    }

    /// Overlay material of one face, if declared.
    pub fn overlay_for(&self, face: Face) -> Option<u16> {
        self.face_overlays
            .as_ref()
            .and_then(|materials| materials[face.index() as usize])
    }

    /// Base tint of one face, defaulting to white.
    pub fn tint_for(&self, face: Face) -> [f32; 3] {
        self.face_tints
            .as_ref()
            .map_or([1.0; 3], |tints| tints[face.index() as usize])
    }

    /// Overlay tint of one face, defaulting to white.
    pub fn overlay_tint_for(&self, face: Face) -> [f32; 3] {
        self.face_overlay_tints
            .as_ref()
            .map_or([1.0; 3], |tints| tints[face.index() as usize])
    }
}

/// Result of sampling one cell.
#[derive(Clone, Copy, Debug, PartialEq)]
// Sampling returns inline, copyable data in the meshing loop without a heap allocation.
#[allow(clippy::large_enum_variant)]
pub enum MeshSample {
    /// The cell has authoritative data.
    Ready(RenderBlock),
    /// The section is not loaded; faces toward it stay visible.
    NotLoaded,
    /// The cell is outside the world; faces toward it stay visible.
    OutOfBounds,
}

/// Identifiers and geometry produced for one section.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MeshData {
    /// Section this mesh belongs to.
    pub section: SectionPos,
    /// Vertex positions in section-local coordinates.
    pub positions: Vec<[f32; 3]>,
    /// One normal per vertex.
    pub normals: Vec<[f32; 3]>,
    /// One texture coordinate per vertex, local to the face.
    pub uvs: Vec<[f32; 2]>,
    /// Triangle indices.
    pub indices: Vec<u32>,
    /// Material per emitted quad.
    pub materials: Vec<u16>,
    /// Overlay material per emitted quad, with `u16::MAX` for no overlay.
    pub overlays: Vec<u16>,
    /// Base vertex tint per emitted vertex.
    pub tints: Vec<[f32; 3]>,
    /// Overlay vertex tint per emitted vertex.
    pub overlay_tints: Vec<[f32; 3]>,
    /// Negative selects directional shading; 0..=1 is resolved vertex brightness.
    pub shades: Vec<f32>,
}

impl MeshData {
    /// Number of emitted quads.
    pub fn face_count(&self) -> usize {
        self.materials.len()
    }

    /// Number of triangles.
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    /// Return whether nothing was emitted.
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// Approximate CPU bytes held by this mesh.
    pub fn byte_size(&self) -> usize {
        self.positions.len() * size_of::<[f32; 3]>()
            + self.normals.len() * size_of::<[f32; 3]>()
            + self.uvs.len() * size_of::<[f32; 2]>()
            + self.indices.len() * size_of::<u32>()
            + self.materials.len() * size_of::<u16>()
            + self.overlays.len() * size_of::<u16>()
            + self.tints.len() * size_of::<[f32; 3]>()
            + self.overlay_tints.len() * size_of::<[f32; 3]>()
            + self.shades.len() * size_of::<f32>()
    }
}

impl MeshData {
    /// Reserved heap capacity; use this when charging owned background output.
    pub fn allocated_bytes(&self) -> usize {
        self.positions.capacity() * size_of::<[f32; 3]>()
            + self.normals.capacity() * size_of::<[f32; 3]>()
            + self.uvs.capacity() * size_of::<[f32; 2]>()
            + self.indices.capacity() * size_of::<u32>()
            + self.materials.capacity() * size_of::<u16>()
            + self.overlays.capacity() * size_of::<u16>()
            + self.tints.capacity() * size_of::<[f32; 3]>()
            + self.overlay_tints.capacity() * size_of::<[f32; 3]>()
            + self.shades.capacity() * size_of::<f32>()
    }
}

/// Build a section mesh.
///
/// Sides must be in `1..=64`, matching authoritative section storage. Invalid
/// sides or unrepresentable block extents return an empty mesh without sampling.
/// Use a bounded entry point to distinguish invalid input from an empty section.
/// `sample` must describe one immutable view throughout the build. Full-cube
/// occupancy is sampled first; visible cells and slow-path neighbours may be
/// sampled again. Callers with a shared budget must reserve
/// [`occupancy_scratch_bytes`] before starting and hold it until return.
pub fn build_mesh(
    side: u32,
    section: SectionPos,
    sample: impl Fn(BlockPos) -> MeshSample,
) -> MeshData {
    build_mesh_inner(side, section, sample, || false, None, None, MeshMode::Faces).unwrap_or_else(
        |_| MeshData {
            section,
            ..MeshData::default()
        },
    )
}

/// Unoptimized semantic reference, useful for geometry and CPU comparisons.
pub fn build_mesh_reference(
    side: u32,
    section: SectionPos,
    sample: impl Fn(BlockPos) -> MeshSample,
) -> MeshData {
    build_mesh_inner(
        side,
        section,
        sample,
        || false,
        None,
        None,
        MeshMode::Reference,
    )
    .unwrap_or_else(|_| MeshData {
        section,
        ..MeshData::default()
    })
}

/// Synchronous counterpart of the bounded occupancy/run mesher.
pub fn build_mesh_optimized(
    side: u32,
    section: SectionPos,
    sample: impl Fn(BlockPos) -> MeshSample,
) -> MeshData {
    build_mesh_inner(side, section, sample, || false, None, None, MeshMode::Runs).unwrap_or_else(
        |_| MeshData {
            section,
            ..MeshData::default()
        },
    )
}

/// Mesh one section and charge the occupancy table to `ledger` until it returns.
pub fn build_mesh_accounted(
    side: u32,
    section: SectionPos,
    sample: impl Fn(BlockPos) -> MeshSample,
    ledger: &mut MemoryLedger,
) -> Result<MeshData, MeshBuildError> {
    build_mesh_inner(
        side,
        section,
        sample,
        || false,
        None,
        Some(ledger),
        MeshMode::Faces,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MeshBuildError {
    Cancelled,
    OutputLimit,
    /// The occupancy table did not fit in the mesh-class budget.
    Memory,
    /// Side is outside `1..=64`, or the section's block extent overflows i64.
    InvalidInput,
}

/// Bound face output before allocation and check cancellation at each y slice.
/// The working reservation must also cover [`occupancy_scratch_bytes`]; shared
/// budget adapters hold a mesh-class scratch lease for the queued/running job.
/// Input is owned by the caller; output allocation is at most twice the stated
/// face payload, allowing final compaction while the original buffers exist.
pub fn build_mesh_bounded(
    side: u32,
    section: SectionPos,
    sample: impl Fn(BlockPos) -> MeshSample,
    cancelled: impl Fn() -> bool,
    max_faces: usize,
) -> Result<MeshData, MeshBuildError> {
    if max_faces > u32::MAX as usize / 4 || max_faces > usize::MAX / 6 {
        return Err(MeshBuildError::OutputLimit);
    }
    build_mesh_inner(
        side,
        section,
        sample,
        cancelled,
        Some(max_faces),
        None,
        MeshMode::Faces,
    )
}

/// Merge coplanar opaque cube faces into rectangles with identical materials and tints.
/// Missing borders, shaped blocks and overlays retain ordinary face semantics.
/// Reserve occupancy_scratch_bytes in addition to bounded output.
/// UVs repeat per cell; consumers must support tiled local face coordinates.
pub fn build_mesh_optimized_bounded(
    side: u32,
    section: SectionPos,
    sample: impl Fn(BlockPos) -> MeshSample,
    cancelled: impl Fn() -> bool,
    max_faces: usize,
) -> Result<MeshData, MeshBuildError> {
    if max_faces > u32::MAX as usize / 4 || max_faces > usize::MAX / 6 {
        return Err(MeshBuildError::OutputLimit);
    }
    build_mesh_inner(
        side,
        section,
        sample,
        cancelled,
        Some(max_faces),
        None,
        MeshMode::Runs,
    )
}

#[derive(Clone, Copy, PartialEq)]
enum MeshMode {
    Reference,
    Faces,
    Runs,
}

fn checked_origin(side: u32, section: SectionPos) -> Option<[i64; 3]> {
    if !(1..=64).contains(&side) {
        return None;
    }
    let mut origin = [0; 3];
    for (out, coordinate) in origin.iter_mut().zip([section.x, section.y, section.z]) {
        *out = coordinate.checked_mul(i64::from(side))?;
        out.checked_add(i64::from(side) - 1)?;
    }
    Some(origin)
}

fn build_mesh_inner(
    side: u32,
    section: SectionPos,
    sample: impl Fn(BlockPos) -> MeshSample,
    cancelled: impl Fn() -> bool,
    max_faces: Option<usize>,
    ledger: Option<&mut MemoryLedger>,
    mode: MeshMode,
) -> Result<MeshData, MeshBuildError> {
    if cancelled() {
        return Err(MeshBuildError::Cancelled);
    }
    let origin = checked_origin(side, section).ok_or(MeshBuildError::InvalidInput)?;
    let mut mesh = MeshData {
        section,
        ..MeshData::default()
    };

    let use_occupancy = mode != MeshMode::Reference;
    let _occupancy_charge = if use_occupancy {
        charge_occupancy(side, ledger)?
    } else {
        None
    };
    let occupancy = if use_occupancy {
        RowOccupancy::build(side, &origin, &sample, &cancelled)?
    } else {
        RowOccupancy::Absent
    };

    if let Some(faces) = max_faces {
        mesh.positions
            .try_reserve_exact(faces * 4)
            .map_err(|_| MeshBuildError::OutputLimit)?;
        mesh.normals
            .try_reserve_exact(faces * 4)
            .map_err(|_| MeshBuildError::OutputLimit)?;
        mesh.uvs
            .try_reserve_exact(faces * 4)
            .map_err(|_| MeshBuildError::OutputLimit)?;
        mesh.indices
            .try_reserve_exact(faces * 6)
            .map_err(|_| MeshBuildError::OutputLimit)?;
        mesh.materials
            .try_reserve_exact(faces)
            .map_err(|_| MeshBuildError::OutputLimit)?;
        mesh.overlays
            .try_reserve_exact(faces)
            .map_err(|_| MeshBuildError::OutputLimit)?;
        mesh.tints
            .try_reserve_exact(faces * 4)
            .map_err(|_| MeshBuildError::OutputLimit)?;
        mesh.overlay_tints
            .try_reserve_exact(faces * 4)
            .map_err(|_| MeshBuildError::OutputLimit)?;
        mesh.shades
            .try_reserve_exact(faces * 4)
            .map_err(|_| MeshBuildError::OutputLimit)?;
    }

    let optimized = mode == MeshMode::Runs && side <= 64;
    if optimized {
        cubes::emit(
            &mut mesh, &occupancy, side, origin, &sample, &cancelled, max_faces,
        )?;
    }
    for y in 0..side {
        if cancelled() {
            return Err(MeshBuildError::Cancelled);
        }
        for z in 0..side {
            if !occupancy.has_visuals(y, z) {
                continue;
            }
            let coverage = occupancy.covered_faces(y, z);
            for x in RowCells::new(side, coverage.as_ref()) {
                let pos = BlockPos::new(
                    origin[0] + i64::from(x),
                    origin[1] + i64::from(y),
                    origin[2] + i64::from(z),
                );
                let block = match sample(pos) {
                    MeshSample::Ready(block) => block,
                    MeshSample::NotLoaded | MeshSample::OutOfBounds => continue,
                };
                if optimized && cubes::eligible(block) {
                    continue;
                }
                if block.cross {
                    if max_faces.is_some_and(|limit| mesh.face_count() + 2 > limit) {
                        return Err(MeshBuildError::OutputLimit);
                    }
                    emit_cross(&mut mesh, &block, [x as f32, y as f32, z as f32]);
                    continue;
                }
                let Some(bounds) = block.bounds else {
                    continue;
                };
                for face in Face::ALL {
                    if coverage
                        .as_ref()
                        .is_some_and(|masks| masks[face.index() as usize] & (1u64 << x) != 0)
                        || face_culled(face, &block, &sample, pos)
                    {
                        continue;
                    }
                    if max_faces.is_some_and(|limit| mesh.face_count() >= limit) {
                        return Err(MeshBuildError::OutputLimit);
                    }
                    emit_face(
                        &mut mesh,
                        face,
                        bounds,
                        [x as f32, y as f32, z as f32],
                        FaceAppearance::new(&block, face),
                    );
                }
            }
        }
    }
    if max_faces.is_some() {
        mesh.positions.shrink_to_fit();
        mesh.normals.shrink_to_fit();
        mesh.uvs.shrink_to_fit();
        mesh.indices.shrink_to_fit();
        mesh.materials.shrink_to_fit();
        mesh.overlays.shrink_to_fit();
        mesh.tints.shrink_to_fit();
        mesh.overlay_tints.shrink_to_fit();
        mesh.shades.shrink_to_fit();
    }
    Ok(mesh)
}

/// Temporary meshing-work payload, excluding stack metadata.
///
/// Budgeted callers reserve these bytes in their shared mesh class before
/// copying mesh input or starting work, and keep the lease until work ends.
/// The optimized path also retains one plane of face appearances and its used
/// bits while it coalesces rectangles. Sides outside `1..=64` use the
/// reference path without these allocations.
pub fn occupancy_scratch_bytes(side: u32) -> u64 {
    if side == 0 || side > 64 {
        return 0;
    }
    let word = if side <= 32 { 4 } else { 8 };
    let rows = u64::from(side) * u64::from(side);
    let occupancy = rows * word + rows.div_ceil(64) * 8;
    let rectangle_work = rows * (size_of::<Option<FaceAppearance>>() + size_of::<bool>()) as u64;
    occupancy.saturating_add(rectangle_work)
}

struct OccupancyCharge<'a> {
    ledger: &'a mut MemoryLedger,
    id: MemoryChargeId,
}

impl Drop for OccupancyCharge<'_> {
    fn drop(&mut self) {
        let _ = self.ledger.release(self.id);
    }
}

fn charge_occupancy<'a>(
    side: u32,
    ledger: Option<&'a mut MemoryLedger>,
) -> Result<Option<OccupancyCharge<'a>>, MeshBuildError> {
    let bytes = occupancy_scratch_bytes(side);
    let Some(ledger) = ledger else {
        return Ok(None);
    };
    if bytes == 0 {
        return Ok(None);
    }
    let id = ledger
        .charge(MemoryResident {
            class: MemoryClass::Mesh,
            bytes,
            rank: EvictionRank::Mesh { distance_sq: 0 },
        })
        .map_err(|_| MeshBuildError::Memory)?;
    Ok(Some(OccupancyCharge { ledger, id }))
}

/// One bit per cell in an x-row. Side 32 uses `u32`; side 64 uses `u64`.
enum RowOccupancy {
    Bits32 {
        side: u32,
        rows: Vec<u32>,
        visual_rows: Vec<u64>,
    },
    Bits64 {
        side: u32,
        rows: Vec<u64>,
        visual_rows: Vec<u64>,
    },
    Absent,
}

impl RowOccupancy {
    fn build(
        side: u32,
        origin: &[i64; 3],
        sample: &impl Fn(BlockPos) -> MeshSample,
        cancelled: &impl Fn() -> bool,
    ) -> Result<Self, MeshBuildError> {
        if side == 0 || side > 64 {
            return Ok(Self::Absent);
        }
        let count = (side * side) as usize;
        let mut visual_rows = vec![0u64; count.div_ceil(64)];
        if side <= 32 {
            let mut rows = vec![0u32; count];
            fill_rows(side, origin, sample, cancelled, |x, y, z, full| {
                let row = (y * side + z) as usize;
                visual_rows[row / 64] |= 1u64 << (row % 64);
                if full {
                    rows[row] |= 1u32 << x;
                }
            })?;
            Ok(Self::Bits32 {
                side,
                rows,
                visual_rows,
            })
        } else {
            let mut rows = vec![0u64; count];
            fill_rows(side, origin, sample, cancelled, |x, y, z, full| {
                let row = (y * side + z) as usize;
                visual_rows[row / 64] |= 1u64 << (row % 64);
                if full {
                    rows[row] |= 1u64 << x;
                }
            })?;
            Ok(Self::Bits64 {
                side,
                rows,
                visual_rows,
            })
        }
    }

    fn has_visuals(&self, y: u32, z: u32) -> bool {
        match self {
            Self::Bits32 {
                side, visual_rows, ..
            }
            | Self::Bits64 {
                side, visual_rows, ..
            } => {
                let row = (y * side + z) as usize;
                visual_rows[row / 64] & (1u64 << (row % 64)) != 0
            }
            Self::Absent => true,
        }
    }

    fn row(&self, y: i64, z: i64) -> u64 {
        let Some(side) = self.side() else {
            return 0;
        };
        if y < 0 || z < 0 || y >= i64::from(side) || z >= i64::from(side) {
            return 0;
        }
        let index = (y as u32 * side + z as u32) as usize;
        match self {
            Self::Bits32 { rows, .. } => u64::from(rows[index]),
            Self::Bits64 { rows, .. } => rows[index],
            Self::Absent => 0,
        }
    }

    /// Six row-wide coverage masks. No halo bit is assumed loaded or opaque.
    fn covered_faces(&self, y: u32, z: u32) -> Option<[u64; 6]> {
        self.side()?;
        let (y, z) = (i64::from(y), i64::from(z));
        let row = self.row(y, z);
        Some([
            row & (row << 1),
            row & (row >> 1),
            row & self.row(y - 1, z),
            row & self.row(y + 1, z),
            row & self.row(y, z - 1),
            row & self.row(y, z + 1),
        ])
    }

    fn side(&self) -> Option<u32> {
        match self {
            Self::Bits32 { side, .. } | Self::Bits64 { side, .. } => Some(*side),
            Self::Absent => None,
        }
    }
}

enum RowCells {
    Bits(u64),
    Slow(std::ops::Range<u32>),
}

impl RowCells {
    fn new(side: u32, coverage: Option<&[u64; 6]>) -> Self {
        let Some(coverage) = coverage else {
            return Self::Slow(0..side);
        };
        let mask = if side == 64 {
            u64::MAX
        } else {
            (1u64 << side) - 1
        };
        let buried = coverage.iter().fold(mask, |bits, covered| bits & covered);
        Self::Bits(mask & !buried)
    }
}

impl Iterator for RowCells {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Bits(bits) if *bits == 0 => None,
            Self::Bits(bits) => {
                let x = bits.trailing_zeros();
                *bits &= *bits - 1;
                Some(x)
            }
            Self::Slow(range) => range.next(),
        }
    }
}

fn fill_rows(
    side: u32,
    origin: &[i64; 3],
    sample: &impl Fn(BlockPos) -> MeshSample,
    cancelled: &impl Fn() -> bool,
    mut mark: impl FnMut(u32, u32, u32, bool),
) -> Result<(), MeshBuildError> {
    for y in 0..side {
        if cancelled() {
            return Err(MeshBuildError::Cancelled);
        }
        for z in 0..side {
            for x in 0..side {
                let pos = BlockPos::new(
                    origin[0] + i64::from(x),
                    origin[1] + i64::from(y),
                    origin[2] + i64::from(z),
                );
                if let MeshSample::Ready(block) = sample(pos)
                    && (block.cross || block.bounds.is_some())
                {
                    mark(x, y, z, fast_cube(MeshSample::Ready(block)));
                }
            }
        }
    }
    Ok(())
}

fn fast_cube(sample: MeshSample) -> bool {
    let MeshSample::Ready(block) = sample else {
        return false;
    };
    !block.cross
        && block.bounds == Some(RenderBox::FULL_CUBE)
        && block.occlusion & 0b0011_1111 == 0b0011_1111
}

fn face_culled(
    face: Face,
    block: &RenderBlock,
    sample: &impl Fn(BlockPos) -> MeshSample,
    pos: BlockPos,
) -> bool {
    let Some(bounds) = block.bounds else {
        return true;
    };
    if !bounds.touches(face) {
        // A partial shape that does not reach the boundary can never be hidden
        // by the neighbor, even when the neighbor fully covers the cell.
        return false;
    }
    let (dx, dy, dz) = face.offset();
    let neighbor = pos.checked_add(dx, dy, dz);
    let Ok(neighbor) = neighbor else {
        return false;
    };
    match sample(neighbor) {
        MeshSample::Ready(neighbor_block) => {
            let opposite = face.opposite();
            // Two touching faces of one material describe a single surface.
            // When this side's cross-section fits inside the neighbour's,
            // drop it and let the neighbour's face carry the surface: equal
            // sections cull both sides, which removes the coincident glass
            // panes between same-material neighbours.
            if let Some(neighbor_bounds) = neighbor_block.bounds
                && !block.cross
                && !neighbor_block.cross
                && neighbor_bounds.touches(opposite)
                && neighbor_block.material_for(opposite) == block.material_for(face)
                && section_within(face, bounds, neighbor_bounds)
            {
                return true;
            }
            // The shared plane sits on the neighbour's opposite boundary:
            // a half slab below cannot hide the face above its midline, so
            // the neighbour must claim coverage of that boundary and its
            // shape must actually reach it.
            if neighbor_block.occlusion & opposite.bit() == 0 {
                return false;
            }
            match neighbor_block.bounds {
                Some(nb) => nb.touches(opposite),
                None => true,
            }
        }
        // Missing data must keep the face visible. Treating it as covered would
        // punch holes in the world exactly where the client has no data.
        MeshSample::NotLoaded | MeshSample::OutOfBounds => false,
    }
}

/// Whether this face's cross-section on the shared plane fits inside the
/// neighbour's opposite face, so the neighbour's face can replace it.
fn section_within(face: Face, bounds: RenderBox, neighbor: RenderBox) -> bool {
    let (first, second) = match face.axis() {
        0 => (1, 2),
        1 => (0, 2),
        _ => (0, 1),
    };
    neighbor.min[first] <= bounds.min[first]
        && bounds.max[first] <= neighbor.max[first]
        && neighbor.min[second] <= bounds.min[second]
        && bounds.max[second] <= neighbor.max[second]
}

#[derive(Clone, Copy, PartialEq)]
struct FaceAppearance {
    material: u16,
    overlay: Option<u16>,
    tint: [f32; 3],
    overlay_tint: [f32; 3],
    shades: [f32; 4],
}

impl FaceAppearance {
    fn new(block: &RenderBlock, face: Face) -> Self {
        Self {
            material: block.material_for(face),
            overlay: block.overlay_for(face),
            tint: block.tint_for(face),
            overlay_tint: block.overlay_tint_for(face),
            shades: block.face_shades.map_or([-1.0; 4], |faces| {
                faces[face.index() as usize].map(|v| f32::from(v) / 255.0)
            }),
        }
    }
}

fn emit_face(
    mesh: &mut MeshData,
    face: Face,
    bounds: RenderBox,
    cell: [f32; 3],
    appearance: FaceAppearance,
) {
    let (origin, edge_u, edge_v) = face_quad(face, bounds, cell);
    let normal = face.normal();
    let base = mesh.positions.len() as u32;
    let corners = [
        origin,
        add(origin, edge_u),
        add(add(origin, edge_u), edge_v),
        add(origin, edge_v),
    ];
    // Local face coordinates. The origin corner is the bottom left of the face
    // image, and v grows towards the first row of the texture.
    let uvs = corners.map(|corner| face_uv(face, corner, cell));
    for (corner, uv) in corners.into_iter().zip(uvs) {
        mesh.positions.push(corner);
        mesh.normals.push(normal);
        mesh.uvs.push(uv);
        mesh.tints.push(appearance.tint);
        mesh.overlay_tints.push(appearance.overlay_tint);
    }
    // Two triangles: (0, 1, 2) and (0, 2, 3). The corner order is chosen so the
    // cross product points along the face normal.
    mesh.indices
        .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    mesh.materials.push(appearance.material);
    mesh.overlays.push(appearance.overlay.unwrap_or(u16::MAX));
    mesh.shades.extend(appearance.shades);
}

fn emit_cross(mesh: &mut MeshData, block: &RenderBlock, cell: [f32; 3]) {
    let planes = [
        (
            Face::NegX,
            [
                add(cell, [0.0, 0.0, 0.0]),
                add(cell, [1.0, 0.0, 1.0]),
                add(cell, [1.0, 1.0, 1.0]),
                add(cell, [0.0, 1.0, 0.0]),
            ],
            [-1.0, 0.0, 1.0],
        ),
        (
            Face::PosX,
            [
                add(cell, [1.0, 0.0, 0.0]),
                add(cell, [0.0, 0.0, 1.0]),
                add(cell, [0.0, 1.0, 1.0]),
                add(cell, [1.0, 1.0, 0.0]),
            ],
            [-1.0, 0.0, -1.0],
        ),
    ];
    let uv = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
    for (face, corners, normal) in planes {
        emit_quad(mesh, corners, uv, normal, FaceAppearance::new(block, face));
    }
}

fn emit_quad(
    mesh: &mut MeshData,
    corners: [[f32; 3]; 4],
    uvs: [[f32; 2]; 4],
    normal: [f32; 3],
    appearance: FaceAppearance,
) {
    let base = mesh.positions.len() as u32;
    for (corner, uv) in corners.into_iter().zip(uvs) {
        mesh.positions.push(corner);
        mesh.normals.push(normal);
        mesh.uvs.push(uv);
        mesh.tints.push(appearance.tint);
        mesh.overlay_tints.push(appearance.overlay_tint);
    }
    mesh.indices
        .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    mesh.materials.push(appearance.material);
    mesh.overlays.push(appearance.overlay.unwrap_or(u16::MAX));
    mesh.shades.extend(appearance.shades);
}

/// Resolved visual surface ready for a mesh; no content registry access is needed.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelQuad {
    pub positions: [[f32; 3]; 4],
    pub uvs: [[f32; 2]; 4],
    pub normal: [f32; 3],
    pub material: u16,
    pub overlay: Option<u16>,
    pub tint: [f32; 3],
    pub overlay_tint: [f32; 3],
    pub cull_face: Option<Face>,
    pub shade: bool,
    pub shade_override: Option<f32>,
    pub double_sided: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelMeshError {
    InvalidQuad,
    OutputLimit,
}

/// Append bounded precompiled surfaces atomically; a rejected append leaves the mesh unchanged.
pub fn append_model(
    mesh: &mut MeshData,
    cell: [f32; 3],
    quads: &[ModelQuad],
    occluded: impl Fn(Face) -> bool,
    max_faces: usize,
) -> Result<(), ModelMeshError> {
    if quads.len() > 1024 || cell.iter().any(|value| !value.is_finite()) {
        return Err(ModelMeshError::InvalidQuad);
    }
    let mut visible = Vec::new();
    let mut faces = 0usize;
    for quad in quads {
        if quad
            .shade_override
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
            || quad
                .positions
                .iter()
                .flatten()
                .chain(quad.uvs.iter().flatten())
                .chain(&quad.normal)
                .chain(&quad.tint)
                .chain(&quad.overlay_tint)
                .any(|value| !value.is_finite())
        {
            return Err(ModelMeshError::InvalidQuad);
        }
        if quad.cull_face.is_some_and(&occluded) {
            continue;
        }
        faces += if quad.double_sided { 2 } else { 1 };
        visible.push(quad);
    }
    if mesh
        .face_count()
        .checked_add(faces)
        .is_none_or(|count| count > max_faces)
        || mesh
            .positions
            .len()
            .checked_add(faces * 4)
            .is_none_or(|count| count > u32::MAX as usize)
    {
        return Err(ModelMeshError::OutputLimit);
    }
    for quad in visible {
        for back in 0..if quad.double_sided { 2 } else { 1 } {
            let order = if back == 0 {
                [0, 1, 2, 3]
            } else {
                [3, 2, 1, 0]
            };
            let positions = order.map(|index| add(cell, quad.positions[index]));
            let uvs = order.map(|index| quad.uvs[index]);
            let normal = quad.normal.map(|v| if back == 0 { v } else { -v });
            emit_quad(
                mesh,
                positions,
                uvs,
                normal,
                FaceAppearance {
                    material: quad.material,
                    overlay: quad.overlay,
                    tint: quad.tint,
                    overlay_tint: quad.overlay_tint,
                    shades: [-1.0; 4],
                },
            );
            let begin = mesh.shades.len() - 4;
            mesh.shades[begin..].fill(if quad.shade {
                quad.shade_override.unwrap_or(-1.0)
            } else {
                1.0
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod model_tests {
    use super::*;

    fn quad() -> ModelQuad {
        ModelQuad {
            positions: [
                [0.0, 0.0, 1.0],
                [1.0, 0.0, 1.0],
                [1.0, 1.0, 1.0],
                [0.0, 1.0, 1.0],
            ],
            uvs: [[0.2, 0.8], [0.9, 0.8], [0.9, 0.1], [0.2, 0.1]],
            normal: [0.0, 0.0, 1.0],
            material: 7,
            overlay: Some(8),
            tint: [0.7; 3],
            overlay_tint: [0.4; 3],
            cull_face: Some(Face::PosZ),
            shade: false,
            shade_override: None,
            double_sided: true,
        }
    }

    #[test]
    fn model_mesh_preserves_uv_winding_layers_and_atomic_budget() {
        let mut mesh = MeshData::default();
        let quad = quad();
        append_model(
            &mut mesh,
            [2.0, 3.0, 4.0],
            std::slice::from_ref(&quad),
            |_| false,
            2,
        )
        .unwrap();
        assert_eq!(mesh.face_count(), 2);
        assert_eq!(mesh.positions[0], [2.0, 3.0, 5.0]);
        assert_eq!(&mesh.uvs[..4], &quad.uvs);
        assert_eq!(mesh.normals[4], [0.0, 0.0, -1.0]);
        assert_eq!(mesh.materials, [7, 7]);
        assert_eq!(mesh.overlays, [8, 8]);
        assert_eq!(mesh.shades, [1.0; 8]);
        let before = mesh.clone();
        assert_eq!(
            append_model(
                &mut mesh,
                [0.0; 3],
                std::slice::from_ref(&quad),
                |_| false,
                3
            ),
            Err(ModelMeshError::OutputLimit)
        );
        assert_eq!(mesh, before);
        append_model(&mut mesh, [0.0; 3], &[quad], |_| true, 2).unwrap();
        assert_eq!(mesh, before);
    }
}

fn face_uv(face: Face, position: [f32; 3], cell: [f32; 3]) -> [f32; 2] {
    let local = [
        position[0] - cell[0],
        position[1] - cell[1],
        position[2] - cell[2],
    ];
    match face {
        Face::NegX => [local[2], 1.0 - local[1]],
        Face::PosX => [1.0 - local[2], 1.0 - local[1]],
        Face::NegY => [local[0], 1.0 - local[2]],
        Face::PosY => [local[0], local[2]],
        Face::NegZ => [1.0 - local[0], 1.0 - local[1]],
        Face::PosZ => [local[0], 1.0 - local[1]],
    }
}

/// Corner and edge vectors for a face, chosen so `u x v` equals the normal.
fn face_quad(face: Face, bounds: RenderBox, cell: [f32; 3]) -> ([f32; 3], [f32; 3], [f32; 3]) {
    let min = add(cell, bounds.min);
    let max = add(cell, bounds.max);
    let size = [
        bounds.max[0] - bounds.min[0],
        bounds.max[1] - bounds.min[1],
        bounds.max[2] - bounds.min[2],
    ];
    match face {
        Face::PosX => (
            [max[0], min[1], max[2]],
            [0.0, 0.0, -size[2]],
            [0.0, size[1], 0.0],
        ),
        Face::NegX => (
            [min[0], min[1], min[2]],
            [0.0, 0.0, size[2]],
            [0.0, size[1], 0.0],
        ),
        Face::PosY => (
            [min[0], max[1], max[2]],
            [size[0], 0.0, 0.0],
            [0.0, 0.0, -size[2]],
        ),
        Face::NegY => (
            [min[0], min[1], min[2]],
            [size[0], 0.0, 0.0],
            [0.0, 0.0, size[2]],
        ),
        Face::PosZ => (
            [min[0], min[1], max[2]],
            [size[0], 0.0, 0.0],
            [0.0, size[1], 0.0],
        ),
        Face::NegZ => (
            [max[0], min[1], min[2]],
            [-size[0], 0.0, 0.0],
            [0.0, size[1], 0.0],
        ),
    }
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

/// Revisions a mesh depends on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MeshJob {
    /// Section being meshed.
    pub section: SectionPos,
    /// Side length used for the build.
    pub side: u32,
    /// Terrain revision of the section at capture time.
    pub section_revision: Option<Revision>,
    /// Revisions of the six neighbors in [`Face::ALL`] order.
    pub neighbor_revisions: [Option<Revision>; 6],
}

impl MeshJob {
    /// Record the dependencies of a mesh request.
    pub fn capture(
        side: u32,
        section: SectionPos,
        revision_of: impl Fn(SectionPos) -> Option<Revision>,
    ) -> Self {
        let mut neighbor_revisions = [None; 6];
        for (slot, face) in neighbor_revisions.iter_mut().zip(Face::ALL) {
            *slot = face.neighbor_section(section).and_then(&revision_of);
        }
        Self {
            section,
            side,
            section_revision: revision_of(section),
            neighbor_revisions,
        }
    }

    /// Return whether the captured revisions are still current.
    pub fn validate(&self, revision_of: impl Fn(SectionPos) -> Option<Revision>) -> bool {
        if revision_of(self.section) != self.section_revision {
            return false;
        }
        for (face, captured) in Face::ALL.iter().zip(self.neighbor_revisions) {
            if face.neighbor_section(self.section).and_then(&revision_of) != captured {
                return false;
            }
        }
        true
    }
}

/// One accepted mesh and the stamp it was built from.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedMesh {
    /// Dependencies recorded when the mesh was requested.
    pub job: MeshJob,
    /// Finished geometry.
    pub mesh: MeshData,
}

/// Result of offering a mesh to the cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InsertOutcome {
    /// The mesh matched its dependencies and replaced the previous entry.
    Accepted,
    /// The data changed while the mesh was being built; the result is dropped.
    Stale,
}

/// Bounded store of finished section meshes.
///
/// The cache keeps one mesh per section. Without a focus it evicts in
/// coordinate order when it exceeds capacity, which keeps tests deterministic.
/// With a focus it evicts the section farthest from that focus. Ties release
/// the lower section coordinate. Neither policy deletes authoritative terrain;
/// the cache only stores derived meshes.
#[derive(Debug)]
pub struct MeshCache {
    capacity: usize,
    focus: Option<SectionPos>,
    entries: BTreeMap<SectionPos, CachedMesh>,
}

impl MeshCache {
    /// Create a cache holding at most `capacity` sections.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            focus: None,
            entries: BTreeMap::new(),
        }
    }

    /// Section the cache should prefer to keep.
    ///
    /// Distances are measured in section coordinates. Callers that move the
    /// camera should set the focus again and [`Self::publish_memory`] so the
    /// ledger ranks meshes with the new distances.
    pub fn set_focus(&mut self, focus: Option<SectionPos>) {
        self.focus = focus;
    }

    /// Focus currently used for eviction and ledger ranking.
    pub const fn focus(&self) -> Option<SectionPos> {
        self.focus
    }

    /// Number of cached meshes.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return whether the cache holds no mesh.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up a finished mesh.
    pub fn get(&self, section: SectionPos) -> Option<&CachedMesh> {
        self.entries.get(&section)
    }

    /// Offer a finished mesh.
    ///
    /// The stamp is validated against `revision_of` before insertion, so an
    /// asynchronous worker cannot publish a mesh that was computed from data the
    /// authority has already replaced.
    pub fn insert(
        &mut self,
        job: MeshJob,
        mesh: MeshData,
        revision_of: impl Fn(SectionPos) -> Option<Revision>,
    ) -> InsertOutcome {
        if !job.validate(&revision_of) {
            return InsertOutcome::Stale;
        }
        self.entries.insert(job.section, CachedMesh { job, mesh });
        while self.entries.len() > self.capacity {
            let Some(victim) = self.select_victim() else {
                break;
            };
            self.entries.remove(&victim);
        }
        InsertOutcome::Accepted
    }

    /// Drop one cached mesh.
    ///
    /// This does not release a ledger charge. Publish again, or release the
    /// charge that [`Self::publish_memory`] returned for `section`.
    pub fn remove(&mut self, section: SectionPos) -> Option<CachedMesh> {
        self.entries.remove(&section)
    }

    /// Replace `published` with one mesh-class charge per cached mesh that
    /// owns heap capacity.
    ///
    /// The exchange is atomic: if the meshes do not fit, `published` stays
    /// charged and this cache is unchanged. A mesh whose reserved capacity is
    /// zero is omitted. Bytes count that capacity, including unused slots, via
    /// [`MeshData::allocated_bytes`]. A missing focus ranks every mesh as far
    /// (`u64::MAX`).
    pub fn publish_memory(
        &self,
        ledger: &mut MemoryLedger,
        published: &[MemoryChargeId],
    ) -> Result<Vec<(SectionPos, MemoryChargeId)>, Fault> {
        let mut incoming = Vec::new();
        let mut sections = Vec::new();
        for (pos, cached) in &self.entries {
            let bytes = u64::try_from(cached.mesh.allocated_bytes()).unwrap_or(u64::MAX);
            if bytes == 0 {
                continue;
            }
            let distance_sq = self
                .focus
                .map(|focus| section_distance_sq(*pos, focus))
                .unwrap_or(u64::MAX);
            incoming.push(MemoryResident {
                class: MemoryClass::Mesh,
                bytes,
                rank: EvictionRank::Mesh { distance_sq },
            });
            sections.push(*pos);
        }
        let ids = ledger.exchange(published, &incoming)?;
        Ok(sections.into_iter().zip(ids).collect())
    }

    fn select_victim(&self) -> Option<SectionPos> {
        let mut victim = None;
        for pos in self.entries.keys().copied() {
            let replace = match (victim, self.focus) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(current), Some(focus)) => {
                    let distance = section_distance_sq(pos, focus);
                    let current_distance = section_distance_sq(current, focus);
                    distance > current_distance || (distance == current_distance && pos < current)
                }
            };
            if replace {
                victim = Some(pos);
            }
        }
        victim
    }

    /// Total CPU bytes held by cached meshes.
    pub fn byte_size(&self) -> usize {
        self.entries
            .values()
            .map(|cached| cached.mesh.byte_size())
            .sum()
    }
}

fn section_distance_sq(section: SectionPos, focus: SectionPos) -> u64 {
    let dx = axis_sq(section.x as i128 - focus.x as i128);
    let dy = axis_sq(section.y as i128 - focus.y as i128);
    let dz = axis_sq(section.z as i128 - focus.z as i128);
    let sum = dx.saturating_add(dy).saturating_add(dz);
    u64::try_from(sum).unwrap_or(u64::MAX)
}

fn axis_sq(delta: i128) -> u128 {
    let magnitude = delta.unsigned_abs();
    magnitude.saturating_mul(magnitude)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use torse_types::{MemoryBudget, MemoryLedger};

    use super::*;

    #[test]
    fn invalid_extents_never_sample_or_charge_scratch() {
        let sample = |_| -> MeshSample { panic!("invalid section must not sample") };
        let mut ledger = MemoryLedger::new(MemoryBudget::new(0));
        for (side, section) in [
            (0, SectionPos::ZERO),
            (65, SectionPos::ZERO),
            (u32::MAX, SectionPos::ZERO),
            (2, SectionPos::new(i64::MAX, 0, 0)),
            (2, SectionPos::new(0, i64::MIN, 0)),
            (3, SectionPos::new(0, 0, i64::MAX / 3)),
        ] {
            for mode in [MeshMode::Reference, MeshMode::Faces, MeshMode::Runs] {
                assert_eq!(
                    build_mesh_inner(
                        side,
                        section,
                        sample,
                        || false,
                        Some(usize::MAX / 16),
                        Some(&mut ledger),
                        mode
                    ),
                    Err(MeshBuildError::InvalidInput)
                );
            }
            assert_eq!(ledger.total_usage(), 0);
            for mesh in [
                build_mesh(side, section, sample),
                build_mesh_reference(side, section, sample),
                build_mesh_optimized(side, section, sample),
            ] {
                assert!(mesh.is_empty());
                assert_eq!(mesh.section, section);
            }
            assert_eq!(
                build_mesh_bounded(side, section, sample, || false, 4),
                Err(MeshBuildError::InvalidInput)
            );
            assert_eq!(
                build_mesh_optimized_bounded(side, section, sample, || false, 4),
                Err(MeshBuildError::InvalidInput)
            );
            assert_eq!(
                build_mesh_accounted(side, section, sample, &mut ledger),
                Err(MeshBuildError::InvalidInput)
            );
            assert_eq!(
                build_mesh_bounded(side, section, sample, || true, 4),
                Err(MeshBuildError::Cancelled)
            );
        }
    }

    #[test]
    fn extreme_legal_and_negative_sections_keep_geometry() {
        for side in [1_u32, 2, 3, 64] {
            let width = i64::from(side);
            for coordinate in [i64::MIN / width, (i64::MAX - (width - 1)) / width, -2] {
                let section = SectionPos::new(coordinate, coordinate, coordinate);
                let origin = checked_origin(side, section).unwrap();
                let sample = |pos: BlockPos| {
                    if [pos.x, pos.y, pos.z] == origin {
                        MeshSample::Ready(RenderBlock::uniform(1))
                    } else {
                        MeshSample::NotLoaded
                    }
                };
                let expected = build_mesh_reference(side, section, sample);
                assert_eq!(expected.face_count(), 6);
                assert_eq!(build_mesh(side, section, sample), expected);
                assert_eq!(
                    build_mesh_bounded(side, section, sample, || false, 6).unwrap(),
                    expected
                );
                let runs =
                    build_mesh_optimized_bounded(side, section, sample, || false, 6).unwrap();
                assert_eq!(runs.face_count(), 6);
                assert_eq!(runs.section, section);
                assert!(
                    runs.positions
                        .iter()
                        .all(|p| p.iter().all(|n| (0.0..=1.0).contains(n)))
                );
            }
        }
    }

    fn reference(
        side: u32,
        section: SectionPos,
        sample: impl Fn(BlockPos) -> MeshSample,
    ) -> MeshData {
        build_mesh_inner(
            side,
            section,
            sample,
            || false,
            None,
            None,
            MeshMode::Reference,
        )
        .unwrap()
    }

    fn solid_sample(side: u32, pos: BlockPos) -> MeshSample {
        let range = 0..i64::from(side);
        if range.contains(&pos.x) && range.contains(&pos.y) && range.contains(&pos.z) {
            MeshSample::Ready(RenderBlock::uniform(1))
        } else {
            MeshSample::NotLoaded
        }
    }

    #[test]
    fn row_shifts_skip_the_interior_and_preserve_word_endpoints() {
        for side in [32, 64] {
            let occupancy =
                RowOccupancy::build(side, &[0; 3], &|pos| solid_sample(side, pos), &|| false)
                    .unwrap();
            let coverage = occupancy.covered_faces(1, 1).unwrap();
            assert_eq!(
                RowCells::new(side, Some(&coverage)).collect::<Vec<_>>(),
                [0, side - 1]
            );
            let boundary = occupancy.covered_faces(0, 1).unwrap();
            assert_eq!(RowCells::new(side, Some(&boundary)).count(), side as usize);
        }
        assert_eq!(
            RowCells::new(65, None).collect::<Vec<_>>(),
            (0..65).collect::<Vec<_>>()
        );
    }

    #[test]
    fn empty_rows_are_not_sampled_again_after_the_prepass() {
        let samples = Cell::new(0);
        let mesh = build_mesh(32, SectionPos::ZERO, |_| {
            samples.set(samples.get() + 1);
            MeshSample::Ready(RenderBlock::EMPTY)
        });
        assert!(mesh.is_empty());
        assert_eq!(samples.get(), 32usize.pow(3));
    }

    #[test]
    fn dense_rows_match_the_reference_including_64_bit_boundaries() {
        for side in [1, 3, 32, 33, 64] {
            let optimized = build_mesh(side, SectionPos::ZERO, |pos| solid_sample(side, pos));
            let expected = reference(side, SectionPos::ZERO, |pos| solid_sample(side, pos));
            assert_eq!(optimized, expected, "side={side}");
            assert_eq!(optimized.face_count(), 6 * side as usize * side as usize);
        }
    }

    #[test]
    fn mixed_shapes_tints_materials_and_missing_halos_match_the_reference() {
        let sample = |pos: BlockPos| {
            let tag = (pos.x * 13 + pos.y * 7 + pos.z * 17).rem_euclid(11);
            let mut block = RenderBlock::uniform((tag % 3) as u16);
            block.face_materials = Some([1, 2, 1, 3, 2, 1]);
            block.face_overlays = Some([Some(4), None, None, Some(5), None, None]);
            block.face_tints = Some([[0.5, 0.25, 0.75]; 6]);
            block.face_overlay_tints = Some([[0.75, 0.5, 0.25]; 6]);
            match tag {
                0 => return MeshSample::NotLoaded,
                1 => return MeshSample::OutOfBounds,
                2 => return MeshSample::Ready(RenderBlock::EMPTY),
                3 => {
                    block.bounds = Some(RenderBox {
                        min: [0.0; 3],
                        max: [1.0, 0.5, 1.0],
                    });
                    block.occlusion = Face::NegY.bit();
                }
                4 => {
                    block.cross = true;
                    block.occlusion = 0;
                }
                5 => block.occlusion = 0,
                6 => block.occlusion = Face::PosX.bit(),
                _ => {}
            }
            MeshSample::Ready(block)
        };
        for side in [1, 3, 8, 16] {
            for section in [SectionPos::ZERO, SectionPos::new(-1, 1, -2)] {
                assert_eq!(
                    build_mesh(side, section, sample),
                    reference(side, section, sample)
                );
            }
        }
    }

    #[test]
    #[ignore = "CPU microbenchmark; run explicitly in release mode with --nocapture"]
    fn row_occupancy_cpu_comparison() {
        use std::time::Instant;
        for dense in [true, false] {
            let side = 32;
            let sample = |pos| {
                let pos = std::hint::black_box(pos);
                std::hint::black_box(if dense {
                    solid_sample(side, pos)
                } else {
                    MeshSample::Ready(RenderBlock::EMPTY)
                })
            };
            let optimized_calls = Cell::new(0usize);
            let optimized = build_mesh(side, SectionPos::ZERO, |pos| {
                optimized_calls.set(optimized_calls.get() + 1);
                sample(pos)
            });
            let reference_calls = Cell::new(0usize);
            assert_eq!(
                optimized,
                reference(side, SectionPos::ZERO, |pos| {
                    reference_calls.set(reference_calls.get() + 1);
                    sample(pos)
                })
            );
            let rounds = 20;
            let start = Instant::now();
            for _ in 0..rounds {
                std::hint::black_box(reference(side, SectionPos::ZERO, sample));
            }
            let reference_us = start.elapsed().as_micros();
            let start = Instant::now();
            for _ in 0..rounds {
                std::hint::black_box(build_mesh(side, SectionPos::ZERO, sample));
            }
            let optimized_us = start.elapsed().as_micros();
            println!(
                "dense={dense} side={side} rounds={rounds} reference_us={reference_us} optimized_us={optimized_us} reference_samples={} optimized_samples={}",
                reference_calls.get(),
                optimized_calls.get()
            );
        }
    }

    #[test]
    fn full_cube_rows_drop_buried_faces_and_keep_partial_cells() {
        let solid = |pos: BlockPos| {
            if (0..2).contains(&pos.x) && (0..2).contains(&pos.y) && (0..2).contains(&pos.z) {
                MeshSample::Ready(RenderBlock::uniform(1))
            } else {
                MeshSample::Ready(RenderBlock::EMPTY)
            }
        };
        let mesh = build_mesh(2, SectionPos::ZERO, solid);
        assert_eq!(mesh.face_count(), 24);

        let mut open = RenderBlock::uniform(1);
        open.occlusion = 0;
        let mut other = RenderBlock::uniform(2);
        other.occlusion = 0;
        let exposed = |pos: BlockPos| match (pos.x, pos.y, pos.z) {
            (0, 0, 0) => MeshSample::Ready(open),
            (1, 0, 0) => MeshSample::Ready(other),
            _ => MeshSample::Ready(RenderBlock::EMPTY),
        };
        assert_eq!(build_mesh(2, SectionPos::ZERO, exposed).face_count(), 12);

        let mut slab = RenderBlock::uniform(3);
        slab.bounds = Some(RenderBox {
            min: [0.0, 0.0, 0.0],
            max: [1.0, 0.5, 1.0],
        });
        slab.occlusion = 0;
        let mixed = |pos: BlockPos| match (pos.x, pos.y, pos.z) {
            (0, 0, 0) => MeshSample::Ready(RenderBlock::uniform(1)),
            (1, 0, 0) => MeshSample::Ready(slab),
            _ => MeshSample::Ready(RenderBlock::EMPTY),
        };
        let mixed_mesh = build_mesh(2, SectionPos::ZERO, mixed);
        assert_eq!(mixed_mesh.face_count(), 11);
    }

    #[test]
    fn buried_full_cube_is_sampled_only_while_building_the_mask() {
        let hits = Cell::new(0u32);
        let sample = |pos: BlockPos| {
            if pos == BlockPos::new(1, 1, 1) {
                hits.set(hits.get() + 1);
            }
            if (0..3).contains(&pos.x) && (0..3).contains(&pos.y) && (0..3).contains(&pos.z) {
                MeshSample::Ready(RenderBlock::uniform(1))
            } else {
                MeshSample::Ready(RenderBlock::EMPTY)
            }
        };
        let mesh = build_mesh(3, SectionPos::ZERO, sample);
        assert_eq!(mesh.face_count(), 54);
        assert_eq!(hits.get(), 1);
    }

    #[test]
    fn wide_rows_keep_the_bit_past_a_32_bit_word() {
        let sample = |pos: BlockPos| {
            if pos == BlockPos::new(32, 0, 0) {
                MeshSample::Ready(RenderBlock::uniform(1))
            } else {
                MeshSample::Ready(RenderBlock::EMPTY)
            }
        };
        assert_eq!(build_mesh(33, SectionPos::ZERO, sample).face_count(), 6);
    }

    #[test]
    fn failed_accounted_build_releases_the_occupancy_charge() {
        let sample = |pos: BlockPos| {
            if pos == BlockPos::ZERO {
                MeshSample::Ready(RenderBlock::uniform(1))
            } else {
                MeshSample::Ready(RenderBlock::EMPTY)
            }
        };
        let mut ledger = MemoryLedger::new(MemoryBudget::new(10_000));
        assert_eq!(
            super::build_mesh_inner(
                4,
                SectionPos::ZERO,
                sample,
                || true,
                Some(6),
                Some(&mut ledger),
                MeshMode::Faces
            ),
            Err(MeshBuildError::Cancelled)
        );
        assert_eq!(ledger.total_usage(), 0);
        assert_eq!(
            super::build_mesh_inner(
                4,
                SectionPos::ZERO,
                sample,
                || false,
                Some(0),
                Some(&mut ledger),
                MeshMode::Faces
            ),
            Err(MeshBuildError::OutputLimit)
        );
        assert_eq!(ledger.total_usage(), 0);
    }

    #[test]
    fn occupancy_table_is_charged_then_released() {
        let sample = |pos: BlockPos| {
            if pos == BlockPos::ZERO {
                MeshSample::Ready(RenderBlock::uniform(1))
            } else {
                MeshSample::Ready(RenderBlock::EMPTY)
            }
        };
        let mut empty = MemoryLedger::new(MemoryBudget::new(0));
        assert_eq!(
            build_mesh_accounted(4, SectionPos::ZERO, sample, &mut empty).unwrap_err(),
            MeshBuildError::Memory
        );
        assert_eq!(empty.total_usage(), 0);
        let mut ledger = MemoryLedger::new(MemoryBudget::new(10_000));
        let mesh = build_mesh_accounted(4, SectionPos::ZERO, sample, &mut ledger).unwrap();
        assert_eq!(ledger.total_usage(), 0);
        assert_eq!(mesh.face_count(), 6);
        let rectangle_cell = (size_of::<Option<FaceAppearance>>() + size_of::<bool>()) as u64;
        assert_eq!(
            occupancy_scratch_bytes(32),
            32 * 32 * 4 + 128 + 32 * 32 * rectangle_cell
        );
        assert_eq!(
            occupancy_scratch_bytes(64),
            64 * 64 * 8 + 512 + 64 * 64 * rectangle_cell
        );
        assert_eq!(occupancy_scratch_bytes(65), 0);
    }

    #[test]
    fn rectangle_work_is_reserved_before_zero_output_meshing() {
        let sampled = Cell::new(0);
        let empty = |_| {
            sampled.set(sampled.get() + 1);
            MeshSample::Ready(RenderBlock::EMPTY)
        };
        let mut denied = MemoryLedger::new(MemoryBudget::new(
            occupancy_scratch_bytes(64).saturating_sub(1),
        ));
        assert_eq!(
            build_mesh_inner(
                64,
                SectionPos::ZERO,
                empty,
                || false,
                Some(0),
                Some(&mut denied),
                MeshMode::Runs,
            ),
            Err(MeshBuildError::Memory)
        );
        assert_eq!(sampled.get(), 0, "denied work must not sample or allocate");
        assert_eq!(denied.total_usage(), 0);

        let mut exact = MemoryLedger::new(MemoryBudget::new(occupancy_scratch_bytes(64)));
        let mesh = build_mesh_inner(
            64,
            SectionPos::ZERO,
            |_| MeshSample::Ready(RenderBlock::EMPTY),
            || false,
            Some(0),
            Some(&mut exact),
            MeshMode::Runs,
        )
        .unwrap();
        assert!(mesh.is_empty());
        assert_eq!(exact.total_usage(), 0, "scratch is released after meshing");
    }

    #[test]
    fn bounded_meshing_matches_reference_and_discards_partial_cancelled_or_oversized_output() {
        let sample = single(Some(cube(0b11_1111)));
        let expected = build_mesh(4, SectionPos::ZERO, &sample);
        assert_eq!(
            build_mesh_bounded(4, SectionPos::ZERO, &sample, || false, 6).unwrap(),
            expected
        );
        assert_eq!(
            build_mesh_bounded(4, SectionPos::ZERO, &sample, || false, 5),
            Err(MeshBuildError::OutputLimit)
        );
        let checks = Cell::new(0);
        let cancelled = || {
            checks.set(checks.get() + 1);
            checks.get() >= 3
        };
        assert_eq!(
            build_mesh_bounded(4, SectionPos::ZERO, sample, cancelled, 6),
            Err(MeshBuildError::Cancelled)
        );
    }

    #[test]
    fn dependency_capture_at_coordinate_limits_keeps_unrepresentable_neighbors_absent() {
        let pos = SectionPos::new(i64::MAX, i64::MIN, 0);
        let job = MeshJob::capture(1, pos, |_| Some(Revision::INITIAL));
        assert_eq!(job.neighbor_revisions[Face::PosX.index() as usize], None);
        assert_eq!(job.neighbor_revisions[Face::NegY.index() as usize], None);
        assert!(job.validate(|_| Some(Revision::INITIAL)));
    }

    fn one_cell(section: SectionPos) -> MeshData {
        let origin = BlockPos::new(section.x, section.y, section.z);
        build_mesh(1, section, move |pos| {
            if pos == origin {
                MeshSample::Ready(cube(0b11_1111))
            } else {
                MeshSample::NotLoaded
            }
        })
    }

    fn cube(occlusion: u8) -> RenderBlock {
        RenderBlock {
            bounds: Some(RenderBox::FULL_CUBE),
            occlusion,
            material: 1,
            face_materials: None,
            ..RenderBlock::EMPTY
        }
    }

    fn single(state: Option<RenderBlock>) -> impl Fn(BlockPos) -> MeshSample {
        move |pos: BlockPos| {
            if pos == BlockPos::new(1, 1, 1) {
                match state {
                    Some(block) => MeshSample::Ready(block),
                    None => MeshSample::NotLoaded,
                }
            } else {
                MeshSample::Ready(RenderBlock {
                    bounds: None,
                    occlusion: 0,
                    material: 0,
                    face_materials: None,
                    ..RenderBlock::EMPTY
                })
            }
        }
    }

    #[test]
    fn isolated_cube_emits_six_faces() {
        let mesh = build_mesh(4, SectionPos::ZERO, single(Some(cube(0b11_1111))));
        assert_eq!(mesh.face_count(), 6);
        assert_eq!(mesh.positions.len(), 24);
        assert_eq!(mesh.triangle_count(), 12);
        assert_eq!(mesh.normals.len(), mesh.positions.len());
        assert_eq!(mesh.uvs.len(), mesh.positions.len());
    }

    #[test]
    fn every_quad_carries_local_face_coordinates() {
        let mesh = build_mesh(4, SectionPos::ZERO, single(Some(cube(0b11_1111))));
        for quad in mesh.uvs.chunks(4) {
            // Corner order is origin, +u, +u+v, +v.
            assert_eq!(quad, [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]]);
        }
    }

    #[test]
    fn slab_side_uses_the_projected_half_of_the_texture() {
        let block = RenderBlock {
            bounds: Some(RenderBox {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 0.5, 1.0],
            }),
            ..cube(0b11_1111)
        };
        let mesh = build_mesh(4, SectionPos::ZERO, single(Some(block)));
        let side = mesh
            .normals
            .chunks(4)
            .zip(mesh.uvs.chunks(4))
            .find(|(normals, _)| normals[0] == Face::NegX.normal())
            .map(|(_, uvs)| uvs)
            .expect("slab side exists");
        assert_eq!(side, &[[0.0, 1.0], [1.0, 1.0], [1.0, 0.5], [0.0, 0.5]]);
        for (normals, uvs) in mesh.normals.chunks(4).zip(mesh.uvs.chunks(4)) {
            if normals[0] == Face::PosY.normal() || normals[0] == Face::NegY.normal() {
                assert_eq!(uvs, [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]]);
            }
        }
    }

    #[test]
    fn face_overlay_and_tints_are_carried_per_quad() {
        let block = RenderBlock {
            bounds: Some(RenderBox::FULL_CUBE),
            face_overlays: Some([None, None, None, Some(7), None, None]),
            face_tints: Some([[0.25, 0.5, 0.75]; 6]),
            face_overlay_tints: Some([[0.8, 0.6, 0.4]; 6]),
            ..cube(0b11_1111)
        };
        let mesh = build_mesh(4, SectionPos::ZERO, single(Some(block)));
        assert_eq!(
            mesh.overlays,
            [u16::MAX, u16::MAX, u16::MAX, 7, u16::MAX, u16::MAX]
        );
        assert!(
            mesh.tints
                .chunks(4)
                .all(|quad| quad == [[0.25, 0.5, 0.75]; 4])
        );
        assert!(
            mesh.overlay_tints
                .chunks(4)
                .all(|quad| quad == [[0.8, 0.6, 0.4]; 4])
        );
    }

    #[test]
    fn crossed_planes_emit_two_double_sided_faces_without_occlusion() {
        let block = RenderBlock {
            cross: true,
            material: 5,
            occlusion: 0,
            ..RenderBlock::EMPTY
        };
        let mesh = build_mesh(4, SectionPos::ZERO, single(Some(block)));
        assert_eq!(mesh.face_count(), 2);
        assert_eq!(mesh.positions.len(), 8);
        assert_eq!(mesh.triangle_count(), 4);
        assert_eq!(mesh.materials, [5, 5]);
    }

    #[test]
    fn crossed_planes_and_neighbors_never_cull_each_other() {
        let cross = RenderBlock {
            bounds: Some(RenderBox::FULL_CUBE),
            cross: true,
            material: 5,
            occlusion: 0,
            ..RenderBlock::EMPTY
        };
        let cube = RenderBlock {
            bounds: Some(RenderBox::FULL_CUBE),
            material: 5,
            occlusion: 0,
            ..RenderBlock::EMPTY
        };
        let sample = |pos: BlockPos| match pos {
            BlockPos { x: 1, y: 1, z: 1 } => MeshSample::Ready(cross),
            BlockPos { x: 2, y: 1, z: 1 } => MeshSample::Ready(cube),
            _ => MeshSample::Ready(RenderBlock::EMPTY),
        };
        let mesh = build_mesh(4, SectionPos::ZERO, sample);
        assert_eq!(mesh.face_count(), 8);
    }

    #[test]
    fn per_face_materials_select_by_face_order() {
        let block = RenderBlock {
            bounds: Some(RenderBox::FULL_CUBE),
            occlusion: 0b11_1111,
            material: 9,
            face_materials: Some([10, 11, 12, 13, 14, 15]),
            ..RenderBlock::EMPTY
        };
        let mesh = build_mesh(4, SectionPos::ZERO, single(Some(block)));
        assert_eq!(mesh.materials, vec![10, 11, 12, 13, 14, 15]);
    }

    #[test]
    fn uniform_block_keeps_one_material_on_every_face() {
        let mesh = build_mesh(4, SectionPos::ZERO, single(Some(RenderBlock::uniform(7))));
        assert_eq!(mesh.materials, vec![7; 6]);
    }

    #[test]
    fn shared_face_between_two_cubes_is_culled() {
        let sample = |pos: BlockPos| {
            if pos == BlockPos::new(1, 1, 1) || pos == BlockPos::new(2, 1, 1) {
                MeshSample::Ready(cube(0b11_1111))
            } else {
                MeshSample::Ready(RenderBlock {
                    bounds: None,
                    occlusion: 0,
                    material: 0,
                    face_materials: None,
                    ..RenderBlock::EMPTY
                })
            }
        };
        let mesh = build_mesh(4, SectionPos::ZERO, sample);
        assert_eq!(mesh.face_count(), 10);
    }

    #[test]
    fn missing_neighbor_data_keeps_faces_visible() {
        let mesh = build_mesh(4, SectionPos::ZERO, single(None));
        assert!(
            mesh.is_empty(),
            "an unloaded cell has no geometry of its own"
        );
    }

    #[test]
    fn unloaded_boundary_neighbor_does_not_cull() {
        // A cube at the section edge with an unloaded neighbor must still emit
        // the boundary face; otherwise a hole appears exactly at the seam.
        let sample = |pos: BlockPos| {
            if pos == BlockPos::new(3, 1, 1) {
                MeshSample::Ready(cube(0b11_1111))
            } else if pos.x == 4 {
                MeshSample::NotLoaded
            } else {
                MeshSample::Ready(RenderBlock {
                    bounds: None,
                    occlusion: 0,
                    material: 0,
                    face_materials: None,
                    ..RenderBlock::EMPTY
                })
            }
        };
        let mesh = build_mesh(4, SectionPos::ZERO, sample);
        assert_eq!(mesh.face_count(), 6);
    }

    #[test]
    fn slab_top_face_survives_a_covering_neighbor() {
        let slab = RenderBlock {
            bounds: Some(RenderBox {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 0.5, 1.0],
            }),
            occlusion: 0b11_1111,
            material: 1,
            face_materials: None,
            ..RenderBlock::EMPTY
        };
        let sample = |pos: BlockPos| {
            if pos == BlockPos::new(1, 1, 1) {
                MeshSample::Ready(slab)
            } else if pos == BlockPos::new(1, 2, 1) {
                MeshSample::Ready(cube(0b11_1111))
            } else {
                MeshSample::Ready(RenderBlock {
                    bounds: None,
                    occlusion: 0,
                    material: 0,
                    face_materials: None,
                    ..RenderBlock::EMPTY
                })
            }
        };
        let mesh = build_mesh(4, SectionPos::ZERO, sample);
        // The slab keeps its top face at y = 1.5 because it does not reach y = 1.
        // The cube above also contributes a +y face, so compare positions.
        let up = Face::PosY.normal();
        let slab_top = mesh
            .positions
            .chunks(4)
            .zip(mesh.normals.chunks(4))
            .filter(|(corners, normals)| {
                normals[0] == up && (corners[0][1] - 1.5).abs() < f32::EPSILON
            })
            .count();
        assert_eq!(slab_top, 1);
    }

    #[test]
    fn neighbor_without_occlusion_bit_does_not_cull() {
        let sample = |pos: BlockPos| {
            if pos == BlockPos::new(1, 1, 1) {
                MeshSample::Ready(cube(0b11_1111))
            } else if pos == BlockPos::new(2, 1, 1) {
                // A different state: the mask rule applies, not the
                // same-material merge.
                MeshSample::Ready(RenderBlock {
                    bounds: Some(RenderBox::FULL_CUBE),
                    occlusion: 0,
                    material: 2,
                    face_materials: None,
                    ..RenderBlock::EMPTY
                })
            } else {
                MeshSample::Ready(RenderBlock {
                    bounds: None,
                    occlusion: 0,
                    material: 0,
                    face_materials: None,
                    ..RenderBlock::EMPTY
                })
            }
        };
        let mesh = build_mesh(4, SectionPos::ZERO, sample);
        // The positive neighbor claims no coverage, so the first cube keeps its
        // +x face. The second cube's -x face is still culled by the first cube,
        // which does claim full coverage.
        assert_eq!(mesh.face_count(), 11);
    }

    #[test]
    fn same_material_contact_faces_merge_without_coverage_claims() {
        let sample = |pos: BlockPos| {
            if (1..=2).contains(&pos.x) && pos.y == 1 && pos.z == 1 {
                // Neither cube claims coverage, yet the shared faces are of
                // one material with equal sections and merge away.
                MeshSample::Ready(cube(0))
            } else {
                MeshSample::Ready(RenderBlock {
                    bounds: None,
                    occlusion: 0,
                    material: 0,
                    face_materials: None,
                    ..RenderBlock::EMPTY
                })
            }
        };
        let mesh = build_mesh(4, SectionPos::ZERO, sample);
        assert_eq!(mesh.face_count(), 10);
    }

    #[test]
    fn same_material_slab_side_merges_into_the_larger_face() {
        // A lower slab beside a full cube of the same material: the slab's
        // side face is a subset and disappears, the cube's larger face stays
        // and carries the whole interface.
        let sample = |pos: BlockPos| {
            if pos == BlockPos::new(1, 1, 1) {
                MeshSample::Ready(RenderBlock {
                    bounds: Some(RenderBox {
                        min: [0.0, 0.0, 0.0],
                        max: [1.0, 0.5, 1.0],
                    }),
                    occlusion: 0,
                    material: 1,
                    face_materials: None,
                    ..RenderBlock::EMPTY
                })
            } else if pos == BlockPos::new(2, 1, 1) {
                MeshSample::Ready(cube(0))
            } else {
                MeshSample::Ready(RenderBlock {
                    bounds: None,
                    occlusion: 0,
                    material: 0,
                    face_materials: None,
                    ..RenderBlock::EMPTY
                })
            }
        };
        let mesh = build_mesh(4, SectionPos::ZERO, sample);
        // Slab keeps five faces (+x merged); the cube keeps six (-x carries
        // the interface, including above the slab's empty half).
        assert_eq!(mesh.face_count(), 11);
    }

    #[test]
    fn positions_are_section_local() {
        let sample = |pos: BlockPos| {
            if pos == BlockPos::new(-5, 1, 1) {
                MeshSample::Ready(cube(0b11_1111))
            } else {
                MeshSample::Ready(RenderBlock {
                    bounds: None,
                    occlusion: 0,
                    material: 0,
                    face_materials: None,
                    ..RenderBlock::EMPTY
                })
            }
        };
        // Section -1 covers blocks -4..= -1 with side 4, so block -5 is not in it.
        let mesh = build_mesh(4, SectionPos::new(-1, 0, 0), sample);
        assert!(mesh.is_empty());
    }

    #[test]
    fn job_rejects_later_terrain_changes() {
        let revision = Cell::new(Some(Revision(3)));
        let job = MeshJob::capture(4, SectionPos::ZERO, |pos| {
            if pos == SectionPos::ZERO {
                revision.get()
            } else {
                None
            }
        });
        assert!(job.validate(|pos| if pos == SectionPos::ZERO {
            revision.get()
        } else {
            None
        }));
        revision.set(Some(Revision(4)));
        assert!(!job.validate(|pos| if pos == SectionPos::ZERO {
            revision.get()
        } else {
            None
        }));
    }

    #[test]
    fn cache_drops_a_stale_mesh_instead_of_overwriting() {
        let revision = Cell::new(Some(Revision(1)));
        let job = MeshJob::capture(4, SectionPos::ZERO, |pos| {
            if pos == SectionPos::ZERO {
                revision.get()
            } else {
                None
            }
        });
        let mesh = build_mesh(4, SectionPos::ZERO, single(Some(cube(0b11_1111))));
        let mut cache = MeshCache::new(4);
        assert_eq!(
            cache.insert(job, mesh.clone(), |pos| if pos == SectionPos::ZERO {
                revision.get()
            } else {
                None
            }),
            InsertOutcome::Accepted
        );
        revision.set(Some(Revision(2)));
        assert_eq!(
            cache.insert(job, mesh, |pos| if pos == SectionPos::ZERO {
                revision.get()
            } else {
                None
            }),
            InsertOutcome::Stale
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_evicts_beyond_capacity() {
        let mut cache = MeshCache::new(1);
        let mut build = |section: SectionPos| {
            let job = MeshJob::capture(4, section, |_| None);
            let mesh = build_mesh(4, section, |_| MeshSample::NotLoaded);
            cache.insert(job, mesh, |_| None)
        };
        assert_eq!(build(SectionPos::ZERO), InsertOutcome::Accepted);
        assert_eq!(build(SectionPos::new(1, 0, 0)), InsertOutcome::Accepted);
        assert_eq!(cache.len(), 1);
        assert!(cache.get(SectionPos::ZERO).is_none());
    }

    #[test]
    fn cache_with_focus_keeps_the_near_section() {
        let mesh = build_mesh(1, SectionPos::ZERO, |_| MeshSample::Ready(cube(0b11_1111)));
        let mut focused = MeshCache::new(1);
        focused.set_focus(Some(SectionPos::ZERO));
        let insert = |cache: &mut MeshCache, section: SectionPos| {
            let job = MeshJob::capture(1, section, |_| None);
            cache.insert(job, mesh.clone(), |_| None)
        };
        assert_eq!(
            insert(&mut focused, SectionPos::ZERO),
            InsertOutcome::Accepted
        );
        assert_eq!(
            insert(&mut focused, SectionPos::new(4, 0, 0)),
            InsertOutcome::Accepted
        );
        assert!(focused.get(SectionPos::ZERO).is_some());
        assert!(focused.get(SectionPos::new(4, 0, 0)).is_none());
    }

    #[test]
    fn publish_memory_ranks_the_far_mesh_and_keeps_the_previous_charge() {
        let near = one_cell(SectionPos::ZERO);
        let far_section = SectionPos::new(3, 0, 0);
        let far = one_cell(far_section);
        let bytes = near.allocated_bytes() as u64;
        assert!(bytes > 0);
        assert_eq!(far.allocated_bytes() as u64, bytes);

        let mut cache = MeshCache::new(4);
        cache.set_focus(Some(SectionPos::ZERO));
        let near_job = MeshJob::capture(1, SectionPos::ZERO, |_| None);
        cache.insert(near_job, near, |_| None);

        let mut ledger = MemoryLedger::new(MemoryBudget::new(bytes));
        let published = cache.publish_memory(&mut ledger, &[]).unwrap();
        assert_eq!(ledger.usage(MemoryClass::Mesh), bytes);

        let far_job = MeshJob::capture(1, far_section, |_| None);
        cache.insert(far_job, far, |_| None);
        let held: Vec<_> = published.iter().map(|(_, id)| *id).collect();
        let error = cache.publish_memory(&mut ledger, &held).unwrap_err();
        assert_eq!(error.code, torse_types::FaultCode::ResourceLimit);
        assert_eq!(ledger.usage(MemoryClass::Mesh), bytes);
        assert!(ledger.get(held[0]).is_some());
        assert_eq!(cache.len(), 2);

        let mut room = MemoryLedger::new(MemoryBudget::new(bytes.saturating_mul(2)));
        let ranked = cache.publish_memory(&mut room, &[]).unwrap();
        let plan = room.eviction_plan(1);
        let far_id = ranked
            .iter()
            .find(|(pos, _)| *pos == far_section)
            .expect("far mesh is charged")
            .1;
        assert_eq!(plan.charges.first().copied(), Some(far_id));
    }

    #[test]
    fn publish_memory_charges_reserved_capacity_of_a_nonempty_mesh() {
        let mesh = quad_with_reserved_positions(1024);
        assert_eq!(mesh.byte_size(), 268);
        assert_eq!(mesh.allocated_bytes(), 12_508);

        let mut cache = MeshCache::new(1);
        let job = MeshJob::capture(1, SectionPos::ZERO, |_| None);
        cache.insert(job, mesh, |_| None);

        let mut tight = MemoryLedger::new(MemoryBudget::new(268));
        let error = cache.publish_memory(&mut tight, &[]).unwrap_err();
        assert_eq!(error.code, torse_types::FaultCode::ResourceLimit);
        assert_eq!(tight.total_usage(), 0);

        let mut ledger = MemoryLedger::new(MemoryBudget::new(12_508));
        cache.publish_memory(&mut ledger, &[]).unwrap();
        assert_eq!(ledger.usage(torse_types::MemoryClass::Mesh), 12_508);
    }

    #[test]
    fn publish_memory_charges_an_empty_mesh_that_keeps_capacity() {
        let mesh = empty_with_reserved_positions(1024);
        assert_eq!(mesh.byte_size(), 0);
        assert_eq!(mesh.allocated_bytes(), 12_288);

        let mut cache = MeshCache::new(1);
        let job = MeshJob::capture(1, SectionPos::ZERO, |_| None);
        cache.insert(job, mesh, |_| None);

        let mut ledger = MemoryLedger::new(MemoryBudget::new(0));
        let error = cache.publish_memory(&mut ledger, &[]).unwrap_err();
        assert_eq!(error.code, torse_types::FaultCode::ResourceLimit);
        assert_eq!(ledger.total_usage(), 0);
        assert!(cache.get(SectionPos::ZERO).is_some());
    }

    fn quad_with_reserved_positions(slots: usize) -> MeshData {
        let mut positions = Vec::with_capacity(slots);
        positions.extend([
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ]);
        MeshData {
            section: SectionPos::ZERO,
            positions,
            normals: vec![[0.0, 1.0, 0.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            materials: vec![1],
            overlays: vec![u16::MAX],
            tints: vec![[1.0, 1.0, 1.0]; 4],
            overlay_tints: vec![[1.0, 1.0, 1.0]; 4],
            shades: vec![1.0; 4],
        }
    }

    fn empty_with_reserved_positions(slots: usize) -> MeshData {
        MeshData {
            section: SectionPos::ZERO,
            positions: Vec::with_capacity(slots),
            normals: Vec::new(),
            uvs: Vec::new(),
            indices: Vec::new(),
            materials: Vec::new(),
            overlays: Vec::new(),
            tints: Vec::new(),
            overlay_tints: Vec::new(),
            shades: Vec::new(),
        }
    }
}
