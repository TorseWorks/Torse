use torse_content::{compile, m0_fixture_pack};
use torse_inventory::{Inventory, ItemStack};
use torse_items::{Catalog, key};
use torse_sim::action::{
    ActionOrigin, ActionOutput, ActionRequest, ActionService, ActionTarget, ActorContext,
};
use torse_sim::{OrderingKey, Phase, WorldAuthority, block_actions};
use torse_types::*;
use torse_world::*;

fn setup() -> (WorldAuthority, ActionService, Catalog, EntityId) {
    let mut pack = m0_fixture_pack();
    block_actions::register_content(&mut pack);
    torse_inventory::register_content(&mut pack);
    torse_items::register_content(&mut pack);
    let content = compile(pack).unwrap();
    let registry = BlockStateRegistry::build(&content).unwrap();
    let catalog = torse_items::fixture_catalog(&registry).unwrap();
    let schemas = ComponentSchemas::build(&content);
    let mut descriptors = block_actions::descriptors();
    descriptors.extend(torse_items::descriptors());
    let mut handlers = torse_items::block_handlers();
    handlers.extend(torse_items::handlers(catalog.clone()));
    let service = ActionService::bind(descriptors, handlers, &schemas).unwrap();
    let compatibility = catalog.bind_compatibility(Compatibility {
        authority_schema: 4,
        rules_digest: content.authority_digest,
        content_digest: content.presentation_digest,
    });
    let mut world = WorldAuthority::new(
        WorldId(1),
        registry,
        schemas.clone(),
        16,
        WorldBounds::new(0, 32).unwrap(),
        compatibility,
    )
    .unwrap();
    world.store_mut().request(SectionPos::ZERO).unwrap();
    world
        .store_mut()
        .install(SectionPos::ZERO, Section::empty(16).unwrap())
        .unwrap();
    world.store_mut().finish_load(SectionPos::ZERO).unwrap();
    let handle = world
        .entities_mut()
        .spawn(Position::new(1.0, 2.0, 1.0))
        .unwrap();
    let id = world.entities().get(handle).unwrap().id;
    world
        .entities_mut()
        .components_mut(handle)
        .unwrap()
        .insert(
            &schemas,
            &torse_inventory::schema(),
            catalog.starter(36).unwrap().fields(),
        )
        .unwrap();
    (world, service, catalog, id)
}
fn actor(id: EntityId) -> ActorContext {
    [
        "world:permission/edit",
        "items:permission/use",
        "inventory:permission/insert",
        "inventory:permission/consume",
        "inventory:permission/swap",
    ]
    .into_iter()
    .fold(
        ActorContext::new(key("test:player"))
            .section(SectionPos::ZERO)
            .target(ActionTarget::Entity(id)),
        |actor, permission| actor.permission(key(permission)),
    )
}
fn request(id: EntityId, place: bool, slot: u32, seq: u64) -> ActionRequest {
    ActionRequest {
        action: key(if place {
            "items:action/place"
        } else {
            "items:action/harvest"
        }),
        api_version: 1,
        origin: ActionOrigin::Network,
        target: ActionTarget::Block(BlockPos::new(8, 8, 8)),
        transaction: TransactionId(u128::from(seq)),
        ordering: OrderingKey {
            phase: Phase::Interaction,
            rule_priority: 0,
            stable_task_key: seq,
            local_sequence: 0,
        },
        parameters: [
            (key("items:field/inventory"), ComponentValue::Entity(id)),
            (key("items:field/slot"), ComponentValue::U32(slot)),
            (key("items:field/half"), ComponentValue::U32(0)),
            (key("items:field/place"), ComponentValue::Bool(place)),
        ]
        .into(),
    }
}
fn inventory(world: &WorldAuthority, catalog: &Catalog, id: EntityId) -> Inventory {
    Inventory::from_fields(
        world
            .entities()
            .get(world.entities().handle_of(id).unwrap())
            .unwrap()
            .components
            .component(&torse_inventory::schema())
            .unwrap(),
        catalog.stack_rules(),
    )
    .unwrap()
}
fn commit(world: &mut WorldAuthority, service: &ActionService, id: EntityId, req: &ActionRequest) {
    let ActionOutput::Atomic(group) = service
        .plan(world, &actor(id), Phase::Interaction, req)
        .unwrap()
    else {
        panic!("atomic group");
    };
    let prepared = world.prepare_group(&group).unwrap();
    world.publish(prepared).unwrap();
}
#[test]
fn placing_completion_and_harvest_conserve_half_blocks() {
    let (mut world, service, catalog, id) = setup();
    let before = inventory(&world, &catalog, id).slots()[0]
        .as_ref()
        .unwrap()
        .count;
    commit(&mut world, &service, id, &request(id, true, 0, 1));
    assert_eq!(
        inventory(&world, &catalog, id).slots()[0]
            .as_ref()
            .unwrap()
            .count,
        before - 1
    );
    commit(&mut world, &service, id, &request(id, true, 0, 2));
    assert_eq!(
        world
            .registry()
            .definition(world.store().query(BlockPos::new(8, 8, 8)).ready().unwrap())
            .unwrap()
            .id,
        key("base:glass")
    );
    commit(&mut world, &service, id, &request(id, false, 0, 3));
    assert_eq!(
        inventory(&world, &catalog, id).slots()[0]
            .as_ref()
            .unwrap()
            .count,
        before
    );
    assert_eq!(world.seq().0, 3);
}
#[test]
fn blocked_placement_full_inventory_and_stale_plan_change_neither_side() {
    let (mut world, service, catalog, id) = setup();
    commit(&mut world, &service, id, &request(id, true, 2, 1));
    let before = inventory(&world, &catalog, id);
    assert!(
        service
            .plan(
                &world,
                &actor(id),
                Phase::Interaction,
                &request(id, true, 2, 2)
            )
            .is_err()
    );
    assert_eq!(inventory(&world, &catalog, id), before);
    // Fill the last free capacity with incompatible items, including the removed stone.
    let mut full = Inventory::new(36).unwrap();
    for slot in 0..36 {
        full.set(
            slot,
            Some(ItemStack {
                item: key("base:plant"),
                count: 64,
            }),
            catalog.stack_rules(),
        )
        .unwrap();
    }
    let schemas = world.schemas().clone();
    let handle = world.entities().handle_of(id).unwrap();
    world
        .entities_mut()
        .components_mut(handle)
        .unwrap()
        .insert(&schemas, &torse_inventory::schema(), full.fields())
        .unwrap();
    assert!(
        service
            .plan(
                &world,
                &actor(id),
                Phase::Interaction,
                &request(id, false, 0, 3)
            )
            .is_err()
    );
    assert!(
        !world
            .store()
            .query(BlockPos::new(8, 8, 8))
            .ready()
            .unwrap()
            .is_empty()
    );
    assert_eq!(inventory(&world, &catalog, id), full);
    // A new plan loses its exact component observation after any intervening edit.
    let stock = catalog.starter(36).unwrap();
    world
        .entities_mut()
        .components_mut(handle)
        .unwrap()
        .insert(&schemas, &torse_inventory::schema(), stock.fields())
        .unwrap();
    let ActionOutput::Atomic(group) = service
        .plan(
            &world,
            &actor(id),
            Phase::Interaction,
            &request(id, false, 0, 4),
        )
        .unwrap()
    else {
        panic!();
    };
    commit(&mut world, &service, id, &request(id, false, 0, 5));
    assert!(world.prepare_group(&group).is_err());
}
#[test]
fn direct_settlement_raw_edits_and_ungranted_inventory_are_rejected() {
    let (world, service, _, id) = setup();
    let mut req = request(id, false, 0, 1);
    req.action = key("items:action/settle");
    assert_eq!(
        service.plan(&world, &actor(id), Phase::Interaction, &req),
        Err(RejectCode::PermissionDenied)
    );
    req = request(id, true, 2, 2);
    assert_eq!(
        service.plan(&world, &actor(EntityId(999)), Phase::Interaction, &req),
        Err(RejectCode::PermissionDenied)
    );
    req.action = key("world:action/place");
    req.parameters = [(
        key("world:field/block"),
        ComponentValue::Content(key("base:stone")),
    )]
    .into();
    assert_eq!(
        service.plan(&world, &actor(id), Phase::Interaction, &req),
        Err(RejectCode::PermissionDenied)
    );
}

