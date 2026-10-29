//! Bounded, read-only preparation of authoritative entity translations.
//!
//! Collision queries are recorded before applying the transaction's terrain
//! overlay. Publication checks those observations again before any write. This
//! catches a host mutating terrain between prepare and publish, including a
//! previously absent section becoming loaded. A journal can reject or drop the
//! prepared record without moving an entity or changing an index.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use torse_content::Shape;
use torse_types::{BlockPos, EntityHandle, EntityId, RejectCode};
use torse_world::{
    BlockEdit, BlockStateRegistry, EntityMove, EntityStore, Position, SectionStore, TerrainQuery,
};
use torse_world_services::{
    Aabb, CollisionCell, CollisionSource, LocalBox, collides, sweep, sweep_bounded,
};

use crate::transaction::Movement;

/// Prototype limit for each absolute center coordinate, in block units.
pub const MAX_MOVEMENT_COORDINATE: f64 = 1_048_576.0;
/// Maximum requested translation along one axis in one transaction.
pub const MAX_MOVEMENT_DELTA: f64 = 16.0;
/// Largest collider half extent along one axis.
pub const MAX_COLLIDER_HALF: f64 = 4.0;
/// Maximum entity translations in a single atomic transaction.
pub const MAX_TRANSACTION_MOVEMENTS: usize = 256;

pub(crate) struct ResolvedMovements {
    pub moves: Vec<EntityMove>,
    pub reads: Vec<(BlockPos, TerrainQuery)>,
}

/// Escape directions: up first (the common burial), then sideways.
const ESCAPE_DIRECTIONS: [[f64; 3]; 8] = [
    [0.0, 1.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, 1.0, 0.0],
    [1.0, 0.0, 0.0],
    [-1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0],
    [0.0, 0.0, -1.0],
];
/// Escape step budget per direction.
const ESCAPE_STEPS: usize = 8;

/// Resolve one stuck entity: find the nearest free position by stepping
/// along the escape directions and report the relocation as a movement.
///
/// Unlike ordinary movement preparation, the start position is allowed to overlap
/// terrain -- that is the condition being escaped. The destination must be
/// free, offsets are bounded, and entity identity is validated exactly like
/// a normal move. `None` means no free neighbour was reachable; the entity
/// stays wedged and the caller reports the failure.
pub fn prepare_escape(
    store: &SectionStore,
    entities: &EntityStore,
    registry: &BlockStateRegistry,
    handle: EntityHandle,
    id: EntityId,
    half: [f64; 3],
) -> Result<Option<EntityMove>, RejectCode> {
    if entities.side() != store.side() {
        return Err(RejectCode::InvalidData);
    }
    let entity = entities.get(handle).ok_or(RejectCode::MissingTarget)?;
    if entity.id != id {
        return Err(RejectCode::MissingTarget);
    }
    let Some(half) = crate::body::of(entity, &[], half)? else {
        return Ok(None);
    };
    let position = entity.position;
    if !bounded_position(position)
        || half
            .iter()
            .any(|v| !v.is_finite() || *v <= 0.0 || *v > MAX_COLLIDER_HALF)
    {
        return Err(RejectCode::InvalidData);
    }
    let view = CollisionView {
        store,
        registry,
        overlay: BTreeMap::new(),
        reads: RefCell::new(BTreeMap::new()),
        unknown: RefCell::new(false),
    };
    let step = half[1] / 2.0;
    for step_index in 1..=ESCAPE_STEPS {
        let offset = f64::from(step_index as u32) * step;
        for direction in ESCAPE_DIRECTIONS {
            let candidate_center = [
                position.x + direction[0] * offset,
                position.y + direction[1] * offset,
                position.z + direction[2] * offset,
            ];
            let Some(candidate) = bounded_center(candidate_center, half) else {
                continue;
            };
            let candidate_aabb = Aabb::from_center(candidate, half);
            let blocked = collides(&view, &candidate_aabb);
            if std::env::var_os("TORSE_ESCAPE_DEBUG").is_some() {
                eprintln!(
                    "escape step {step_index} dir {direction:?} center {candidate:?} blocked={blocked} unknown={}",
                    view.unknown.borrow()
                );
            }
            if blocked {
                continue;
            }
            if *view.unknown.borrow() {
                return Err(RejectCode::InvalidData);
            }
            return Ok(Some(EntityMove {
                id,
                previous_bits: position.to_bits(),
                position_bits: Position::new(
                    candidate_aabb.center()[0],
                    candidate_aabb.center()[1],
                    candidate_aabb.center()[2],
                )
                .to_bits(),
            }));
        }
    }
    Ok(None)
}

