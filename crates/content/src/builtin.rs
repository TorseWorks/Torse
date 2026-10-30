//! Dependency-free M0 fixture content.
//!
//! The fixture intentionally exercises independent shape properties: stone is a
//! full cube, glass is a transparent lower slab, and a plant has visual and
//! crossed visual and bounded selection geometry but no collision. Component, capability, permission, and
//! action declarations are included to validate references across all four
//! content contracts without requiring a running world.

use torse_types::{ContentKey, ReplicationDeclaration};

use crate::{
    AlphaMode, BlockDefinition, CapabilityBinding, CapabilityDescriptor, CollisionProfile,
    ComponentSchema, ContentPack, ExecutionContract, FaceMaterials, FaceOverlays, FiniteField,
    InteractionProfile, LightTransport, MethodDescriptor, ModuleDescriptor, ModuleSide,
    OcclusionProfile, PersistencePolicy, RenderProfile, RequestedPermission, SelectionProfile,
    ServiceDescriptor, Shape, SourceRef, Sourced, StateSchema,
};

/// Build the deterministic content pack used by M0 tests and tooling.
pub fn m0_fixture_pack() -> ContentPack {
    let simple_state = key("base:state/simple");
    let slab_state = key("base:state/slab");
    let plant_state = key("base:state/plant_growth");
    // The inventory schema is declared but not attached to any of the
    // blocks. This keeps the built-in block models visually realistic while
    // still exercising component and capability reference resolution.
    let inventory_schema = key("base:schema/inventory_content");
    let inventory_capability = key("base:capability/item_port");
    let inspect_action = key("base:action/inspect");

    ContentPack {
        state_schemas: vec![
            sourced(
                "fixtures/m0/state_simple.json",
                StateSchema {
                    id: simple_state.clone(),
                    schema_version: 1,
                    finite_fields: Vec::new(),
                    component_schemas: Vec::new(),
                },
            ),
            sourced(
                "fixtures/m0/state_slab.json",
                crate::presets::slab_schema(slab_state.clone()),
            ),
            sourced(
                "fixtures/m0/state_plant.json",
                StateSchema {
                    id: plant_state.clone(),
                    schema_version: 1,
                    finite_fields: vec![FiniteField {
                        id: key("base:field/growth"),
                        default_value: 0,
                        values: vec![0, 1, 2, 3],
                    }],
                    component_schemas: Vec::new(),
                },
            ),
            sourced(
                "fixtures/m0/state_inventory.json",
                StateSchema {
                    id: key("base:state/container"),
                    schema_version: 1,
                    finite_fields: Vec::new(),
                    component_schemas: vec![ComponentSchema {
                        id: inventory_schema.clone(),
                        schema_version: 1,
                        owner_module: key("base:inventory"),
                        fields: Vec::new(),
                        persistence: PersistencePolicy::Always,
                        replication: ReplicationDeclaration::SERVER_PRIVATE,
                        migration_binding: None,
                    }],
                },
            ),
        ],
        capabilities: vec![sourced(
            "fixtures/m0/capability_item_port.json",
            CapabilityDescriptor {
                id: inventory_capability.clone(),
                api_version: 1,
                query_methods: vec![MethodDescriptor {
                    id: key("base:method/item_port.query"),
                    parameter_schema: key("base:schema/empty"),
                    result_schema: inventory_schema.clone(),
                }],
                mutation_methods: Vec::new(),
                target_schema_constraints: vec![inventory_schema.clone()],
                access_contract: key("base:access/authorized_viewer"),
                execution_contract: ExecutionContract::BoundedNative,
            },
        )],
        actions: torse_actions::m0_fixture_actions()
            .into_iter()
            .map(|action| {
                let source = format!("fixtures/m0/{}.json", action.id.path().replace('/', "_"));
                sourced(&source, action)
            })
            .collect(),
        modules: vec![sourced(
            "fixtures/m0/module_base.json",
            ModuleDescriptor {
                id: key("base:core"),
                module_version: 1,
                framework_api: 1,
                side: ModuleSide::Shared,
                required_modules: Vec::new(),
                optional_modules: Vec::new(),
                provided_services: vec![ServiceDescriptor {
                    id: key("base:service/content"),
                    api_version: 1,
                }],
                required_services: Vec::new(),
                registered_schemas: vec![simple_state.clone(), plant_state.clone()],
                registered_capabilities: vec![inventory_capability.clone()],
                registered_actions: vec![inspect_action.clone()],
                registered_systems: Vec::new(),
                requested_permissions: vec![RequestedPermission {
                    id: key("base:permission/inspect"),
                }],
                configuration_schema: None,
                lifecycle_binding: None,
            },
        )],
        blocks: vec![
            sourced(
                "fixtures/m0/stone.json",
                block(
                    "base:stone",
                    simple_state,
                    Shape::FullCube,
                    Some(Shape::FullCube),
                    Some(Shape::FullCube),
                    0b11_1111,
                    AlphaMode::Opaque,
                    inspect_action.clone(),
                    None,
                ),
            ),
            sourced("fixtures/m0/glass_slab.json", {
                let mut slab = block(
                    "base:glass_slab",
                    slab_state.clone(),
                    Shape::LowerSlab,
                    Some(Shape::LowerSlab),
                    Some(Shape::LowerSlab),
                    // A blended material hides nothing: neighbours stay
                    // visible through the glass, so occlusion claims none.
                    0,
                    AlphaMode::Mask,
                    inspect_action.clone(),
                    Some(key("base:glass")),
                );
                // The slab and the full glass block share one material.
                slab.render_profile.material = key("base:material/glass");
                crate::presets::slab(&mut slab, slab_state.clone());
                slab.definition_version = 2;
                slab
            }),
            sourced("fixtures/m0/plant.json", {
                let mut plant = BlockDefinition {
                    collision_profile: CollisionProfile { shape: None },
                    tags: vec![key("base:tag/plant")],
                    ..block(
                        "base:plant",
                        plant_state,
                        Shape::Cross,
                        None,
                        None,
                        0,
                        AlphaMode::Mask,
                        inspect_action.clone(),
                        None,
                    )
                };
                crate::presets::crossed_plant(&mut plant);
                plant.definition_version = 2;
                plant
            }),
            sourced(
                "fixtures/m0/glass.json",
                block(
                    "base:glass",
                    key("base:state/simple"),
                    Shape::FullCube,
                    Some(Shape::FullCube),
                    Some(Shape::FullCube),
                    // Transparent like the slab it completes into.
                    0,
                    AlphaMode::Mask,
                    inspect_action,
                    None,
                ),
            ),
        ],
    }
}

