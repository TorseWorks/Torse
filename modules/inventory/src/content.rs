//! Optional module metadata; none of these declarations enables a handler alone.

use torse_actions::{
    ActionDescriptor, ActionKind, ActionPhase, CommandBinding, HandlerBinding, PermissionRule,
};
use torse_content::{
    CapabilityDescriptor, ComponentField, ComponentFieldType, ComponentSchema, ContentPack,
    ExecutionContract, MethodDescriptor, PersistencePolicy,
};
use torse_sim::action::register_action_module;
use torse_types::{ContentKey, ReplicationDeclaration, ReplicationVisibility};

use crate::{MAX_SLOTS, key};

/// Stable module identity used by protected-component ownership checks.
pub fn owner() -> ContentKey {
    key("inventory:module")
}
/// Persistent container component identity.
pub fn schema() -> ContentKey {
    key("inventory:schema/container")
}

/// Public item-port query and transfer actions, with explicit permissions.
pub fn descriptors() -> Vec<ActionDescriptor> {
    ["inspect", "transfer"]
        .into_iter()
        .map(|name| ActionDescriptor {
            id: key(&format!("inventory:action/{name}")),
            api_version: 2,
            kind: if name == "inspect" {
                ActionKind::Query
            } else {
                ActionKind::Mutation
            },
            parameter_schema: key(&format!("inventory:schema/{name}")),
            result_schema: schema(),
            permission: PermissionRule {
                permission: Some(key(if name == "inspect" {
                    "inventory:permission/read"
                } else {
                    "inventory:permission/transfer"
                })),
            },
            target_constraints: vec![schema()],
            phase: ActionPhase::Interaction,
            max_items: 4096,
            max_bytes: 65_536,
            handler: HandlerBinding {
                handler: key(&format!("inventory:handler/{name}")),
                owner_module: owner(),
            },
            commands: vec![CommandBinding {
                name: key(&format!("inventory:command.{name}")),
                aliases: Vec::new(),
                usage_key: key(&format!("inventory:text/{name}")),
            }],
        })
        .collect()
}

/// Register optional container, request schemas, and the item-port capability.
pub fn register_content(pack: &mut ContentPack) {
    let mut components = Vec::new();
    for name in ["container", "inspect", "transfer"] {
        let fields: Vec<(&str, ComponentFieldType)> = match name {
            "container" => vec![(
                "slots",
                ComponentFieldType::List {
                    max_items: MAX_SLOTS as u32,
                },
            )],
            "transfer" => vec![
                ("source_slot", ComponentFieldType::Unsigned32),
                ("destination", ComponentFieldType::EntityReference),
                ("destination_slot", ComponentFieldType::Unsigned32),
                ("count", ComponentFieldType::Unsigned32),
            ],
            _ => Vec::new(),
        };
        components.push(ComponentSchema {
            id: key(&format!("inventory:schema/{name}")),
            schema_version: if name == "container" { 2 } else { 1 },
            owner_module: owner(),
            fields: fields
                .into_iter()
                .map(|(field, ty)| ComponentField {
                    id: key(&format!("inventory:field/{field}")),
                    field_type: ty,
                    required: true,
                })
                .collect(),
            persistence: if name == "container" {
                PersistencePolicy::Always
            } else {
                PersistencePolicy::Never
            },
            replication: ReplicationDeclaration {
                // Contents are only projected to host-approved object readers.
                // Request parameter schemas remain entirely server-private.
                visibility: if name == "container" {
                    ReplicationVisibility::Authorized
                } else {
                    ReplicationVisibility::Private
                },
                ..ReplicationDeclaration::SERVER_PRIVATE
            },
            migration_binding: None,
        });
    }
    let method = |name: &str| MethodDescriptor {
        id: key(&format!("inventory:method/{name}")),
        parameter_schema: key(&format!("inventory:schema/{name}")),
        result_schema: schema(),
    };
    let capability = CapabilityDescriptor {
        id: key("inventory:capability/item_port"),
        api_version: 2,
        query_methods: vec![method("inspect")],
        mutation_methods: vec![method("transfer")],
        target_schema_constraints: vec![schema()],
        access_contract: key("inventory:permission/transfer"),
        execution_contract: ExecutionContract::BoundedNative,
    };
    register_action_module(
        pack,
        owner(),
        key("inventory:state/container"),
        components,
        descriptors(),
        vec![capability],
    );
}