/// Centre of an escape candidate; `None` when its collider would leave the
/// bounded region.
fn bounded_center(center: [f64; 3], half: [f64; 3]) -> Option<[f64; 3]> {
    if !center.iter().all(|v| v.is_finite()) {
        return None;
    }
    let aabb = Aabb::from_center(center, half);
    if !bounded_position(Position::new(aabb.min[0], aabb.min[1], aabb.min[2]))
        || !bounded_position(Position::new(aabb.max[0], aabb.max[1], aabb.max[2]))
    {
        return None;
    }
    Some(center)
}

/// Start-position policy for movement preparation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MovementMode {
    /// The start must not overlap terrain; overlap is a conflict.
    Normal,
    /// The start may overlap terrain (a wedged entity escaping); every
    /// destination must be free, and each request moves exactly one entity.
    Escape,
}

pub(crate) fn prepare_movements(
    store: &SectionStore,
    entities: &EntityStore,
    registry: &BlockStateRegistry,
    edits: &[BlockEdit],
    requests: &[Movement],
    mode: MovementMode,
) -> Result<ResolvedMovements, RejectCode> {
    if requests.len() > MAX_TRANSACTION_MOVEMENTS {
        return Err(RejectCode::QuotaExceeded);
    }
    if entities.side() != store.side() {
        return Err(RejectCode::InvalidData);
    }
    let view = CollisionView {
        store,
        registry,
        overlay: edits.iter().map(|edit| (edit.pos, edit.state)).collect(),
        reads: RefCell::new(BTreeMap::new()),
        unknown: RefCell::new(false),
    };
    let mut seen = BTreeSet::new();
    let mut moves = Vec::with_capacity(requests.len());
    for request in requests {
        if !seen.insert(request.id) {
            return Err(RejectCode::InvalidData);
        }
        let entity = entities
            .get(request.handle)
            .ok_or(RejectCode::MissingTarget)?;
        if entity.id != request.id {
            return Err(RejectCode::MissingTarget);
        }
        if entity
            .components
            .component(&torse_world::block_instance_schema())
            .is_some()
        {
            return Err(RejectCode::PermissionDenied);
        }
        if entity.position.to_bits() != request.previous_bits {
            return Err(RejectCode::Conflict);
        }
        let delta = request.delta_bits.map(f64::from_bits);
        let half = request.half_bits.map(f64::from_bits);
        let position = entity.position;
        if !bounded_position(position)
            || delta
                .iter()
                .any(|v| !v.is_finite() || v.abs() > MAX_MOVEMENT_DELTA)
            || half
                .iter()
                .any(|v| !v.is_finite() || *v <= 0.0 || *v > MAX_COLLIDER_HALF)
            || !bounded_position(position.translated(delta))
        {
            return Err(RejectCode::InvalidData);
        }
        // Declared bodies use adaptive steps. Undeclared entities keep the
        // original request bounds and minimum width for compatibility.
        let declared =
            torse_world::EntityCollider::of(entity).map_err(|_| RejectCode::InvalidData)?;
        let Some(half) = crate::body::of(entity, &[], half)? else {
            trace_loaded(&view, position, delta)?;
            if *view.unknown.borrow() {
                return Err(RejectCode::NotLoaded);
            }
            moves.push(EntityMove {
                id: request.id,
                previous_bits: request.previous_bits,
                position_bits: position.translated(delta).to_bits(),
            });
            continue;
        };
        if declared.is_none() && half.iter().any(|v| *v < torse_world_services::MAX_SUBSTEP) {
            return Err(RejectCode::InvalidData);
        }
        let collider = Aabb::from_center([position.x, position.y, position.z], half);
        let overlapping = collides(&view, &collider);
        if *view.unknown.borrow() {
            return Err(RejectCode::InvalidData);
        }
        // Escape moves are the one path that may start overlapped -- that is
        // the state being escaped -- but they carry a single entity per
        // request and the destination must be free. No sweep: the caller
        // already resolved the destination through prepare_escape.
        if mode == MovementMode::Escape {
            let _ = overlapping;
            let target = position.translated(delta);
            let target_aabb = Aabb::from_center([target.x, target.y, target.z], half);
            if collides(&view, &target_aabb) || *view.unknown.borrow() {
                return Err(RejectCode::Conflict);
            }
            moves.push(EntityMove {
                id: request.id,
                previous_bits: request.previous_bits,
                position_bits: target.to_bits(),
            });
            continue;
        }
        let result = if declared.is_some() {
            sweep_bounded(&view, collider, delta).ok_or(RejectCode::QuotaExceeded)?
        } else {
            sweep(&view, collider, delta)
        };
        if *view.unknown.borrow() {
            return Err(RejectCode::InvalidData);
        }
        let result_aabb = result.aabb;
        if collides(&view, &result_aabb) {
            return Err(RejectCode::Conflict);
        }
        let center = result_aabb.center();
        moves.push(EntityMove {
            id: request.id,
            previous_bits: request.previous_bits,
            position_bits: Position::new(center[0], center[1], center[2]).to_bits(),
        });
    }
    Ok(ResolvedMovements {
        moves,
        reads: view.reads.into_inner().into_iter().collect(),
    })
}

