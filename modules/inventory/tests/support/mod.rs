//! Shared fixtures use the public optional-module and action assembly paths.

use torse_content::{compile, m0_fixture_pack};
use torse_inventory::{Inventory, ItemCatalog, ItemStack};
use torse_sim::action::{
    ActionOrigin, ActionOutput, ActionRequest, ActionService, ActionTarget, ActorContext,
};
use torse_sim::{OrderingKey, Phase, Transaction, WorldAuthority, block_actions};
use torse_types::{
    BlockPos, CommitCursor, Compatibility, ContentKey, EntityId, SectionPos, SnapshotMeta,
    TransactionId, WorldId,
};
use torse_world::{
    BlockStateRegistry, ComponentFields, ComponentSchemas, ComponentValue, Position, Section,
    WorldBounds,
};

pub fn key(value: &str) -> ContentKey {
    ContentKey::parse(value).expect("fixture key")
}

pub fn setup() -> (WorldAuthority, ActionService, ItemCatalog, [EntityId; 3]) {
    let mut pack = m0_fixture_pack();
    block_actions::register_content(&mut pack);
    torse_inventory::register_content(&mut pack);
    let content = compile(pack).expect("module content compiles");
    let catalog =
        ItemCatalog::new([(key("base:stone"), 64), (key("base:glass_slab"), 64)]).expect("catalog");
    let mut schemas = ComponentSchemas::build(&content);
    catalog
        .bind_schemas(&mut schemas)
        .expect("inventory validation");
    let mut handlers = block_actions::handlers();
    handlers.extend(torse_inventory::handlers(catalog.clone()));
    let mut descriptors = block_actions::descriptors();
    descriptors.extend(torse_inventory::descriptors());
    let service = ActionService::bind(descriptors, handlers, &schemas).expect("bind");
    let mut world = WorldAuthority::new(
        WorldId(31),
        BlockStateRegistry::build(&content).expect("states"),
        schemas.clone(),
        16,
        WorldBounds::new(0, 32).expect("bounds"),
        catalog.bind_compatibility(Compatibility {
            authority_schema: 3,
            rules_digest: content.authority_digest,
            content_digest: content.presentation_digest,
        }),
    )
    .expect("world");
    world
        .store_mut()
        .request(SectionPos::ZERO)
        .expect("request");
    world
        .store_mut()
        .install(SectionPos::ZERO, Section::empty(16).expect("section"))
        .expect("install");
    world
        .store_mut()
        .finish_load(SectionPos::ZERO)
        .expect("load");
    let mut ids = [EntityId(0); 3];
    for (index, id) in ids.iter_mut().enumerate() {
        let handle = world
            .entities_mut()
            .spawn(Position::new(2.0 + index as f64, 2.0, 2.0))
            .expect("spawn");
        *id = world.entities().get(handle).expect("entity").id;
        let mut inventory = Inventory::new(2).expect("inventory");
        if index == 0 {
            inventory
                .set(
                    0,
                    Some(ItemStack {
                        item: key("base:stone"),
                        count: 1,
                        data: ComponentFields::new(),
                    }),
                    &catalog,
                )
                .expect("stock");
        }
        world
            .entities_mut()
            .components_mut(handle)
            .expect("components")
            .insert(&schemas, &torse_inventory::schema(), inventory.fields())
            .expect("install inventory");
    }
    (world, service, catalog, ids)
}

pub fn actor(ids: &[EntityId]) -> ActorContext {
    ids.iter().fold(
        ActorContext::new(key("base:test_actor"))
            .permission(key("inventory:permission/transfer"))
            .permission(key("inventory:permission/read"))
            .permission(key("world:permission/edit"))
            .target(ActionTarget::Block(BlockPos::new(8, 2, 2))),
        |actor, id| actor.target(ActionTarget::Entity(*id)),
    )
}

