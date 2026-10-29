//! Persistent item bodies, terrain physics, bounded merging and pickup plans.
use crate::{Catalog, key, owner};
use std::collections::BTreeSet;
use torse_content::{ComponentField, ComponentFieldType, ComponentSchema, PersistencePolicy};
use torse_inventory::{Inventory, InventoryOperation, ItemStack, plan_inventory};
use torse_sim::action::{ActionTarget, ActionView, ActorContext};
use torse_sim::{OrderingKey, Phase, Transaction, TransactionGroup, WorldAuthority};
use torse_types::{
    ContentKey, EntityId, RejectCode, ReplicationDeclaration, ReplicationVisibility, Tick,
    TransactionId,
};
use torse_world::{
    ComponentEdit, ComponentFields, ComponentValue as Value, Entity, EntityCollider, Position,
};

pub const HALF: f64 = 0.125;
pub const PICKUP_REACH: f64 = 3.0;
pub const MAX_STEP_BODIES: usize = 64;
const STEP: f64 = 0.05;

fn leave_filled_cell(
    world: &WorldAuthority,
    entity: &Entity,
    id: TransactionId,
    ordering: OrderingKey,
) -> Option<Transaction> {
    // Only initial inert bodies need this: terrain may have been filled again
    // between the harvest and the first collider activation.
    if torse_sim::movement::observe_body(
        world.store(),
        world.registry(),
        entity.position,
        [HALF; 3],
    ) != Err(RejectCode::Conflict)
    {
        return None;
    }
    let mut previous = Vec::new();
    for x in [
        (entity.position.x - HALF).floor() as i64,
        (entity.position.x + HALF).floor() as i64,
    ] {
        for y in [
            (entity.position.y - HALF).floor() as i64,
            (entity.position.y + HALF).floor() as i64,
        ] {
            for z in [
                (entity.position.z - HALF).floor() as i64,
                (entity.position.z + HALF).floor() as i64,
            ] {
                let pos = torse_types::BlockPos::new(x, y, z);
                let state = world.store().query(pos).ready()?;
                if !previous.iter().any(|(p, _)| *p == pos) {
                    previous.push((pos, state));
                }
            }
        }
    }
    let actor = ActorContext::new(owner())
        .target(ActionTarget::Entity(entity.id))
        .permission(torse_sim::action::relocate_permission());
    for [dx, dy, dz] in [
        [0., 1., 0.],
        [-1., 0., 0.],
        [1., 0., 0.],
        [0., 0., -1.],
        [0., 0., 1.],
        [0., -1., 0.],
    ] {
        let destination = Position::new(
            entity.position.x + dx,
            entity.position.y + dy,
            entity.position.z + dz,
        );
        let Ok(reads) = torse_sim::movement::observe_body(
            world.store(),
            world.registry(),
            destination,
            [HALF; 3],
        ) else {
            continue;
        };
        if reads.iter().any(|(_, query)| query.ready().is_none()) {
            continue;
        }
        let Ok(mut tx) = actor.view(world).plan_relocation(
            entity.id,
            entity.position,
            destination,
            torse_sim::DestinationPolicy::Clear,
            id,
            ordering,
        ) else {
            continue;
        };
        for (pos, state) in &previous {
            tx.read_block(*pos, *state);
        }
        for (pos, query) in reads {
            tx.read_block(pos, query.ready()?);
        }
        return Some(tx);
    }
    None
}

