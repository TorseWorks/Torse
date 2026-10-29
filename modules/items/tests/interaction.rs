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
    setup_policy(false)
}
fn setup_policy(physical: bool) -> (WorldAuthority, ActionService, Catalog, EntityId) {
    let mut pack = m0_fixture_pack();
    block_actions::register_content(&mut pack);
    torse_inventory::register_content(&mut pack);
    torse_items::register_content(&mut pack);
    let content = compile(pack).unwrap();
    let registry = BlockStateRegistry::build(&content).unwrap();
    let catalog = torse_items::fixture_catalog(&registry).unwrap();
    let catalog = if physical {
        catalog.with_world_drops()
    } else {
        catalog
    };
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
        "items:permission/pickup",
        "world:permission/spawn",
        "world:permission/despawn",
    ]
    .into_iter()
    .fold(
        ActorContext::new(key("test:player"))
            .section(SectionPos::ZERO)
            .target(ActionTarget::Entity(id)),
        |actor, permission| actor.permission(key(permission)),
    )
}

#[test]
fn physical_harvest_falls_and_persists_without_automatic_pickup() {
    let (mut world, service, catalog, id) = setup_policy(true);
    let pos = BlockPos::new(8, 8, 8);
    let stone = world.registry().default_state(&key("base:stone")).unwrap();
    world.store_mut().edit(pos, stone).unwrap();
    world
        .store_mut()
        .edit(BlockPos::new(8, 0, 8), stone)
        .unwrap();
    let before = inventory(&world, &catalog, id);
    commit(&mut world, &service, id, &request(id, false, 2, 1));
    assert_eq!(inventory(&world, &catalog, id), before);
    let drop = world
        .entities()
        .iter()
        .find(|(_, e)| {
            e.components
                .component(&torse_items::drops::schema())
                .is_some()
        })
        .unwrap()
        .1
        .id;
    for tick in 1..100 {
        for group in torse_items::drops::plans(&world, &catalog, Tick(tick)) {
            assert!(matches!(
                world.commit_group(&group).unwrap().outcome,
                TransactionOutcome::Committed { .. }
            ));
        }
    }
    let body = world
        .entities()
        .get(world.entities().handle_of(drop).unwrap())
        .unwrap();
    assert!(
        (body.position.y - 1.125).abs() < 1e-6,
        "landed at {:?}",
        body.position
    );
    assert_eq!(
        torse_items::drops::DropState::read(body, &catalog)
            .unwrap()
            .stack
            .count,
        1
    );
    let snapshot = world
        .capture_snapshot(SnapshotMeta {
            cursor: CommitCursor {
                world: world.world(),
                seq: world.seq(),
            },
            compatibility: world.coordinator().compatibility(),
            resume_at: world.coordinator().resume_point(),
        })
        .unwrap();
    let mut recovery = torse_sim::Recovery::new(world.world(), world.registry(), world.schemas());
    recovery.restore_snapshot(&snapshot).unwrap();
    let restored = recovery.finish().unwrap();
    let recovered = restored
        .entities()
        .get(restored.entities().handle_of(drop).unwrap())
        .unwrap();
    assert_eq!(recovered.position.to_bits(), body.position.to_bits());
    assert_eq!(
        torse_items::drops::DropState::read(recovered, &catalog).unwrap(),
        torse_items::drops::DropState::read(body, &catalog).unwrap()
    );
}