pub fn request(source: EntityId, destination: EntityId, id: u128) -> ActionRequest {
    ActionRequest {
        action: key("inventory:action/transfer"),
        api_version: 2,
        origin: ActionOrigin::Network,
        target: ActionTarget::Entity(source),
        transaction: TransactionId(id),
        ordering: OrderingKey {
            phase: Phase::Interaction,
            rule_priority: 0,
            stable_task_key: id as u64,
            local_sequence: 0,
        },
        parameters: [
            (
                key("inventory:field/destination"),
                ComponentValue::Entity(destination),
            ),
            (key("inventory:field/source_slot"), ComponentValue::U32(0)),
            (
                key("inventory:field/destination_slot"),
                ComponentValue::U32(0),
            ),
            (key("inventory:field/count"), ComponentValue::U32(1)),
        ]
        .into_iter()
        .collect(),
    }
}

pub fn plan(
    service: &ActionService,
    world: &WorldAuthority,
    ids: &[EntityId],
    request: &ActionRequest,
) -> Transaction {
    match service
        .plan(world, &actor(ids), Phase::Interaction, request)
        .expect("plan")
    {
        ActionOutput::Mutation(tx) => *tx,
        _ => panic!("expected mutation plan"),
    }
}

pub fn fields(world: &WorldAuthority, id: EntityId) -> &ComponentFields {
    let handle = world.entities().handle_of(id).expect("handle");
    world
        .entities()
        .get(handle)
        .expect("entity")
        .components
        .component(&torse_inventory::schema())
        .expect("inventory fields")
}

pub fn total(world: &WorldAuthority, ids: &[EntityId], catalog: &ItemCatalog) -> u64 {
    ids.iter()
        .map(|id| {
            Inventory::from_fields(fields(world, *id), catalog)
                .expect("inventory")
                .slots()
                .iter()
                .flatten()
                .map(|stack| u64::from(stack.count))
                .sum::<u64>()
        })
        .sum()
}

pub fn meta(world: &WorldAuthority) -> SnapshotMeta {
    SnapshotMeta {
        cursor: CommitCursor {
            world: world.world(),
            seq: world.seq(),
        },
        compatibility: world.coordinator().compatibility(),
        resume_at: world.coordinator().resume_point(),
    }
}

/// An unrecognized nested extension must survive every transport unchanged.
pub fn instance_data() -> ComponentFields {
    [
        (key("base:durability"), ComponentValue::U32(73)),
        (
            key("extension:opaque"),
            ComponentValue::Record(
                [(
                    key("extension:values"),
                    ComponentValue::List(vec![
                        ComponentValue::Text("custom label".to_owned()),
                        ComponentValue::I64(-19),
                        ComponentValue::Bool(false),
                        ComponentValue::Content(key("extension:unknown_content")),
                    ]),
                )]
                .into(),
            ),
        ),
    ]
    .into()
}

pub fn install_instance(world: &mut WorldAuthority, catalog: &ItemCatalog, id: EntityId) {
    let mut inventory = Inventory::new(2).expect("inventory");
    inventory
        .set(
            0,
            Some(ItemStack {
                item: key("base:stone"),
                count: 1,
                data: instance_data(),
            }),
            catalog,
        )
        .expect("instance");
    let schemas = world.schemas().clone();
    let handle = world.entities().handle_of(id).expect("entity");
    world
        .entities_mut()
        .components_mut(handle)
        .expect("components")
        .insert(&schemas, &torse_inventory::schema(), inventory.fields())
        .expect("install instance");
}

pub fn codec_roundtrip<T: torse_codec::wire::Wire>(value: &T) -> T {
    let budget = torse_types::OutputBudget {
        max_items: 10_000,
        max_bytes: 4_000_000,
    };
    let limits = torse_codec::CodecLimits {
        max_frame_bytes: 1_000_000,
        max_field_bytes: 65_536,
        max_depth: torse_codec::MIN_WORLD_DEPTH,
    };
    let mut bytes = Vec::new();
    torse_codec::wire::encode(value, &mut bytes, budget, limits).expect("encode");
    let decoded = torse_codec::wire::decode(&bytes, budget, limits).expect("decode");
    let mut encoded_again = Vec::new();
    torse_codec::wire::encode(&decoded, &mut encoded_again, budget, limits).expect("re-encode");
    assert_eq!(encoded_again, bytes);
    decoded
}