fn bounded_position(position: Position) -> bool {
    position.is_finite()
        && [position.x, position.y, position.z]
            .iter()
            .all(|value| value.abs() <= MAX_MOVEMENT_COORDINATE)
}

/// Read-only collision used by detached prediction; it never edits an entity store.
pub(crate) fn predict_at(
    store: &SectionStore,
    registry: &BlockStateRegistry,
    position: Position,
    half: [f64; 3],
    delta: [f64; 3],
) -> Result<Position, RejectCode> {
    if !bounded_position(position)
        || !bounded_position(position.translated(delta))
        || half
            .iter()
            .any(|v| !v.is_finite() || *v <= 0.0 || *v > MAX_COLLIDER_HALF)
        || delta
            .iter()
            .any(|v| !v.is_finite() || v.abs() > MAX_MOVEMENT_DELTA)
    {
        return Err(RejectCode::InvalidData);
    }
    let view = CollisionView {
        store,
        registry,
        overlay: BTreeMap::new(),
        reads: RefCell::new(BTreeMap::new()),
        unknown: RefCell::new(false),
    };
    let body = Aabb::from_center([position.x, position.y, position.z], half);
    let _ = collides(&view, &body);
    if *view.unknown.borrow() {
        return Err(RejectCode::NotLoaded);
    }
    let result = sweep_bounded(&view, body, delta).ok_or(RejectCode::QuotaExceeded)?;
    let blocked = collides(&view, &result.aabb);
    if *view.unknown.borrow() {
        return Err(RejectCode::NotLoaded);
    }
    if blocked {
        return Err(RejectCode::Conflict);
    }
    let center = result.aabb.center();
    Ok(Position::new(center[0], center[1], center[2]))
}

/// Traverse every crossed cell, including very short diagonal intersections.
/// With at most 16 cells per axis, 96 visits cover the endpoint and boundary ties.
fn trace_loaded(
    view: &CollisionView<'_>,
    start: Position,
    delta: [f64; 3],
) -> Result<(), RejectCode> {
    let origin = [start.x, start.y, start.z];
    let mut cell = origin.map(|v| v.floor() as i64);
    let end = std::array::from_fn::<_, 3, _>(|a| (origin[a] + delta[a]).floor() as i64);
    let step = delta.map(|d| {
        if d > 0.0 {
            1
        } else if d < 0.0 {
            -1
        } else {
            0
        }
    });
    let interval = delta.map(|d| {
        if d == 0.0 {
            f64::INFINITY
        } else {
            1.0 / d.abs()
        }
    });
    let mut next = std::array::from_fn::<_, 3, _>(|a| {
        if step[a] == 0 {
            f64::INFINITY
        } else {
            ((cell[a] + i64::from(step[a] > 0)) as f64 - origin[a]) / delta[a]
        }
    });
    for _ in 0..96 {
        if matches!(
            view.collision(BlockPos::new(cell[0], cell[1], cell[2])),
            CollisionCell::NotLoaded | CollisionCell::OutOfBounds
        ) {
            return Err(RejectCode::NotLoaded);
        }
        if cell == end {
            return Ok(());
        }
        let axis = (0..3).min_by(|a, b| next[*a].total_cmp(&next[*b])).unwrap();
        cell[axis] += step[axis];
        next[axis] += interval[axis];
    }
    Err(RejectCode::QuotaExceeded)
}