#[test]
fn catalog_rules_are_canonical_and_reject_invalid_external_definitions() {
    use torse_items::ItemDefinition;
    let (world, _, _, _) = setup();
    let definition = ItemDefinition {
        id: key("test:stone"),
        limit: 64,
        block: Some(key("base:stone")),
    };
    let other = ItemDefinition {
        id: key("test:plant"),
        limit: 32,
        block: Some(key("base:plant")),
    };
    let drops = vec![(
        key("base:stone"),
        ItemStack {
            item: definition.id.clone(),
            count: 1,
        },
    )];
    let a = Catalog::new(
        [definition.clone(), other.clone()],
        drops.clone(),
        world.registry(),
    )
    .unwrap();
    let b = Catalog::new([other, definition.clone()], drops.clone(), world.registry()).unwrap();
    let base = world.coordinator().compatibility();
    assert_eq!(a.bind_compatibility(base), b.bind_compatibility(base));
    let mut changed = drops.clone();
    changed[0].1.count = 2;
    let c = Catalog::new(a.definitions().cloned(), changed, world.registry()).unwrap();
    assert_ne!(
        a.bind_compatibility(base).rules_digest,
        c.bind_compatibility(base).rules_digest
    );
    assert!(
        Catalog::new(
            [definition.clone(), definition.clone()],
            drops.clone(),
            world.registry()
        )
        .is_err()
    );
    let mut invalid = definition;
    invalid.block = Some(key("missing:block"));
    assert!(Catalog::new([invalid], drops, world.registry()).is_err());
}