#[test]
fn initial_drop_escapes_refilled_terrain_but_waits_when_enclosed_or_unloaded() {
    for case in 0..3 {
        let (mut world, _, catalog, _) = setup_policy(true);
        let y = if case == 2 { 15 } else { 8 };
        let at = Position::new(8.5, f64::from(y) + 0.5, 8.5);
        let h = world.entities_mut().spawn(at).unwrap();
        let schemas = world.schemas().clone();
        world
            .entities_mut()
            .components_mut(h)
            .unwrap()
            .insert(
                &schemas,
                &torse_items::drops::schema(),
                torse_items::drops::DropState {
                    stack: ItemStack {
                        item: key("base:stone"),
                        count: 7,
                    },
                    velocity: [0; 3],
                }
                .fields(),
            )
            .unwrap();
        world
            .entities_mut()
            .components_mut(h)
            .unwrap()
            .insert(
                &schemas,
                &collider_schema(),
                EntityCollider::Disabled.fields(),
            )
            .unwrap();
        let stone = world.registry().default_state(&key("base:stone")).unwrap();
        world
            .store_mut()
            .edit(BlockPos::new(8, y.into(), 8), stone)
            .unwrap();
        if case != 0 {
            for [dx, dy, dz] in [
                [0, 1, 0],
                [-1, 0, 0],
                [1, 0, 0],
                [0, 0, -1],
                [0, 0, 1],
                [0, -1, 0],
            ] {
                if case == 2 && dy == 1 {
                    continue;
                }
                world
                    .store_mut()
                    .edit(BlockPos::new(8 + dx, i64::from(y) + dy, 8 + dz), stone)
                    .unwrap();
            }
        }
        for tick in 1..30 {
            for group in torse_items::drops::plans(&world, &catalog, Tick(tick)) {
                let prepared = world.prepare_group(&group).unwrap();
                world.publish(prepared).unwrap();
            }
        }
        let body = world.entities().get(h).unwrap();
        assert_eq!(
            torse_items::drops::DropState::read(body, &catalog)
                .unwrap()
                .stack
                .count,
            7
        );
        if case == 0 {
            assert!((body.position.y - 9.125).abs() < 1e-6);
            assert_eq!(
                EntityCollider::of(body).unwrap(),
                Some(EntityCollider::Box { half: [125_000; 3] })
            );
        } else {
            assert_eq!(body.position.to_bits(), at.to_bits());
            assert_eq!(
                EntityCollider::of(body).unwrap(),
                Some(EntityCollider::Disabled)
            );
        }
    }
}

#[test]
fn pickup_is_atomic_partial_and_rejects_forged_child_actions() {
    let (mut world, service, catalog, id) = setup_policy(true);
    let handle = world.entities().handle_of(id).unwrap();
    let schemas = world.schemas().clone();
    let mut stock = Inventory::new(1).unwrap();
    stock
        .set(
            0,
            Some(ItemStack {
                item: key("base:stone"),
                count: 63,
            }),
            catalog.stack_rules(),
        )
        .unwrap();
    world
        .entities_mut()
        .components_mut(handle)
        .unwrap()
        .insert(&schemas, &torse_inventory::schema(), stock.fields())
        .unwrap();
    let drop_handle = world
        .entities_mut()
        .spawn(Position::new(2.0, 2.0, 1.0))
        .unwrap();
    let drop = world.entities().get(drop_handle).unwrap().id;
    let fields = torse_items::drops::DropState {
        stack: ItemStack {
            item: key("base:stone"),
            count: 2,
        },
        velocity: [0; 3],
    }
    .fields();
    world
        .entities_mut()
        .components_mut(drop_handle)
        .unwrap()
        .insert(&schemas, &torse_items::drops::schema(), fields)
        .unwrap();
    world
        .entities_mut()
        .components_mut(drop_handle)
        .unwrap()
        .insert(
            &schemas,
            &collider_schema(),
            EntityCollider::Disabled.fields(),
        )
        .unwrap();
    let actor = actor(id).target(ActionTarget::Entity(drop));
    let call = torse_items::pickup_call(id, drop);
    let mut req = request(id, false, 0, 3);
    req.action = call.action;
    req.target = call.target;
    req.parameters = call.parameters;
    let ActionOutput::Atomic(group) = service
        .plan(&world, &actor, Phase::Interaction, &req)
        .unwrap()
    else {
        panic!("atomic pickup");
    };
    world.publish(world.prepare_group(&group).unwrap()).unwrap();
    assert_eq!(
        inventory(&world, &catalog, id).slots()[0]
            .as_ref()
            .unwrap()
            .count,
        64
    );
    assert_eq!(
        torse_items::drops::DropState::read(world.entities().get(drop_handle).unwrap(), &catalog)
            .unwrap()
            .stack
            .count,
        1
    );
    let seq = world.seq();
    assert!(
        service
            .plan(&world, &actor, Phase::Interaction, &req)
            .is_err()
    );
    req.action = key("items:action/pickup_inventory");
    req.target = ActionTarget::Entity(id);
    assert_eq!(
        service.plan(&world, &actor, Phase::Interaction, &req),
        Err(RejectCode::PermissionDenied)
    );
    assert_eq!(world.seq(), seq);
}

