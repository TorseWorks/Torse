//! Optional native place/break actions, shared by every input adapter.

pub mod placement;

use torse_actions::{
    ActionDescriptor, ActionKind, ActionPhase, CommandBinding, HandlerBinding, PermissionRule,
};
use torse_content::{
    ComponentField, ComponentFieldType, ComponentSchema, ContentPack, PersistencePolicy,
};
use torse_types::{ContentKey, RejectCode, ReplicationDeclaration};
use torse_world::{BlockStateId, ComponentValue, TerrainQuery};

use crate::Transaction;
use crate::action::{
    ActionHandler, ActionOutput, ActionRequest, ActionTarget, ActionView, NativeHandler,
    register_action_module,
};

fn key(value: &str) -> ContentKey {
    ContentKey::parse(value).expect("block action key")
}

/// Module identity checked by the dispatcher and generic component boundary.
pub fn owner() -> ContentKey {
    key("world:module")
}

/// Frozen descriptors; handlers must be explicitly bound by the application.
pub fn descriptors() -> Vec<ActionDescriptor> {
    ["place", "break"]
        .into_iter()
        .map(|name| ActionDescriptor {
            id: key(&format!("world:action/{name}")),
            api_version: 1,
            kind: ActionKind::Mutation,
            parameter_schema: key(&format!("world:schema/{name}")),
            result_schema: key("world:schema/result"),
            permission: PermissionRule {
                permission: Some(key("world:permission/edit")),
            },
            target_constraints: Vec::new(),
            phase: ActionPhase::Interaction,
            max_items: 1,
            max_bytes: 64,
            handler: HandlerBinding {
                handler: key(&format!("world:handler/{name}")),
                owner_module: owner(),
            },
            commands: vec![CommandBinding {
                name: key(&format!("world:command.{name}")),
                aliases: Vec::new(),
                usage_key: key(&format!("world:text/{name}")),
            }],
        })
        .collect()
}

/// Install optional block action metadata without altering the M0 fixture.
pub fn register_content(pack: &mut ContentPack) {
    let components = ["place", "break", "result"]
        .into_iter()
        .map(|name| ComponentSchema {
            id: key(&format!("world:schema/{name}")),
            schema_version: 1,
            owner_module: owner(),
            fields: if name == "place" {
                vec![
                    ComponentField {
                        id: key("world:field/block"),
                        field_type: ComponentFieldType::ContentReference,
                        required: true,
                    },
                    // The aim half steers half-type blocks between their
                    // lower and upper states.
                    ComponentField {
                        id: key("world:field/half"),
                        field_type: ComponentFieldType::Unsigned32,
                        required: false,
                    },
                ]
            } else {
                Vec::new()
            },
            persistence: PersistencePolicy::Never,
            replication: ReplicationDeclaration::SERVER_PRIVATE,
            migration_binding: None,
        })
        .collect();
    register_action_module(
        pack,
        owner(),
        key("world:state/actions"),
        components,
        descriptors(),
        Vec::new(),
    );
}

/// Bounded implementations for the two descriptors above.
pub fn handlers() -> Vec<NativeHandler> {
    [false, true]
        .into_iter()
        .map(|place| NativeHandler {
            handler: key(if place {
                "world:handler/place"
            } else {
                "world:handler/break"
            }),
            owner: owner(),
            implementation: Box::new(BlockHandler { place }),
        })
        .collect()
}

struct BlockHandler {
    place: bool,
}

impl ActionHandler for BlockHandler {
    fn plan(
        &self,
        view: &ActionView<'_>,
        request: &ActionRequest,
    ) -> Result<ActionOutput, RejectCode> {
        let ActionTarget::Block(pos) = request.target else {
            return Err(RejectCode::InvalidData);
        };
        let current = match view.terrain(pos)? {
            TerrainQuery::Ready(state) => state,
            TerrainQuery::NotLoaded => return Err(RejectCode::NotLoaded),
            TerrainQuery::OutOfBounds | TerrainQuery::Exterior => {
                return Err(RejectCode::MissingTarget);
            }
        };
        let state = if self.place {
            let Some(ComponentValue::Content(block)) =
                request.parameters.get(&key("world:field/block"))
            else {
                return Err(RejectCode::InvalidData);
            };
            let half = match request.parameters.get(&key("world:field/half")) {
                Some(ComponentValue::U32(value)) => *value as usize,
                _ => 0,
            };
            if current.is_empty() {
                // Half-type blocks take the half from the aim point: the
                // client computes it from where the ray met the surface.
                view.state_with_half(block, half)
                    .ok_or(RejectCode::MissingTarget)?
            } else {
                // A cell that already holds the same block may complete into
                // the block its content names, filling the remaining volume.
                view.completion_state_for_side(block, current, half)
                    .ok_or(RejectCode::Conflict)?
            }
        } else {
            if current.is_empty() {
                return Err(RejectCode::Conflict);
            }
            BlockStateId::EMPTY
        };
        // A solid placement must not overlap any entity's collider:
        // burying a walker leaves it wedged, since movement refuses
        // colliders that already overlap terrain. Empty results (air,
        // break) and non-solid completions fall through.
        if view.cell_blockers(pos, state)? {
            return Err(RejectCode::Conflict);
        }
        let mut tx = Transaction::new(request.transaction, owner(), request.ordering);
        tx.read_block(pos, current).write_block(pos, state);
        if self.place {
            tx.require_block_clear(pos, state);
        }
        Ok(ActionOutput::Mutation(Box::new(tx)))
    }
}