/// Whether an enabled body at `position` is clear of terrain.
///
/// `Ok` lists the cells that were observed. Overlap is [`RejectCode::Conflict`].
/// Missing cells and the lower world boundary are [`RejectCode::NotLoaded`].
/// Space above the build ceiling is known empty for body movement.
pub fn observe_body(
    store: &SectionStore,
    registry: &BlockStateRegistry,
    position: Position,
    half: [f64; 3],
) -> Result<Vec<(BlockPos, TerrainQuery)>, RejectCode> {
    let center = bounded_center([position.x, position.y, position.z], half)
        .ok_or(RejectCode::InvalidData)?;
    let view = CollisionView {
        store,
        registry,
        overlay: BTreeMap::new(),
        reads: RefCell::new(BTreeMap::new()),
        unknown: RefCell::new(false),
    };
    let blocked = collides(&view, &Aabb::from_center(center, half));
    let unknown = *view.unknown.borrow();
    let reads: Vec<_> = view.reads.into_inner().into_iter().collect();
    if unknown
        || reads
            .iter()
            .any(|(_, query)| matches!(query, TerrainQuery::NotLoaded | TerrainQuery::OutOfBounds))
    {
        return Err(RejectCode::NotLoaded);
    }
    if blocked {
        return Err(RejectCode::Conflict);
    }
    Ok(reads)
}

/// Validate resized bodies at their resolved final positions, including terrain writes.
pub(crate) fn validate_bodies(
    store: &SectionStore,
    entities: &EntityStore,
    registry: &BlockStateRegistry,
    edits: &[BlockEdit],
    moves: &[EntityMove],
    components: &[torse_world::ComponentEdit],
    lifecycle: Option<&torse_world::EntityLifecycle>,
) -> Result<Vec<(BlockPos, TerrainQuery)>, RejectCode> {
    let view = CollisionView {
        store,
        registry,
        overlay: edits.iter().map(|e| (e.pos, e.state)).collect(),
        reads: RefCell::new(BTreeMap::new()),
        unknown: RefCell::new(false),
    };
    let mut targets = Vec::new();
    for edit in components
        .iter()
        .filter(|e| e.schema == torse_world::collider_schema())
    {
        let entity = entities
            .handle_of(edit.id)
            .and_then(|h| entities.get(h))
            .ok_or(RejectCode::MissingTarget)?;
        let Some(half) = crate::body::of(entity, components, crate::player::WALKER_HALF)? else {
            continue;
        };
        let position = moves
            .iter()
            .find(|m| m.id == edit.id)
            .map_or(entity.position, |m| Position::from_bits(m.position_bits));
        targets.push((position, half));
    }
    for spawned in lifecycle.into_iter().flat_map(|l| &l.spawned) {
        if let Some(fields) = spawned.components.get(&torse_world::collider_schema())
            && let Some(half) = torse_world::EntityCollider::from_fields(fields)
                .map_err(|_| RejectCode::InvalidData)?
                .half()
        {
            targets.push((Position::from_bits(spawned.position_bits), half));
        }
    }
    for (position, half) in targets {
        let center = bounded_center([position.x, position.y, position.z], half)
            .ok_or(RejectCode::InvalidData)?;
        let blocked = collides(&view, &Aabb::from_center(center, half));
        if *view.unknown.borrow() {
            return Err(RejectCode::NotLoaded);
        }
        if blocked {
            return Err(RejectCode::Conflict);
        }
    }
    Ok(view.reads.into_inner().into_iter().collect())
}