#[allow(clippy::too_many_arguments)]
fn block(
    id: &str,
    state_schema: ContentKey,
    render_shape: Shape,
    collision_shape: Option<Shape>,
    selection_shape: Option<Shape>,
    face_mask: u8,
    alpha_mode: AlphaMode,
    action: ContentKey,
    completes_to: Option<ContentKey>,
) -> BlockDefinition {
    // A helper keeps the fixture focused on deliberately different profiles.
    // Render, collision, selection, occlusion, and light are still populated as
    // separate fields so no hidden inference is introduced.
    let id = key(id);
    // One material per block keeps the fallback colours distinct; real content
    // may share a material across blocks and override individual faces.
    let material = key(&format!("base:material/{}", id.path()));
    BlockDefinition {
        id,
        definition_version: if face_mask == 0x3f { 2 } else { 1 },
        state_schema,
        render_profile: RenderProfile {
            shape: render_shape,
            material,
            face_materials: FaceMaterials::none(),
            face_overlays: FaceOverlays::none(),
            alpha_mode,
        },
        collision_profile: CollisionProfile {
            shape: collision_shape,
        },
        selection_profile: SelectionProfile {
            shape: selection_shape,
        },
        occlusion_profile: OcclusionProfile { face_mask },
        light_transport: LightTransport {
            absorption: if face_mask == 0x3f { 1.0 } else { 0.0 },
            emission: 0.0,
        },
        interaction_profile: InteractionProfile {
            action: Some(action),
            completes_to,
        },
        component_defaults: Vec::new(),
        capability_bindings: Vec::<CapabilityBinding>::new(),
        behavior_bindings: Vec::new(),
        state_overrides: Vec::new(),
        tags: Vec::new(),
    }
}

fn sourced<T>(source: &str, value: T) -> Sourced<T> {
    // Fixture sources point at plausible future file names even though M0
    // constructs the descriptors in memory.
    Sourced::new(SourceRef::new(source, 1), value)
}

fn key(value: &str) -> ContentKey {
    // Built-in keys are constants verified by tests; an invalid literal here is
    // a programming error, not user input that should be recoverable at runtime.
    ContentKey::parse(value).expect("built-in content key is valid")
}
