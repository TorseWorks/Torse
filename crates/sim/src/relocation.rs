//! Exact relocation of one live entity.
//!
//! The plan is unpublished. Preparation writes the destination as an entity
//! move, so identity, components, and the spatial index stay on the normal
//! publication path. The request does not pass through movement sweep limits.

use torse_types::{BlockPos, EntityId, RejectCode, TransactionId};
use torse_world::{
    BlockStateRegistry, EntityCollider, EntityMove, EntityStore, Position, SectionStore,
    TerrainQuery,
};
use torse_world_services::CONTACT_EPSILON;

use crate::OrderingKey;
use crate::action::{ActionTarget, ActorContext};
use crate::authority::WorldAuthority;
use crate::movement::{MAX_COLLIDER_HALF, MAX_MOVEMENT_COORDINATE};
use crate::transaction::{DestinationPolicy, Relocation, Transaction};

/// One relocation per transaction. A larger batch is rejected.
const MAX_RELOCATIONS: usize = 1;

pub(crate) struct Resolved {
    pub moves: Vec<EntityMove>,
    pub reads: Vec<(BlockPos, TerrainQuery)>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn plan(
    world: &WorldAuthority,
    actor: &ActorContext,
    entity: EntityId,
    expected: Position,
    destination: Position,
    policy: DestinationPolicy,
    transaction: TransactionId,
    ordering: OrderingKey,
) -> Result<Transaction, RejectCode> {
    if !actor.permits(&crate::action::relocate_permission())
        || !actor.allows(ActionTarget::Entity(entity))
    {
        return Err(RejectCode::PermissionDenied);
    }
    let relocation = Relocation {
        id: entity,
        previous_bits: expected.to_bits(),
        destination_bits: destination.to_bits(),
        policy,
    };
    let resolved = resolve(
        world.store(),
        world.entities(),
        world.registry(),
        std::slice::from_ref(&relocation),
    )?;
    let mut tx = Transaction::new(transaction, crate::action::relocation_source(), ordering);
    for (pos, query) in resolved.reads {
        match query {
            TerrainQuery::Ready(state) => {
                tx.read_block(pos, state);
            }
            TerrainQuery::NotLoaded | TerrainQuery::OutOfBounds => {
                return Err(RejectCode::NotLoaded);
            }
        }
    }
    tx.relocations.push(relocation);
    Ok(tx)
}

pub(crate) fn resolve(
    store: &SectionStore,
    entities: &EntityStore,
    registry: &BlockStateRegistry,
    relocations: &[Relocation],
) -> Result<Resolved, RejectCode> {
    if relocations.len() > MAX_RELOCATIONS {
        return Err(RejectCode::QuotaExceeded);
    }
    let mut moves = Vec::with_capacity(relocations.len());
    let mut reads = Vec::new();
    for relocation in relocations {
        let handle = entities
            .handle_of(relocation.id)
            .ok_or(RejectCode::MissingTarget)?;
        let stored = entities.get(handle).ok_or(RejectCode::MissingTarget)?;
        if stored.id != relocation.id {
            return Err(RejectCode::MissingTarget);
        }
        if stored
            .components
            .component(&torse_world::block_instance_schema())
            .is_some()
        {
            return Err(RejectCode::PermissionDenied);
        }
        if stored.position.to_bits() != relocation.previous_bits {
            return Err(RejectCode::Conflict);
        }
        let destination = Position::from_bits(relocation.destination_bits);
        if !acceptable(destination, store.side())
            || destination.to_bits() == stored.position.to_bits()
        {
            return Err(RejectCode::InvalidData);
        }
        let collider = EntityCollider::of(stored).map_err(|_| RejectCode::InvalidData)?;
        reads.extend(observe(
            store,
            registry,
            destination,
            collider,
            relocation.policy,
        )?);
        moves.push(EntityMove {
            id: relocation.id,
            previous_bits: relocation.previous_bits,
            position_bits: destination.to_bits(),
        });
    }
    Ok(Resolved { moves, reads })
}

fn acceptable(position: Position, side: u32) -> bool {
    position.is_finite()
        && [position.x, position.y, position.z]
            .iter()
            .all(|value| value.abs() <= MAX_MOVEMENT_COORDINATE)
        && position.section(side).is_some()
}

/// Recheck the destination against all writes in the atomic publication.
pub(crate) fn validate_final(
    store: &SectionStore,
    entities: &EntityStore,
    registry: &BlockStateRegistry,
    relocations: &[Relocation],
    edits: &[torse_world::BlockEdit],
    components: &[torse_world::ComponentEdit],
) -> Result<Vec<(BlockPos, TerrainQuery)>, RejectCode> {
    let mut reads = Vec::new();
    for relocation in relocations {
        let entity = entities
            .handle_of(relocation.id)
            .and_then(|h| entities.get(h))
            .ok_or(RejectCode::MissingTarget)?;
        let schema = torse_world::collider_schema();
        let collider = components
            .iter()
            .find(|e| e.id == relocation.id && e.schema == schema)
            .map_or_else(
                || entity.components.component(&schema),
                |e| e.fields.as_ref(),
            )
            .map(EntityCollider::from_fields)
            .transpose()
            .map_err(|_| RejectCode::InvalidData)?;
        reads.extend(observe_with_edits(
            store,
            registry,
            Position::from_bits(relocation.destination_bits),
            collider,
            relocation.policy,
            edits,
        )?);
    }
    Ok(reads)
}

fn observe(
    store: &SectionStore,
    registry: &BlockStateRegistry,
    destination: Position,
    collider: Option<EntityCollider>,
    policy: DestinationPolicy,
) -> Result<Vec<(BlockPos, TerrainQuery)>, RejectCode> {
    observe_with_edits(store, registry, destination, collider, policy, &[])
}

fn observe_with_edits(
    store: &SectionStore,
    registry: &BlockStateRegistry,
    destination: Position,
    collider: Option<EntityCollider>,
    policy: DestinationPolicy,
    edits: &[torse_world::BlockEdit],
) -> Result<Vec<(BlockPos, TerrainQuery)>, RejectCode> {
    let Some(half) = collider.and_then(EntityCollider::half) else {
        let cell = destination.checked_block().ok_or(RejectCode::InvalidData)?;
        return one_cell(store, cell, policy, edits);
    };
    if half
        .iter()
        .any(|value| !value.is_finite() || *value <= 0.0 || *value > MAX_COLLIDER_HALF)
    {
        return Err(RejectCode::InvalidData);
    }
    match policy {
        DestinationPolicy::Clear => {
            crate::movement::observe_body_with_edits(store, registry, destination, half, edits)
        }
        DestinationPolicy::Resident => {
            let mut reads = Vec::new();
            for cell in covered(destination, half)? {
                match store.query(cell) {
                    TerrainQuery::Ready(state) => reads.push((cell, TerrainQuery::Ready(state))),
                    TerrainQuery::NotLoaded | TerrainQuery::OutOfBounds => {
                        return Err(RejectCode::NotLoaded);
                    }
                }
            }
            Ok(reads)
        }
    }
}

fn one_cell(
    store: &SectionStore,
    cell: BlockPos,
    policy: DestinationPolicy,
    edits: &[torse_world::BlockEdit],
) -> Result<Vec<(BlockPos, TerrainQuery)>, RejectCode> {
    match store.query(cell) {
        TerrainQuery::Ready(state) => {
            let final_state = edits
                .iter()
                .rev()
                .find(|e| e.pos == cell)
                .map_or(state, |e| e.state);
            if policy == DestinationPolicy::Resident || final_state.is_empty() {
                Ok(vec![(cell, TerrainQuery::Ready(state))])
            } else {
                Err(RejectCode::Conflict)
            }
        }
        TerrainQuery::NotLoaded | TerrainQuery::OutOfBounds => Err(RejectCode::NotLoaded),
    }
}

fn covered(position: Position, half: [f64; 3]) -> Result<Vec<BlockPos>, RejectCode> {
    let min = [
        position.x - half[0],
        position.y - half[1],
        position.z - half[2],
    ];
    let max = [
        position.x + half[0] - CONTACT_EPSILON,
        position.y + half[1] - CONTACT_EPSILON,
        position.z + half[2] - CONTACT_EPSILON,
    ];
    if min.iter().chain(max.iter()).any(|value| !value.is_finite()) {
        return Err(RejectCode::InvalidData);
    }
    let lo = [
        min[0].floor() as i64,
        min[1].floor() as i64,
        min[2].floor() as i64,
    ];
    let hi = [
        max[0].floor() as i64,
        max[1].floor() as i64,
        max[2].floor() as i64,
    ];
    let mut count = 1i64;
    for axis in 0..3 {
        count = count.saturating_mul(hi[axis].saturating_sub(lo[axis]).saturating_add(1));
    }
    if count <= 0 || count > 512 {
        return Err(RejectCode::QuotaExceeded);
    }
    let mut cells = Vec::with_capacity(count as usize);
    for y in lo[1]..=hi[1] {
        for z in lo[2]..=hi[2] {
            for x in lo[0]..=hi[0] {
                cells.push(BlockPos::new(x, y, z));
            }
        }
    }
    Ok(cells)
}