struct CollisionView<'a> {
    store: &'a SectionStore,
    registry: &'a BlockStateRegistry,
    overlay: BTreeMap<BlockPos, torse_world::BlockStateId>,
    reads: RefCell<BTreeMap<BlockPos, TerrainQuery>>,
    unknown: RefCell<bool>,
}

impl CollisionSource for CollisionView<'_> {
    fn shape_reach(&self) -> ([u8; 3], [u8; 3]) {
        self.registry.collision_reach()
    }
    fn collision(&self, pos: BlockPos) -> CollisionCell {
        // World bounds constrain stored blocks. The immutable build ceiling
        // does not cap entity motion: bodies may stand on its highest layer
        // or fall back into loaded terrain. No mutable cell exists up here.
        if pos.y >= self.store.bounds().max_y {
            return CollisionCell::Empty;
        }
        let observed = self.store.query(pos);
        self.reads.borrow_mut().insert(pos, observed);
        match observed {
            TerrainQuery::Ready(original) => {
                let state = self.overlay.get(&pos).copied().unwrap_or(original);
                if state.is_empty() {
                    return CollisionCell::Empty;
                }
                // Unknown states stay unknown: treating them as empty would
                // let entities walk into cells the registry cannot validate.
                if self.registry.definition(state).is_none() {
                    *self.unknown.borrow_mut() = true;
                    return CollisionCell::NotLoaded;
                }
                // The per-state collision shape follows state values, so an
                // upper slab blocks its own half and a lower one blocks the
                // other.
                match self.registry.collision_shape(state) {
                    None => CollisionCell::Empty,
                    Some(shape) => CollisionCell::Solid(boxes(&shape)),
                }
            }
            TerrainQuery::NotLoaded => CollisionCell::NotLoaded,
            TerrainQuery::OutOfBounds => CollisionCell::OutOfBounds,
        }
    }
}

pub fn boxes(shape: &Shape) -> Vec<LocalBox> {
    if let Shape::Compound(boxes) = shape {
        return boxes
            .iter()
            .map(|bounds| LocalBox {
                min: bounds.min.map(f64::from),
                max: bounds.max.map(f64::from),
            })
            .collect();
    }
    // Corner/stair collision uses the same conservative enclosing cube as the
    // M1 reference scenario. State-dependent multi-box geometry is deferred.
    vec![match shape {
        Shape::Model(_) | Shape::Compound(_) => return Vec::new(),
        Shape::FullCube | Shape::Corner | Shape::Stair | Shape::Cross => LocalBox::FULL_CUBE,
        Shape::LowerSlab => LocalBox {
            min: [0.0; 3],
            max: [1.0, 0.5, 1.0],
        },
        Shape::Panel => LocalBox {
            min: [0.45, 0.0, 0.0],
            max: [0.55, 1.0, 1.0],
        },
        Shape::Box(bounds) => LocalBox {
            min: bounds.min.map(f64::from),
            max: bounds.max.map(f64::from),
        },
    }]
}

#[cfg(test)]
mod compound_tests {
    use super::*;

    #[test]
    fn compound_collision_preserves_the_open_gap_between_parts() {
        struct Source(Vec<LocalBox>);
        impl torse_world_services::CollisionSource for Source {
            fn collision(&self, pos: BlockPos) -> torse_world_services::CollisionCell {
                if pos == BlockPos::new(0, 0, 0) {
                    torse_world_services::CollisionCell::Solid(self.0.clone())
                } else {
                    torse_world_services::CollisionCell::Empty
                }
            }
        }
        let shape = Shape::Compound(vec![
            torse_content::BoxBounds {
                min: [0.0; 3],
                max: [0.2, 1.0, 1.0],
            },
            torse_content::BoxBounds {
                min: [0.8, 0.0, 0.0],
                max: [1.0; 3],
            },
        ]);
        let source = Source(boxes(&shape));
        assert!(!collides(&source, &Aabb::from_center([0.5; 3], [0.1; 3])));
        assert!(collides(
            &source,
            &Aabb::from_center([0.1, 0.5, 0.5], [0.05; 3])
        ));
    }
}