#[test]
fn nearby_equal_items_merge_without_overflow_or_crossing_a_wall() {
    let (mut world, _, catalog, _) = setup_policy(true);
    let schemas = world.schemas().clone();
    for (position, item, count) in [
        (Position::new(4.2, 4.5, 4.5), "base:stone", 40),
        (Position::new(4.4, 4.5, 4.5), "base:stone", 30),
        (Position::new(4.3, 4.5, 4.5), "base:plant", 1),
    ] {
        let h = world.entities_mut().spawn(position).unwrap();
        let state = torse_items::drops::DropState {
            stack: ItemStack {
                item: key(item),
                count,
            },
            velocity: [0; 3],
        };
        world
            .entities_mut()
            .components_mut(h)
            .unwrap()
            .insert(&schemas, &torse_items::drops::schema(), state.fields())
            .unwrap();
        world
            .entities_mut()
            .components_mut(h)
            .unwrap()
            .insert(
                &schemas,
                &collider_schema(),
                EntityCollider::Disabled.fields(),
            )
            .unwrap();
    }
    let groups = torse_items::drops::plans(&world, &catalog, Tick(1));
    for group in groups {
        world.publish(world.prepare_group(&group).unwrap()).unwrap();
    }
    let stacks: Vec<_> = world
        .entities()
        .iter()
        .filter_map(|(_, e)| torse_items::drops::DropState::read(e, &catalog).ok())
        .map(|d| d.stack)
        .collect();
    let mut stone: Vec<_> = stacks
        .iter()
        .filter(|s| s.item == key("base:stone"))
        .map(|s| s.count)
        .collect();
    stone.sort();
    assert_eq!(stone, vec![6, 64]);
    assert_eq!(
        stacks
            .iter()
            .filter(|s| s.item == key("base:plant"))
            .count(),
        1
    );
    // Frozen merge geometry must remain a real read dependency.
    let lower = world.registry().default_state(&key("base:stone")).unwrap();
    world
        .store_mut()
        .edit(BlockPos::new(4, 4, 4), lower)
        .unwrap();
    assert!(
        torse_items::drops::plans(&world, &catalog, Tick(2))
            .iter()
            .all(|g| g.members.iter().all(|t| t.despawns.is_empty()))
    );
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
#[test]
fn supply_requires_host_permission_and_stale_unlimited_edits_are_rejected() {
    use torse_items::supply::{SupplyMode, call};
    let (mut world, service, catalog, id) = setup_policy(true);
    let mut req = request(id, true, 0, 1);
    let config = call(id, SupplyMode::Infinite);
    req.action = config.action;
    req.target = config.target;
    req.parameters = config.parameters;
    assert!(
        service
            .plan(&world, &actor(id), Phase::Interaction, &req)
            .is_err()
    );
    let manager = actor(id).permission(key("items:permission/configure_supply"));
    assert!(
        service
            .plan(&world, &manager, Phase::Interaction, &req)
            .is_err()
    );
    let manager = manager.permission(key("items:permission/unlimited"));
    let ActionOutput::Mutation(tx) = service
        .plan(&world, &manager, Phase::Interaction, &req)
        .unwrap()
    else {
        panic!("mode mutation")
    };
    let prepared = world.prepare(&tx).unwrap();
    world.publish(prepared).unwrap();
    let mut place = request(id, true, 0, 2);
    let snapshot = world
        .capture_snapshot(SnapshotMeta {
            cursor: CommitCursor {
                world: world.world(),
                seq: world.seq(),
            },
            compatibility: world.coordinator().compatibility(),
            resume_at: world.coordinator().resume_point(),
        })
        .unwrap();
    let mut recovery = torse_sim::Recovery::new(world.world(), world.registry(), world.schemas());
    recovery.restore_snapshot(&snapshot).unwrap();
    let recovered = recovery.finish().unwrap();
    let entity = recovered
        .entities()
        .get(recovered.entities().handle_of(id).unwrap())
        .unwrap();
    assert_eq!(torse_items::supply::mode(entity), SupplyMode::Infinite);
    assert_eq!(
        inventory(&recovered, &catalog, id),
        catalog.starter(36).unwrap()
    );
    place.parameters.insert(
        key("items:field/supply_item"),
        ComponentValue::Content(key("base:stone")),
    );
    assert!(
        service
            .plan(&world, &actor(id), Phase::Interaction, &place)
            .is_err()
    );
    let ActionOutput::Atomic(group) = service
        .plan(&world, &manager, Phase::Interaction, &place)
        .unwrap()
    else {
        panic!("supply edit group")
    };
    req.parameters = call(id, SupplyMode::Finite).parameters;
    req.transaction = TransactionId(3);
    let ActionOutput::Mutation(tx) = service
        .plan(&world, &manager, Phase::Interaction, &req)
        .unwrap()
    else {
        panic!("mode mutation")
    };
    let prepared = world.prepare(&tx).unwrap();
    world.publish(prepared).unwrap();
    assert!(world.prepare_group(&group).is_err());
    assert_eq!(
        inventory(&world, &catalog, id),
        catalog.starter(36).unwrap()
    );
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