pub fn schema() -> ContentKey {
    key("items:schema/drop")
}
fn field(name: &str) -> ContentKey {
    key(&format!("items:drop/{name}"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropState {
    pub stack: ItemStack,
    /// Integer microunits per second; no rendering animation affects this state.
    pub velocity: [i64; 3],
}
impl DropState {
    pub fn fields(&self) -> ComponentFields {
        [
            (field("item"), Value::Content(self.stack.item.clone())),
            (field("count"), Value::U32(self.stack.count)),
            (field("vx"), Value::I64(self.velocity[0])),
            (field("vy"), Value::I64(self.velocity[1])),
            (field("vz"), Value::I64(self.velocity[2])),
        ]
        .into()
    }
    pub fn read(entity: &Entity, catalog: &Catalog) -> Result<Self, RejectCode> {
        Self::from_fields(
            entity
                .components
                .component(&schema())
                .ok_or(RejectCode::MissingTarget)?,
            catalog,
        )
    }
    pub fn from_fields(fields: &ComponentFields, catalog: &Catalog) -> Result<Self, RejectCode> {
        let Some(Value::Content(item)) = fields.get(&field("item")) else {
            return Err(RejectCode::InvalidData);
        };
        let Some(Value::U32(count)) = fields.get(&field("count")) else {
            return Err(RejectCode::InvalidData);
        };
        let mut velocity = [0; 3];
        for (axis, name) in ["vx", "vy", "vz"].iter().enumerate() {
            let Some(Value::I64(value)) = fields.get(&field(name)) else {
                return Err(RejectCode::InvalidData);
            };
            if !(-64_000_000..=64_000_000).contains(value) {
                return Err(RejectCode::InvalidData);
            }
            velocity[axis] = *value;
        }
        if fields.len() != 5
            || *count == 0
            || *count
                > catalog
                    .definitions
                    .get(item)
                    .ok_or(RejectCode::InvalidData)?
                    .limit
        {
            return Err(RejectCode::InvalidData);
        }
        Ok(Self {
            stack: ItemStack {
                item: item.clone(),
                count: *count,
            },
            velocity,
        })
    }
}

pub(crate) fn declaration() -> ComponentSchema {
    ComponentSchema {
        id: schema(),
        schema_version: 1,
        owner_module: owner(),
        fields: [
            ("item", ComponentFieldType::ContentReference),
            ("count", ComponentFieldType::Unsigned32),
            ("vx", ComponentFieldType::Signed64),
            ("vy", ComponentFieldType::Signed64),
            ("vz", ComponentFieldType::Signed64),
        ]
        .into_iter()
        .map(|(name, field_type)| ComponentField {
            id: field(name),
            field_type,
            required: true,
        })
        .collect(),
        persistence: PersistencePolicy::Always,
        replication: ReplicationDeclaration {
            visibility: ReplicationVisibility::Public,
            ..ReplicationDeclaration::SERVER_PRIVATE
        },
        migration_binding: None,
    }
}

pub(crate) fn spawn(
    tx: &mut Transaction,
    position: Position,
    stack: ItemStack,
    velocity: [i64; 3],
) {
    // A harvest group removes its solid cell in a different member. Start inert
    // so independent child validation can run; the first physics step enables
    // the body against the newly committed terrain before applying gravity.
    tx.spawn_entity(
        position,
        [
            (schema(), DropState { stack, velocity }.fields()),
            (
                torse_world::collider_schema(),
                EntityCollider::Disabled.fields(),
            ),
        ]
        .into(),
    );
}

pub(crate) fn insertion(
    view: &ActionView<'_>,
    catalog: &Catalog,
    inventory: EntityId,
    stack: &ItemStack,
) -> Result<(Vec<InventoryOperation>, ComponentFields, u32), RejectCode> {
    let previous = view
        .component(inventory, &torse_inventory::schema())?
        .clone();
    let slots = Inventory::from_fields(&previous, &catalog.stacks)?;
    let limit = catalog
        .definitions
        .get(&stack.item)
        .ok_or(RejectCode::InvalidData)?
        .limit;
    let mut remaining = stack.count;
    let mut operations = Vec::new();
    for empty in [false, true] {
        for (slot, old) in slots.slots().iter().enumerate() {
            let capacity = match old {
                Some(old) if !empty && old.item == stack.item => limit - old.count,
                None if empty => limit,
                _ => 0,
            };
            let count = remaining.min(capacity);
            if count != 0 {
                operations.push(InventoryOperation::Insert {
                    target: inventory,
                    slot,
                    stack: ItemStack {
                        item: stack.item.clone(),
                        count,
                    },
                });
                remaining -= count;
            }
        }
    }
    if remaining == stack.count {
        return Err(RejectCode::Conflict);
    }
    Ok((operations, previous, stack.count - remaining))
}

fn replace(tx: &mut Transaction, entity: &Entity, state: &DropState) {
    tx.components.push(ComponentEdit {
        id: entity.id,
        schema: schema(),
        previous: entity.components.component(&schema()).cloned(),
        fields: Some(state.fields()),
    });
}

pub(crate) fn take(
    view: &ActionView<'_>,
    catalog: &Catalog,
    drop: EntityId,
    inventory: EntityId,
    id: TransactionId,
    ordering: OrderingKey,
) -> Result<Transaction, RejectCode> {
    let (handle, entity) = view.entity(drop)?;
    let mut state = DropState::read(entity, catalog)?;
    let (_, _, taken) = insertion(view, catalog, inventory, &state.stack)?;
    let mut tx = Transaction::new(id, owner(), ordering);
    state.stack.count -= taken;
    if state.stack.count == 0 {
        tx.despawn_entity(handle, entity);
    } else {
        replace(&mut tx, entity, &state);
        tx.move_entity(handle, entity.id, entity.position, [0.0; 3], [0.25; 3]);
    }
    Ok(tx)
}

pub(crate) fn receive(
    view: &ActionView<'_>,
    catalog: &Catalog,
    drop: EntityId,
    inventory: EntityId,
    id: TransactionId,
    ordering: OrderingKey,
) -> Result<Transaction, RejectCode> {
    let (_, entity) = view.entity(drop)?;
    let state = DropState::read(entity, catalog)?;
    let (operations, previous, _) = insertion(view, catalog, inventory, &state.stack)?;
    Ok(plan_inventory(
        view,
        &catalog.stacks,
        &operations,
        &[(inventory, previous)].into(),
        id,
        ordering,
    )?
    .into_parts()
    .0)
}

pub fn near(left: Position, right: Position, distance: f64) -> bool {
    left.is_finite()
        && right.is_finite()
        && (left.x - right.x).powi(2) + (left.y - right.y).powi(2) + (left.z - right.z).powi(2)
            <= distance * distance
}

/// Pure, bounded plans. The host commits them in order through its ordinary journal.
pub fn plans(world: &WorldAuthority, catalog: &Catalog, tick: Tick) -> Vec<TransactionGroup> {
    let count = world.entities().len();
    if count == 0 {
        return Vec::new();
    }
    let offset = (tick.0 as usize).wrapping_mul(MAX_STEP_BODIES) % count;
    let candidates: Vec<_> = world
        .entities()
        .iter()
        .skip(offset)
        .chain(world.entities().iter())
        .take(count.min(MAX_STEP_BODIES))
        .filter(|(_, e)| e.components.component(&schema()).is_some())
        .collect();
    let mut used = BTreeSet::new();
    let mut groups = Vec::new();
    for (handle, entity) in candidates {
        if !used.insert(entity.id) {
            continue;
        }
        let Ok(state) = DropState::read(entity, catalog) else {
            continue;
        };
        let ordering = OrderingKey {
            phase: Phase::Simulation,
            rule_priority: 10,
            stable_task_key: entity.id.0 as u64,
            local_sequence: 0,
        };
        let id = TransactionId((u128::from(tick.0) << 64) | entity.id.0 as u64 as u128);
        let Some(section) = entity.position.section(world.store().side()) else {
            continue;
        };
        let mut merged = false;
        'neighbors: for dy in -1..=1 {
            for dz in -1..=1 {
                for dx in -1..=1 {
                    let pos = torse_types::SectionPos::new(
                        section.x + dx,
                        section.y + dy,
                        section.z + dz,
                    );
                    for other_handle in world
                        .entities()
                        .entities_in_section(pos)
                        .take(MAX_STEP_BODIES)
                    {
                        let Some(other) = world.entities().get(other_handle) else {
                            continue;
                        };
                        if other.id <= entity.id
                            || used.contains(&other.id)
                            || !near(entity.position, other.position, 0.6)
                        {
                            continue;
                        }
                        let Ok(mut target) = DropState::read(other, catalog) else {
                            continue;
                        };
                        if target.stack.item != state.stack.item {
                            continue;
                        }
                        let limit = catalog.definitions[&state.stack.item].limit;
                        let amount = state.stack.count.min(limit - target.stack.count);
                        if amount == 0 {
                            continue;
                        }
                        let middle = Position::new(
                            (entity.position.x + other.position.x) * 0.5,
                            (entity.position.y + other.position.y) * 0.5,
                            (entity.position.z + other.position.z) * 0.5,
                        );
                        let Ok(observed) = torse_sim::movement::observe_body(
                            world.store(),
                            world.registry(),
                            middle,
                            [HALF; 3],
                        ) else {
                            continue;
                        };
                        let mut tx = Transaction::new(id, owner(), ordering);
                        for (pos, query) in observed {
                            if let Some(state) = query.ready() {
                                tx.read_block(pos, state);
                            }
                        }
                        target.stack.count += amount;
                        replace(&mut tx, other, &target);
                        tx.move_entity(other_handle, other.id, other.position, [0.0; 3], [0.25; 3]);
                        if amount == state.stack.count {
                            tx.despawn_entity(handle, entity);
                        } else {
                            let mut remaining = state.clone();
                            remaining.stack.count -= amount;
                            replace(&mut tx, entity, &remaining);
                            tx.move_entity(handle, entity.id, entity.position, [0.0; 3], [0.25; 3]);
                        }
                        if let Ok(group) = TransactionGroup::new(id, ordering, vec![tx]) {
                            used.insert(other.id);
                            groups.push(group);
                            merged = true;
                        }
                        break 'neighbors;
                    }
                }
            }
        }
        if merged {
            continue;
        }
        if matches!(
            EntityCollider::of(entity),
            Ok(Some(EntityCollider::Disabled))
        ) {
            let actor = ActorContext::new(owner())
                .target(ActionTarget::Entity(entity.id))
                .permission(torse_world::collider_permission());
            if let Ok(tx) = actor.view(world).plan_collider_transition(
                entity.id,
                EntityCollider::Disabled,
                EntityCollider::Box { half: [125_000; 3] },
                id,
                ordering,
            ) && let Ok(group) = TransactionGroup::new(id, ordering, vec![tx])
            {
                groups.push(group);
            } else if let Some(tx) = leave_filled_cell(world, entity, id, ordering)
                && let Ok(group) = TransactionGroup::new(id, ordering, vec![tx])
            {
                groups.push(group);
            }
            continue;
        }
        let mut next = state.clone();
        next.velocity[1] = (next.velocity[1] - 1_000_000).max(-32_000_000);
        let delta = next.velocity.map(|v| v as f64 / 1_000_000.0 * STEP);
        let mut tx = Transaction::new(id, owner(), ordering);
        tx.move_entity(handle, entity.id, entity.position, delta, [0.25; 3]);
        let Ok(prepared) = world.prepare(&tx) else {
            continue;
        };
        if prepared
            .collision_reads()
            .iter()
            .any(|(_, q)| matches!(q, torse_world::TerrainQuery::NotLoaded))
        {
            continue;
        }
        for (pos, query) in prepared.collision_reads() {
            if let Some(state) = query.ready() {
                tx.read_block(*pos, state);
            }
        }
        let Some(movement) = prepared.entity_moves().first() else {
            continue;
        };
        let settled = Position::from_bits(movement.position_bits);
        let actual = [
            settled.x - entity.position.x,
            settled.y - entity.position.y,
            settled.z - entity.position.z,
        ];
        for axis in 0..3 {
            if (actual[axis] - delta[axis]).abs() > 1e-8 {
                next.velocity[axis] = 0;
            } else if axis != 1 {
                next.velocity[axis] = next.velocity[axis] * 4 / 5;
            }
        }
        if settled.to_bits() == entity.position.to_bits() && next == state {
            continue;
        }
        replace(&mut tx, entity, &next);
        if let Ok(group) = TransactionGroup::new(id, ordering, vec![tx]) {
            groups.push(group);
        }
    }
    groups
}
