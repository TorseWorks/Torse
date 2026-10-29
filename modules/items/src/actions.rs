use super::*;
use std::sync::Arc;
use torse_actions::{ActionDescriptor, ActionKind, ActionPhase, HandlerBinding, PermissionRule};
use torse_content::{
    ComponentField, ComponentFieldType, ComponentSchema, ContentPack, PersistencePolicy,
};
use torse_inventory::{InventoryOperation, plan_inventory};
use torse_sim::action::{
    ActionHandler, ActionInvocation, ActionOutput, ActionRequest, ActionTarget, ActionView,
    NativeHandler, register_action_module,
};
use torse_types::ReplicationDeclaration;
use torse_world::{ComponentFields, ComponentValue, TerrainQuery};

const NAMES: [&str; 4] = ["place", "harvest", "settle", "swap"];
/// Stock-controlled actors may only change blocks through atomic item actions.
/// Actors without the item permission retain the host's explicit creative policy.
pub fn block_handlers() -> Vec<NativeHandler> {
    torse_sim::block_actions::handlers()
        .into_iter()
        .map(|mut handler| {
            handler.implementation = Box::new(BlockGuard {
                inner: handler.implementation,
            });
            handler
        })
        .collect()
}
struct BlockGuard {
    inner: Box<dyn ActionHandler>,
}
impl ActionHandler for BlockGuard {
    fn plan(
        &self,
        view: &ActionView<'_>,
        request: &ActionRequest,
    ) -> Result<ActionOutput, RejectCode> {
        if view.actor().permits(&key("items:permission/use"))
            && view.caller_action()
                != Some(&key(if request.action == key("world:action/place") {
                    "items:action/place"
                } else {
                    "items:action/harvest"
                }))
        {
            return Err(RejectCode::PermissionDenied);
        }
        self.inner.plan(view, request)
    }
}
pub fn descriptors() -> Vec<ActionDescriptor> {
    let mut result: Vec<_> = NAMES
        .into_iter()
        .map(|name| ActionDescriptor {
            id: key(&format!("items:action/{name}")),
            api_version: 1,
            kind: ActionKind::Mutation,
            parameter_schema: key(&format!("items:schema/{name}")),
            result_schema: key("items:schema/result"),
            permission: PermissionRule {
                permission: Some(key(if name == "swap" {
                    "inventory:permission/swap"
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
                owner_module: if matches!(name, "settle" | "swap") {
                    torse_inventory::owner()
                } else {
                    owner()
                },
            },
            commands: vec![],
        })
        .collect();
    result.extend(crate::drop_actions::descriptors());
    result.extend(crate::supply::descriptors());
    result
}
pub fn register_content(pack: &mut ContentPack) {
    let mut components: Vec<_> = NAMES
        .into_iter()
        .chain(["result"])
        .map(|name| {
            let fields: Vec<(&str, ComponentFieldType)> = match name {
                "swap" => vec![
                    ("slot", ComponentFieldType::Unsigned32),
                    ("other", ComponentFieldType::Unsigned32),
                ],
                "result" => vec![],
                _ => vec![
                    ("inventory", ComponentFieldType::EntityReference),
                    ("slot", ComponentFieldType::Unsigned32),
                    ("half", ComponentFieldType::Unsigned32),
                    ("place", ComponentFieldType::Bool),
                ],
            };
            ComponentSchema {
                id: key(&format!("items:schema/{name}")),
                schema_version: 1,
                owner_module: owner(),
                fields: fields
                    .into_iter()
                    .map(|(id, field_type)| ComponentField {
                        id: key(&format!("items:field/{id}")),
                        field_type,
                        required: true,
                    })
                    .collect(),
                persistence: PersistencePolicy::Never,
                replication: ReplicationDeclaration::SERVER_PRIVATE,
                migration_binding: None,
            }
        })
        .collect();
    components.extend(crate::drop_actions::declarations());
    components.extend(crate::supply::declarations());
    for component in &mut components {
        if ["place", "harvest", "settle"]
            .iter()
            .any(|name| component.id == key(&format!("items:schema/{name}")))
        {
            component.fields.push(ComponentField {
                id: key("items:field/supply_item"),
                field_type: ComponentFieldType::ContentReference,
                required: false,
            });
        }
    }
    register_action_module(
        pack,
        owner(),
        key("items:state/actions"),
        components,
        descriptors(),
        vec![],
    );
}
pub fn handlers(catalog: Catalog) -> Vec<NativeHandler> {
    let catalog = Arc::new(catalog);
    let mut result: Vec<_> = NAMES
        .into_iter()
        .map(|name| NativeHandler {
            handler: key(&format!("items:handler/{name}")),
            owner: if matches!(name, "settle" | "swap") {
                torse_inventory::owner()
            } else {
                owner()
            },
            implementation: Box::new(Handler {
                catalog: catalog.clone(),
                name,
            }),
        })
        .collect();
    result.extend(crate::drop_actions::handlers(catalog));
    result.extend(crate::supply::handlers());
    result
}
fn number(fields: &ComponentFields, name: &str) -> Result<usize, RejectCode> {
    match fields.get(&key(&format!("items:field/{name}"))) {
        Some(ComponentValue::U32(value)) => Ok(*value as usize),
        _ => Err(RejectCode::InvalidData),
    }
}
struct Handler {
    catalog: Arc<Catalog>,
    name: &'static str,
}
impl ActionHandler for Handler {
    fn plan(
        &self,
        view: &ActionView<'_>,
        request: &ActionRequest,
    ) -> Result<ActionOutput, RejectCode> {
        if self.name == "swap" {
            let ActionTarget::Entity(target) = request.target else {
                return Err(RejectCode::InvalidData);
            };
            let fields = view.component(target, &torse_inventory::schema())?.clone();
            let plan = plan_inventory(
                view,
                &self.catalog.stacks,
                &[InventoryOperation::Swap {
                    left: target,
                    left_slot: number(&request.parameters, "slot")?,
                    right: target,
                    right_slot: number(&request.parameters, "other")?,
                }],
                &[(target, fields)].into(),
                request.transaction,
                request.ordering,
            )?;
            return Ok(ActionOutput::Mutation(Box::new(plan.into_parts().0)));
        }
        let ActionTarget::Block(pos) = request.target else {
            return Err(RejectCode::InvalidData);
        };
        let Some(ComponentValue::Entity(target)) =
            request.parameters.get(&key("items:field/inventory"))
        else {
            return Err(RejectCode::InvalidData);
        };
        if self.name != "settle"
            && crate::supply::mode(view.entity(*target)?.1) == crate::supply::SupplyMode::Infinite
        {
            if !view.actor().permits(&key("items:permission/unlimited")) {
                return Err(RejectCode::PermissionDenied);
            }
            let mut parameters = ComponentFields::new();
            if self.name == "place" {
                let Some(ComponentValue::Content(item)) =
                    request.parameters.get(&key("items:field/supply_item"))
                else {
                    return Err(RejectCode::InvalidData);
                };
                let block = self.catalog.block(item).ok_or(RejectCode::MissingTarget)?;
                let half = number(&request.parameters, "half")?;
                if half > 3 {
                    return Err(RejectCode::InvalidData);
                }
                parameters.insert(
                    key("world:field/block"),
                    ComponentValue::Content(block.clone()),
                );
                parameters.insert(key("world:field/half"), ComponentValue::U32(half as u32));
            }
            return Ok(ActionOutput::Invoke(vec![
                ActionInvocation {
                    action: key(if self.name == "place" {
                        "world:action/place"
                    } else {
                        "world:action/break"
                    }),
                    api_version: 1,
                    target: request.target,
                    parameters,
                },
                ActionInvocation {
                    action: key("items:action/supply_guard"),
                    api_version: 1,
                    target: ActionTarget::Entity(*target),
                    parameters: ComponentFields::new(),
                },
            ]));
        }
        if self.name == "harvest" && self.catalog.world_drops {
            return Ok(ActionOutput::Invoke(vec![
                ActionInvocation {
                    action: key("world:action/break"),
                    api_version: 1,
                    target: request.target,
                    parameters: ComponentFields::new(),
                },
                ActionInvocation {
                    action: key("items:action/spawn_harvest"),
                    api_version: 1,
                    target: request.target,
                    parameters: ComponentFields::new(),
                },
            ]));
        }
        let Some(ComponentValue::Bool(place)) = request.parameters.get(&key("items:field/place"))
        else {
            return Err(RejectCode::InvalidData);
        };
        if self.name != "settle" && *place != (self.name == "place") {
            return Err(RejectCode::InvalidData);
        }
        if self.name == "settle"
            && view.caller_action()
                != Some(&key(if *place {
                    "items:action/place"
                } else {
                    "items:action/harvest"
                }))
        {
            return Err(RejectCode::PermissionDenied);
        }
        let previous = view.component(*target, &torse_inventory::schema())?.clone();
        let inventory = Inventory::from_fields(&previous, &self.catalog.stacks)?;
        let slot = number(&request.parameters, "slot")?;
        let mut parameters = ComponentFields::new();
        let operations = if *place {
            let stack = inventory
                .slots()
                .get(slot)
                .and_then(Option::as_ref)
                .ok_or(RejectCode::Conflict)?;
            let block = self
                .catalog
                .block(&stack.item)
                .ok_or(RejectCode::MissingTarget)?;
            parameters.insert(
                key("world:field/block"),
                ComponentValue::Content(block.clone()),
            );
            let half = number(&request.parameters, "half")?;
            if half > 3 {
                return Err(RejectCode::InvalidData);
            }
            parameters.insert(key("world:field/half"), ComponentValue::U32(half as u32));
            vec![InventoryOperation::Consume {
                target: *target,
                slot,
                stack: ItemStack {
                    item: stack.item.clone(),
                    count: 1,
                },
            }]
        } else {
            let state = match view.terrain(pos)? {
                TerrainQuery::Ready(state) if !state.is_empty() => state,
                TerrainQuery::NotLoaded => return Err(RejectCode::NotLoaded),
                _ => return Err(RejectCode::MissingTarget),
            };
            let stack = self
                .catalog
                .states
                .get(&state)
                .ok_or(RejectCode::MissingTarget)?;
            let limit = self.catalog.definitions[&stack.item].limit;
            let mut remaining = stack.count;
            let mut operations = vec![];
            // Fill compatible stacks before empty slots, without ever dropping excess.
            for empty in [false, true] {
                for (slot, existing) in inventory.slots().iter().enumerate() {
                    let capacity = match existing {
                        Some(other) if !empty && other.item == stack.item => limit - other.count,
                        None if empty => limit,
                        _ => 0,
                    };
                    let count = capacity.min(remaining);
                    if count != 0 {
                        operations.push(InventoryOperation::Insert {
                            target: *target,
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
            if remaining != 0 {
                return Err(RejectCode::Conflict);
            }
            operations
        };
        if self.name == "settle" {
            let plan = plan_inventory(
                view,
                &self.catalog.stacks,
                &operations,
                &[(*target, previous)].into(),
                request.transaction,
                request.ordering,
            )?;
            return Ok(ActionOutput::Mutation(Box::new(plan.into_parts().0)));
        }
        Ok(ActionOutput::Invoke(vec![
            ActionInvocation {
                action: key(if *place {
                    "world:action/place"
                } else {
                    "world:action/break"
                }),
                api_version: 1,
                target: request.target,
                parameters,
            },
            ActionInvocation {
                action: key("items:action/settle"),
                api_version: 1,
                target: request.target,
                parameters: request.parameters.clone(),
            },
        ]))
    }
}
