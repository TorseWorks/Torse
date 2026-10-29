//! Protected action compositions for physical item ingress and withdrawal.
use crate::{Catalog, drops, key, owner};
use std::sync::Arc;
use torse_actions::{ActionDescriptor, ActionKind, ActionPhase, HandlerBinding, PermissionRule};
use torse_content::{ComponentField, ComponentFieldType, ComponentSchema, PersistencePolicy};
use torse_inventory::{Inventory, InventoryOperation, ItemStack, plan_inventory};
use torse_sim::action::{
    ActionHandler, ActionInvocation, ActionOutput, ActionRequest, ActionTarget, ActionView,
    NativeHandler,
};
use torse_sim::{Transaction, durable::Call};
use torse_types::{EntityId, RejectCode, ReplicationDeclaration};
use torse_world::{ComponentFields, ComponentValue as Value, Position, TerrainQuery};

const NAMES: [&str; 7] = [
    "spawn_harvest",
    "pickup",
    "pickup_inventory",
    "pickup_drop",
    "throw",
    "throw_inventory",
    "throw_drop",
];
fn inventory_owner(name: &str) -> bool {
    matches!(name, "pickup_inventory" | "throw_inventory")
}
pub(crate) fn descriptors() -> Vec<ActionDescriptor> {
    NAMES
        .into_iter()
        .map(|name| ActionDescriptor {
            id: key(&format!("items:action/{name}")),
            api_version: 1,
            kind: ActionKind::Mutation,
            parameter_schema: key(&format!("items:schema/{name}")),
            result_schema: key("items:schema/result"),
            permission: PermissionRule {
                permission: Some(key(if name.starts_with("pickup") {
                    "items:permission/pickup"
                } else {
                    "items:permission/use"
                })),
            },
            target_constraints: vec![],
            phase: ActionPhase::Interaction,
            max_items: 4096,
            max_bytes: 65_536,
            handler: HandlerBinding {
                handler: key(&format!("items:handler/{name}")),
                owner_module: if inventory_owner(name) {
                    torse_inventory::owner()
                } else {
                    owner()
                },
            },
            commands: vec![],
        })
        .collect()
}
pub(crate) fn declarations() -> Vec<ComponentSchema> {
    let mut result = vec![drops::declaration()];
    for name in NAMES {
        let mut fields = vec![];
        if name != "spawn_harvest" {
            fields.push(("inventory", ComponentFieldType::EntityReference, true));
            fields.push((
                if name.starts_with("pickup") {
                    "drop"
                } else {
                    "slot"
                },
                if name.starts_with("pickup") {
                    ComponentFieldType::EntityReference
                } else {
                    ComponentFieldType::Unsigned32
                },
                true,
            ));
            if !name.starts_with("pickup") {
                fields.push(("supply_item", ComponentFieldType::ContentReference, false));
            }
            for axis in ["x", "y", "z"] {
                fields.push((axis, ComponentFieldType::Signed64, false));
            }
        }
        result.push(ComponentSchema {
            id: key(&format!("items:schema/{name}")),
            schema_version: 1,
            owner_module: owner(),
            fields: fields
                .into_iter()
                .map(|(name, field_type, required)| ComponentField {
                    id: key(&format!("items:field/{name}")),
                    field_type,
                    required,
                })
                .collect(),
            persistence: PersistencePolicy::Never,
            replication: ReplicationDeclaration::SERVER_PRIVATE,
            migration_binding: None,
        });
    }
    result
}
pub(crate) fn handlers(catalog: Arc<Catalog>) -> Vec<NativeHandler> {
    NAMES
        .into_iter()
        .map(|name| NativeHandler {
            handler: key(&format!("items:handler/{name}")),
            owner: if inventory_owner(name) {
                torse_inventory::owner()
            } else {
                owner()
            },
            implementation: Box::new(Handler {
                catalog: catalog.clone(),
                name,
            }),
        })
        .collect()
}
struct Handler {
    catalog: Arc<Catalog>,
    name: &'static str,
}
fn entity(fields: &ComponentFields, name: &str) -> Result<EntityId, RejectCode> {
    match fields.get(&key(&format!("items:field/{name}"))) {
        Some(Value::Entity(id)) => Ok(*id),
        _ => Err(RejectCode::InvalidData),
    }
}
fn parent(view: &ActionView<'_>, action: &str) -> Result<(), RejectCode> {
    if view.caller_action() == Some(&key(&format!("items:action/{action}"))) {
        Ok(())
    } else {
        Err(RejectCode::PermissionDenied)
    }
}
fn position(
    view: &ActionView<'_>,
    inventory: EntityId,
    parameters: &ComponentFields,
) -> Result<Position, RejectCode> {
    if view.actor().permits(&key("items:permission/host_position")) {
        let coordinates: Option<Vec<_>> = ["x", "y", "z"]
            .into_iter()
            .map(|axis| {
                if let Some(Value::I64(value)) =
                    parameters.get(&key(&format!("items:field/{axis}")))
                {
                    Some(*value as f64 / 1_000_000.0)
                } else {
                    None
                }
            })
            .collect();
        if let Some(values) = coordinates {
            if values
                .iter()
                .any(|v| v.abs() > torse_sim::movement::MAX_MOVEMENT_COORDINATE)
            {
                return Err(RejectCode::InvalidData);
            }
            return Ok(Position::new(values[0], values[1], values[2]));
        }
    }
    Ok(view.entity(inventory)?.1.position)
}
fn pickup_targets(
    view: &ActionView<'_>,
    parameters: &ComponentFields,
) -> Result<(EntityId, EntityId), RejectCode> {
    let inventory = entity(parameters, "inventory")?;
    if crate::supply::mode(view.entity(inventory)?.1) == crate::supply::SupplyMode::Infinite {
        return Err(RejectCode::Conflict);
    }
    let drop = entity(parameters, "drop")?;
    let from = position(view, inventory, parameters)?;
    let target = view.entity(drop)?.1.position;
    if !drops::near(from, target, drops::PICKUP_REACH) {
        return Err(RejectCode::PermissionDenied);
    }
    // Check the short reach segment, so adjacent rooms cannot collect through walls.
    let steps = 24;
    for step in 1..steps {
        let t = f64::from(step) / f64::from(steps);
        let cell = Position::new(
            from.x + (target.x - from.x) * t,
            from.y + (target.y - from.y) * t,
            from.z + (target.z - from.z) * t,
        )
        .checked_block()
        .ok_or(RejectCode::InvalidData)?;
        if !matches!(view.terrain(cell)?, TerrainQuery::Ready(state) if state.is_empty()) {
            return Err(RejectCode::Conflict);
        }
    }
    Ok((drop, inventory))
}
impl ActionHandler for Handler {
    fn plan(
        &self,
        view: &ActionView<'_>,
        request: &ActionRequest,
    ) -> Result<ActionOutput, RejectCode> {
        let mutation = |tx| Ok(ActionOutput::Mutation(Box::new(tx)));
        if self.name == "spawn_harvest" {
            parent(view, "harvest")?;
            let ActionTarget::Block(pos) = request.target else {
                return Err(RejectCode::InvalidData);
            };
            let state = view.terrain(pos)?.ready().ok_or(RejectCode::NotLoaded)?;
            let stack = self
                .catalog
                .states
                .get(&state)
                .ok_or(RejectCode::MissingTarget)?
                .clone();
            let mut tx = Transaction::new(request.transaction, owner(), request.ordering);
            tx.read_block(pos, state);
            drops::spawn(
                &mut tx,
                Position::new(pos.x as f64 + 0.5, pos.y as f64 + 0.5, pos.z as f64 + 0.5),
                stack,
                [0; 3],
            );
            return mutation(tx);
        }
        if self.name.starts_with("pickup") {
            let (drop, inventory) = pickup_targets(view, &request.parameters)?;
            match self.name {
                "pickup_inventory" => {
                    parent(view, "pickup")?;
                    return mutation(drops::receive(
                        view,
                        &self.catalog,
                        drop,
                        inventory,
                        request.transaction,
                        request.ordering,
                    )?);
                }
                "pickup_drop" => {
                    parent(view, "pickup")?;
                    return mutation(drops::take(
                        view,
                        &self.catalog,
                        drop,
                        inventory,
                        request.transaction,
                        request.ordering,
                    )?);
                }
                _ => {}
            }
            if request.target != ActionTarget::Entity(drop) {
                return Err(RejectCode::InvalidData);
            }
            return Ok(ActionOutput::Invoke(vec![
                ActionInvocation {
                    action: key("items:action/pickup_inventory"),
                    api_version: 1,
                    target: ActionTarget::Entity(inventory),
                    parameters: request.parameters.clone(),
                },
                ActionInvocation {
                    action: key("items:action/pickup_drop"),
                    api_version: 1,
                    target: ActionTarget::Entity(drop),
                    parameters: request.parameters.clone(),
                },
            ]));
        }
        let inventory = entity(&request.parameters, "inventory")?;
        let Some(Value::U32(slot)) = request.parameters.get(&key("items:field/slot")) else {
            return Err(RejectCode::InvalidData);
        };
        if request.target != ActionTarget::Entity(inventory) {
            return Err(RejectCode::InvalidData);
        }
        let infinite =
            crate::supply::mode(view.entity(inventory)?.1) == crate::supply::SupplyMode::Infinite;
        let previous = view
            .component(inventory, &torse_inventory::schema())?
            .clone();
        let stack = if infinite {
            if !view.actor().permits(&key("items:permission/unlimited"))
                || self.name == "throw_inventory"
            {
                return Err(RejectCode::PermissionDenied);
            }
            let Some(Value::Content(item)) =
                request.parameters.get(&key("items:field/supply_item"))
            else {
                return Err(RejectCode::InvalidData);
            };
            if !self.catalog.definitions.contains_key(item) {
                return Err(RejectCode::InvalidData);
            }
            ItemStack {
                item: item.clone(),
                count: 1,
            }
        } else {
            let stock = Inventory::from_fields(&previous, &self.catalog.stacks)?;
            let stack = stock
                .slots()
                .get(*slot as usize)
                .and_then(Option::as_ref)
                .ok_or(RejectCode::MissingTarget)?;
            ItemStack {
                item: stack.item.clone(),
                count: 1,
            }
        };
        if self.name == "throw_inventory" {
            parent(view, "throw")?;
            return mutation(
                plan_inventory(
                    view,
                    &self.catalog.stacks,
                    &[InventoryOperation::Consume {
                        target: inventory,
                        slot: *slot as usize,
                        stack,
                    }],
                    &[(inventory, previous)].into(),
                    request.transaction,
                    request.ordering,
                )?
                .into_parts()
                .0,
            );
        }
        if self.name == "throw_drop" {
            parent(view, "throw")?;
            let at = position(view, inventory, &request.parameters)?;
            let mut tx = Transaction::new(request.transaction, owner(), request.ordering);
            drops::spawn(&mut tx, at, stack, [0, 1_500_000, 0]);
            return mutation(tx);
        }
        if infinite {
            return Ok(ActionOutput::Invoke(vec![
                ActionInvocation {
                    action: key("items:action/throw_drop"),
                    api_version: 1,
                    target: request.target,
                    parameters: request.parameters.clone(),
                },
                ActionInvocation {
                    action: key("items:action/supply_guard"),
                    api_version: 1,
                    target: request.target,
                    parameters: ComponentFields::new(),
                },
            ]));
        }
        Ok(ActionOutput::Invoke(
            ["throw_inventory", "throw_drop"]
                .map(|name| ActionInvocation {
                    action: key(&format!("items:action/{name}")),
                    api_version: 1,
                    target: request.target,
                    parameters: request.parameters.clone(),
                })
                .to_vec(),
        ))
    }
}

pub fn pickup_call(inventory: EntityId, drop: EntityId) -> Call {
    Call {
        action: key("items:action/pickup"),
        version: 1,
        target: ActionTarget::Entity(drop),
        parameters: [
            (key("items:field/inventory"), Value::Entity(inventory)),
            (key("items:field/drop"), Value::Entity(drop)),
        ]
        .into(),
    }
}
pub fn throw_call(inventory: EntityId, slot: u32) -> Call {
    Call {
        action: key("items:action/throw"),
        version: 1,
        target: ActionTarget::Entity(inventory),
        parameters: [
            (key("items:field/inventory"), Value::Entity(inventory)),
            (key("items:field/slot"), Value::U32(slot)),
        ]
        .into(),
    }
}
/// Only trusted local hosts grant host_position; remote parameters cannot move a player.
pub fn with_position(mut call: Call, at: Position) -> Call {
    for (axis, value) in ["x", "y", "z"].into_iter().zip([at.x, at.y, at.z]) {
        call.parameters.insert(
            key(&format!("items:field/{axis}")),
            Value::I64((value * 1_000_000.0) as i64),
        );
    }
    call
}
