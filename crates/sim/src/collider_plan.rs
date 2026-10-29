//! Owner-authored plans that enable or disable one entity body.
//! The result is unpublished and carries the physics module as its source.
use torse_types::{EntityId, RejectCode, TransactionId};
use torse_world::{ComponentEdit, EntityCollider};

use crate::OrderingKey;
use crate::action::{ActionTarget, ActorContext};
use crate::authority::WorldAuthority;
use crate::transaction::Transaction;

pub(crate) fn plan(
    world: &WorldAuthority,
    actor: &ActorContext,
    entity: EntityId,
    expected: EntityCollider,
    requested: EntityCollider,
    transaction: TransactionId,
    ordering: OrderingKey,
) -> Result<Transaction, RejectCode> {
    if !actor.permits(&torse_world::collider_permission())
        || !actor.allows(ActionTarget::Entity(entity))
    {
        return Err(RejectCode::PermissionDenied);
    }
    let handle = world
        .entities()
        .handle_of(entity)
        .ok_or(RejectCode::MissingTarget)?;
    let stored = world
        .entities()
        .get(handle)
        .ok_or(RejectCode::MissingTarget)?;
    if stored.id != entity
        || stored
            .components
            .component(&torse_world::block_instance_schema())
            .is_some()
    {
        return Err(RejectCode::PermissionDenied);
    }
    let current = EntityCollider::of(stored)
        .map_err(|_| RejectCode::InvalidData)?
        .ok_or(RejectCode::MissingTarget)?;
    if current != expected {
        return Err(RejectCode::Conflict);
    }
    // `EntityCollider::Box` is publicly constructible, so reject an invalid
    // request here instead of returning a plan that preparation must later
    // discard for schema validation.
    EntityCollider::from_fields(&requested.fields()).map_err(|_| RejectCode::InvalidData)?;
    let mut tx = Transaction::new(transaction, torse_world::collider_owner(), ordering);
    if let Some(half) = requested.half() {
        for (pos, query) in
            crate::movement::observe_body(world.store(), world.registry(), stored.position, half)?
        {
            match query {
                torse_world::TerrainQuery::Ready(state) => {
                    tx.read_block(pos, state);
                }
                torse_world::TerrainQuery::Exterior => {}
                torse_world::TerrainQuery::NotLoaded | torse_world::TerrainQuery::OutOfBounds => {
                    return Err(RejectCode::NotLoaded);
                }
            }
        }
    }
    tx.components.push(ComponentEdit {
        id: entity,
        schema: torse_world::collider_schema(),
        previous: Some(expected.fields()),
        fields: Some(requested.fields()),
    });
    Ok(tx)
}
