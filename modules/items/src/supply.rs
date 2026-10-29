//! Persistent player supply policy; presentation selection never changes stock.
use crate::{key, owner};
use torse_actions::{ActionDescriptor, ActionKind, ActionPhase, HandlerBinding, PermissionRule};
use torse_content::{ComponentField, ComponentFieldType, ComponentSchema, PersistencePolicy};
use torse_sim::action::{
    ActionHandler, ActionOutput, ActionRequest, ActionTarget, ActionView, NativeHandler,
};
use torse_sim::{Transaction, durable::Call};
use torse_types::{
    ContentKey, EntityId, RejectCode, ReplicationDeclaration, ReplicationVisibility,
};
use torse_world::{ComponentEdit, ComponentValue as Value, Entity};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SupplyMode {
    #[default]
    Finite,
    Infinite,
}
pub fn schema() -> ContentKey {
    key("items:schema/supply")
}
fn field() -> ContentKey {
    key("items:field/infinite")
}
pub fn mode(entity: &Entity) -> SupplyMode {
    if entity
        .components
        .component(&schema())
        .and_then(|fields| fields.get(&field()))
        == Some(&Value::Bool(true))
    {
        SupplyMode::Infinite
    } else {
        SupplyMode::Finite
    }
}
pub fn call(inventory: EntityId, mode: SupplyMode) -> Call {
    Call {
        action: key("items:action/supply"),
        version: 1,
        target: ActionTarget::Entity(inventory),
        parameters: [(field(), Value::Bool(mode == SupplyMode::Infinite))].into(),
    }
}
/// Add a catalog selection to a placement request; the server checks supply authority.
pub fn with_item(mut call: Call, item: ContentKey) -> Call {
    call.parameters
        .insert(key("items:field/supply_item"), Value::Content(item));
    call
}
pub(crate) fn declarations() -> Vec<ComponentSchema> {
    ["supply", "supply_guard"]
        .into_iter()
        .map(|name| ComponentSchema {
            id: key(&format!("items:schema/{name}")),
            schema_version: 1,
            owner_module: owner(),
            fields: if name == "supply" {
                vec![ComponentField {
                    id: field(),
                    field_type: ComponentFieldType::Bool,
                    required: true,
                }]
            } else {
                vec![]
            },
            persistence: if name == "supply" {
                PersistencePolicy::Always
            } else {
                PersistencePolicy::Never
            },
            replication: ReplicationDeclaration {
                visibility: ReplicationVisibility::Authorized,
                ..ReplicationDeclaration::SERVER_PRIVATE
            },
            migration_binding: None,
        })
        .collect()
}
pub(crate) fn descriptors() -> Vec<ActionDescriptor> {
    ["supply", "supply_guard"]
        .into_iter()
        .map(|name| ActionDescriptor {
            id: key(&format!("items:action/{name}")),
            api_version: 1,
            kind: ActionKind::Mutation,
            parameter_schema: key(&format!("items:schema/{name}")),
            result_schema: key("items:schema/result"),
            permission: PermissionRule {
                permission: Some(key(if name == "supply" {
                    "items:permission/configure_supply"
                } else {
                    "items:permission/unlimited"
                })),
            },
            target_constraints: vec![],
            phase: ActionPhase::Interaction,
            max_items: 16,
            max_bytes: 4096,
            handler: HandlerBinding {
                handler: key(&format!("items:handler/{name}")),
                owner_module: owner(),
            },
            commands: vec![],
        })
        .collect()
}
pub(crate) fn handlers() -> Vec<NativeHandler> {
    [false, true]
        .into_iter()
        .map(|guard| NativeHandler {
            handler: key(if guard {
                "items:handler/supply_guard"
            } else {
                "items:handler/supply"
            }),
            owner: owner(),
            implementation: Box::new(Handler { guard }),
        })
        .collect()
}
struct Handler {
    guard: bool,
}
impl ActionHandler for Handler {
    fn plan(
        &self,
        view: &ActionView<'_>,
        request: &ActionRequest,
    ) -> Result<ActionOutput, RejectCode> {
        let ActionTarget::Entity(id) = request.target else {
            return Err(RejectCode::InvalidData);
        };
        let entity = view.entity(id)?.1;
        // Supply belongs to an existing inventory; arbitrary entities cannot acquire it.
        view.component(id, &torse_inventory::schema())?;
        let previous = entity.components.component(&schema()).cloned();
        let fields = if self.guard {
            if !["place", "harvest", "throw"]
                .into_iter()
                .any(|name| view.caller_action() == Some(&key(&format!("items:action/{name}"))))
                || mode(entity) != SupplyMode::Infinite
            {
                return Err(RejectCode::PermissionDenied);
            }
            previous.clone().ok_or(RejectCode::InvalidData)?
        } else {
            let Some(Value::Bool(infinite)) = request.parameters.get(&field()) else {
                return Err(RejectCode::InvalidData);
            };
            if *infinite && !view.actor().permits(&key("items:permission/unlimited")) {
                return Err(RejectCode::PermissionDenied);
            }
            [(field(), Value::Bool(*infinite))].into()
        };
        let mut tx = Transaction::new(request.transaction, owner(), request.ordering);
        tx.components.push(ComponentEdit {
            id,
            schema: schema(),
            previous,
            fields: Some(fields),
        });
        Ok(ActionOutput::Mutation(Box::new(tx)))
    }
}
