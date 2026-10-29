//! Exact relocation keeps identity and does not widen ordinary movement.
use torse_content::{compile, m0_fixture_pack};
use torse_sim::action::{ActionTarget, ActorContext};
use torse_sim::durable::StoredTransactionGroup;
use torse_sim::{
    DestinationPolicy, OrderingKey, Phase, Transaction, TransactionGroup, WorldAuthority,
};
use torse_types::*;
use torse_world::*;

fn key(value: &str) -> ContentKey {
    ContentKey::parse(value).expect("key")
}

fn world() -> WorldAuthority {
    let content = compile(m0_fixture_pack()).expect("content");
    let registry = BlockStateRegistry::build(&content).expect("registry");
    let schemas = ComponentSchemas::build(&content);
    WorldAuthority::new(
        WorldId(1),
        registry,
        schemas,
        16,
        WorldBounds::new(0, 32).expect("bounds"),
        Compatibility {
            authority_schema: 1,
            rules_digest: Digest([1; 32]),
            content_digest: content.presentation_digest,
        },
    )
    .expect("authority")
}

fn load(world: &mut WorldAuthority, section: SectionPos) {
    world.store_mut().request(section).unwrap();
    world
        .store_mut()
        .install(section, Section::empty(16).unwrap())
        .unwrap();
    world.store_mut().finish_load(section).unwrap();
}

fn ordering() -> OrderingKey {
    OrderingKey {
        phase: Phase::Interaction,
        rule_priority: 0,
        stable_task_key: 1,
        local_sequence: 0,
    }
}

fn grant(entity: EntityId) -> ActorContext {
    ActorContext::new(key("test:game"))
        .target(ActionTarget::Entity(entity))
        .permission(torse_sim::action::relocate_permission())
}

fn place(world: &mut WorldAuthority, body: EntityCollider, at: Position) -> (EntityId, Position) {
    let handle = world.entities_mut().spawn(at).unwrap();
    let schemas = world.schemas().clone();
    world
        .entities_mut()
        .components_mut(handle)
        .unwrap()
        .insert(&schemas, &collider_schema(), body.fields())
        .unwrap();
    let entity = world.entities().get(handle).unwrap();
    (entity.id, entity.position)
}

fn position(world: &WorldAuthority, id: EntityId) -> Position {
    let handle = world.entities().handle_of(id).unwrap();
    world.entities().get(handle).unwrap().position
}

fn rejection(result: TransactionResult<torse_sim::Committed>) -> RejectCode {
    match result.outcome {
        TransactionOutcome::Rejected(code) => code,
        TransactionOutcome::Committed { .. } => panic!("expected rejection"),
    }
}

#[test]
fn planning_requires_permission_and_does_not_publish() {
    let mut world = world();
    load(&mut world, SectionPos::ZERO);
    load(&mut world, SectionPos::new(1, 0, 0));
    let body = EntityCollider::Box { half: [200_000; 3] };
    let origin = Position::new(0.5, 0.5, 0.5);
    let (id, expected) = place(&mut world, body, origin);
    let destination = Position::new(20.5, 0.5, 0.5);
    let seq = world.seq();
    let no_permission = ActorContext::new(key("test:game")).target(ActionTarget::Entity(id));
    assert_eq!(
        no_permission.view(&world).plan_relocation(
            id,
            expected,
            destination,
            DestinationPolicy::Clear,
            TransactionId(1),
            ordering(),
        ),
        Err(RejectCode::PermissionDenied)
    );
    let section_only = ActorContext::new(key("test:game"))
        .section(SectionPos::ZERO)
        .permission(torse_sim::action::relocate_permission());
    assert_eq!(
        section_only.view(&world).plan_relocation(
            id,
            expected,
            destination,
            DestinationPolicy::Clear,
            TransactionId(1),
            ordering(),
        ),
        Err(RejectCode::PermissionDenied)
    );
    let missing = ActorContext::new(key("test:game"))
        .target(ActionTarget::Entity(EntityId(99)))
        .permission(torse_sim::action::relocate_permission());
    assert_eq!(
        missing.view(&world).plan_relocation(
            EntityId(99),
            expected,
            destination,
            DestinationPolicy::Clear,
            TransactionId(1),
            ordering(),
        ),
        Err(RejectCode::MissingTarget)
    );
    assert_eq!(
        grant(id).view(&world).plan_relocation(
            id,
            Position::new(1.0, 0.5, 0.5),
            destination,
            DestinationPolicy::Clear,
            TransactionId(1),
            ordering(),
        ),
        Err(RejectCode::Conflict)
    );
    assert_eq!(
        grant(id).view(&world).plan_relocation(
            id,
            expected,
            expected,
            DestinationPolicy::Clear,
            TransactionId(1),
            ordering(),
        ),
        Err(RejectCode::InvalidData)
    );
    assert_eq!(
        grant(id).view(&world).plan_relocation(
            id,
            expected,
            Position::new(f64::NAN, 0.5, 0.5),
            DestinationPolicy::Clear,
            TransactionId(1),
            ordering(),
        ),
        Err(RejectCode::InvalidData)
    );
    assert_eq!(
        grant(id).view(&world).plan_relocation(
            id,
            expected,
            Position::new(0.5, torse_sim::movement::MAX_MOVEMENT_COORDINATE + 8.0, 0.5),
            DestinationPolicy::Resident,
            TransactionId(1),
            ordering(),
        ),
        Err(RejectCode::InvalidData)
    );
    assert_eq!(world.seq(), seq);
    assert_eq!(position(&world, id), origin);
    let collider = EntityCollider::of(
        world
            .entities()
            .get(world.entities().handle_of(id).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(collider, Some(body));
}

#[test]
fn clear_and_resident_destinations_keep_identity() {
    let mut world = world();
    load(&mut world, SectionPos::ZERO);
    load(&mut world, SectionPos::new(1, 0, 0));
    let body = EntityCollider::Box { half: [200_000; 3] };
    let origin = Position::new(0.5, 0.5, 0.5);
    let (id, expected) = place(&mut world, body, origin);
    let stone = world.registry().default_state(&key("base:stone")).unwrap();
    world
        .store_mut()
        .edit(BlockPos::new(4, 0, 0), stone)
        .unwrap();
    let blocked = Position::new(4.5, 0.5, 0.5);
    let actor = grant(id);
    assert_eq!(
        actor.view(&world).plan_relocation(
            id,
            expected,
            blocked,
            DestinationPolicy::Clear,
            TransactionId(1),
            ordering(),
        ),
        Err(RejectCode::Conflict)
    );
    assert_eq!(position(&world, id), origin);
    let into_stone = actor
        .view(&world)
        .plan_relocation(
            id,
            expected,
            blocked,
            DestinationPolicy::Resident,
            TransactionId(2),
            ordering(),
        )
        .unwrap();
    assert_eq!(into_stone.source, torse_sim::action::relocation_source());
    assert!(into_stone.movements.is_empty());
    assert!(into_stone.spawns.is_empty() && into_stone.despawns.is_empty());
    assert!(!into_stone.preconditions.is_empty());
    assert_eq!(world.seq(), CommitSeq::INITIAL);
    world.commit(&into_stone).unwrap();
    assert_eq!(position(&world, id).to_bits(), blocked.to_bits());
    assert!(world.entities().handle_of(id).is_some());
    let stored = world
        .entities()
        .get(world.entities().handle_of(id).unwrap())
        .unwrap();
    assert_eq!(stored.id, id);
    assert_eq!(EntityCollider::of(stored).unwrap(), Some(body));

    let unloaded = Position::new(40.5, 0.5, 0.5);
    let here = position(&world, id);
    assert_eq!(
        actor.view(&world).plan_relocation(
            id,
            here,
            unloaded,
            DestinationPolicy::Clear,
            TransactionId(3),
            ordering(),
        ),
        Err(RejectCode::NotLoaded)
    );
    assert_eq!(
        actor.view(&world).plan_relocation(
            id,
            here,
            unloaded,
            DestinationPolicy::Resident,
            TransactionId(4),
            ordering(),
        ),
        Err(RejectCode::NotLoaded)
    );
    assert_eq!(
        actor.view(&world).plan_relocation(
            id,
            here,
            Position::new(0.5, 40.5, 0.5),
            DestinationPolicy::Resident,
            TransactionId(5),
            ordering(),
        ),
        Err(RejectCode::NotLoaded)
    );
    assert_eq!(position(&world, id).to_bits(), here.to_bits());
}

#[test]
fn disabled_bodies_check_one_cell_and_block_instances_are_refused() {
    let mut world = world();
    load(&mut world, SectionPos::ZERO);
    let disabled = EntityCollider::Disabled;
    let origin = Position::new(1.5, 0.5, 0.5);
    let (id, expected) = place(&mut world, disabled, origin);
    let stone = world.registry().default_state(&key("base:stone")).unwrap();
    world
        .store_mut()
        .edit(BlockPos::new(3, 0, 0), stone)
        .unwrap();
    let actor = grant(id);
    let solid = Position::new(3.5, 0.5, 0.5);
    assert_eq!(
        actor.view(&world).plan_relocation(
            id,
            expected,
            solid,
            DestinationPolicy::Clear,
            TransactionId(1),
            ordering(),
        ),
        Err(RejectCode::Conflict)
    );
    let resident = actor
        .view(&world)
        .plan_relocation(
            id,
            expected,
            solid,
            DestinationPolicy::Resident,
            TransactionId(2),
            ordering(),
        )
        .unwrap();
    let frozen = StoredTransactionGroup::single(&world, &resident).unwrap();
    let restored = frozen.restore(&world).unwrap();
    world.commit_group(&restored).unwrap();
    assert_eq!(position(&world, id).to_bits(), solid.to_bits());

    let open = Position::new(2.5, 0.5, 0.5);
    let here = position(&world, id);
    let clear = actor
        .view(&world)
        .plan_relocation(
            id,
            here,
            open,
            DestinationPolicy::Clear,
            TransactionId(3),
            ordering(),
        )
        .unwrap();
    world.commit(&clear).unwrap();
    assert_eq!(position(&world, id).to_bits(), open.to_bits());

    let bound = BlockInstance {
        pos: BlockPos::new(2, 0, 0),
        block: key("base:stone"),
    };
    let schemas = world.schemas().clone();
    let handle = world.entities().handle_of(id).unwrap();
    world
        .entities_mut()
        .components_mut(handle)
        .unwrap()
        .insert(&schemas, &block_instance_schema(), bound.fields())
        .unwrap();
    assert_eq!(
        actor.view(&world).plan_relocation(
            id,
            open,
            Position::new(1.5, 0.5, 0.5),
            DestinationPolicy::Clear,
            TransactionId(4),
            ordering(),
        ),
        Err(RejectCode::PermissionDenied)
    );
    assert_eq!(position(&world, id).to_bits(), open.to_bits());
}

#[test]
fn a_group_can_relocate_and_edit_another_owners_component() {
    let mut world = world();
    load(&mut world, SectionPos::ZERO);
    let body = EntityCollider::Disabled;
    let origin = Position::new(0.5, 0.5, 0.5);
    let (id, expected) = place(&mut world, body, origin);
    let schemas = world.schemas().clone();
    let handle = world.entities().handle_of(id).unwrap();
    let motion = PlayerMotion {
        vertical_micros: 0,
        grounded: false,
    };
    world
        .entities_mut()
        .components_mut(handle)
        .unwrap()
        .insert(&schemas, &player_motion_schema(), motion.fields())
        .unwrap();
    let destination = Position::new(2.5, 0.5, 0.5);
    let relocation = grant(id)
        .view(&world)
        .plan_relocation(
            id,
            expected,
            destination,
            DestinationPolicy::Clear,
            TransactionId(1),
            ordering(),
        )
        .unwrap();
    let mut velocity = Transaction::new(TransactionId(2), collider_owner(), ordering());
    let next = PlayerMotion {
        vertical_micros: 1_000,
        grounded: false,
    };
    velocity.components.push(ComponentEdit {
        id,
        schema: player_motion_schema(),
        previous: Some(motion.fields()),
        fields: Some(next.fields()),
    });
    let group =
        TransactionGroup::new(TransactionId(3), ordering(), vec![relocation, velocity]).unwrap();
    world.commit_group(&group).unwrap();
    let stored = world
        .entities()
        .get(world.entities().handle_of(id).unwrap())
        .unwrap();
    assert_eq!(stored.id, id);
    assert_eq!(stored.position.to_bits(), destination.to_bits());
    assert_eq!(PlayerMotion::of(stored).unwrap(), Some(next));
    assert_eq!(EntityCollider::of(stored).unwrap(), Some(body));

    let again = grant(id)
        .view(&world)
        .plan_relocation(
            id,
            destination,
            Position::new(1.5, 0.5, 0.5),
            DestinationPolicy::Clear,
            TransactionId(4),
            ordering(),
        )
        .unwrap();
    let duplicate = again.clone();
    assert_eq!(
        TransactionGroup::new(TransactionId(5), ordering(), vec![again, duplicate]).err(),
        Some(RejectCode::InvalidData)
    );
    let handle = world.entities().handle_of(id).unwrap();
    let current = position(&world, id);
    let mut sweep = Transaction::new(TransactionId(6), key("base:motion"), ordering());
    sweep.move_entity(handle, id, current, [1.0, 0.0, 0.0], [0.3, 0.9, 0.3]);
    let relocated = grant(id)
        .view(&world)
        .plan_relocation(
            id,
            current,
            Position::new(1.5, 0.5, 0.5),
            DestinationPolicy::Clear,
            TransactionId(8),
            ordering(),
        )
        .unwrap();
    assert_eq!(
        TransactionGroup::new(TransactionId(7), ordering(), vec![relocated, sweep]).err(),
        Some(RejectCode::Conflict)
    );
}

#[test]
fn clear_destination_checks_final_terrain_in_single_transactions_and_groups() {
    for grouped in [false, true] {
        for policy in [DestinationPolicy::Clear, DestinationPolicy::Resident] {
            for body in [
                EntityCollider::Disabled,
                EntityCollider::Box { half: [200_000; 3] },
            ] {
                let mut world = world();
                load(&mut world, SectionPos::ZERO);
                let origin = Position::new(0.5, 2.5, 0.5);
                let (id, expected) = place(&mut world, body, origin);
                let mut relocation = grant(id)
                    .view(&world)
                    .plan_relocation(
                        id,
                        expected,
                        Position::new(2.5, 2.5, 0.5),
                        policy,
                        TransactionId(1),
                        ordering(),
                    )
                    .unwrap();
                let cell = BlockPos::new(2, 2, 0);
                let stone = world.registry().default_state(&key("base:stone")).unwrap();
                let mut terrain =
                    Transaction::new(TransactionId(2), key("test:terrain"), ordering());
                terrain.write_block(cell, stone);
                let result = if grouped {
                    world
                        .commit_group(
                            &TransactionGroup::new(
                                TransactionId(3),
                                ordering(),
                                vec![relocation, terrain],
                            )
                            .unwrap(),
                        )
                        .unwrap()
                } else {
                    relocation.mutations.extend(terrain.mutations);
                    world.commit(&relocation).unwrap()
                };
                if policy == DestinationPolicy::Clear {
                    assert_eq!(rejection(result), RejectCode::Conflict);
                    assert_eq!(position(&world, id), origin);
                    assert_eq!(
                        world.store().query(cell),
                        TerrainQuery::Ready(BlockStateId::EMPTY)
                    );
                    assert_eq!(world.seq(), CommitSeq::INITIAL);
                } else {
                    assert!(matches!(
                        result.outcome,
                        TransactionOutcome::Committed { .. }
                    ));
                    assert_eq!(position(&world, id), Position::new(2.5, 2.5, 0.5));
                }
            }
        }
    }
}

#[test]
fn relocation_checks_the_final_group_collider_and_rejects_terrain_only_preparation() {
    let mut world = world();
    load(&mut world, SectionPos::ZERO);
    let origin = Position::new(0.5, 2.5, 0.5);
    let (id, expected) = place(&mut world, EntityCollider::Disabled, origin);
    let stone = world.registry().default_state(&key("base:stone")).unwrap();
    world
        .store_mut()
        .edit(BlockPos::new(3, 2, 2), stone)
        .unwrap();
    let mut relocation = grant(id)
        .view(&world)
        .plan_relocation(
            id,
            expected,
            Position::new(2.5, 2.5, 2.5),
            DestinationPolicy::Clear,
            TransactionId(1),
            ordering(),
        )
        .unwrap();
    relocation.write_block(BlockPos::new(7, 2, 2), stone);
    let coordinator = torse_sim::CommitCoordinator::new(WorldId(1));
    assert!(matches!(
        coordinator.prepare(world.store(), &relocation),
        Err(torse_sim::PrepareError::Rejected(RejectCode::InvalidData))
    ));
    let mut resize = Transaction::new(TransactionId(2), collider_owner(), ordering());
    resize.components.push(ComponentEdit {
        id,
        schema: collider_schema(),
        previous: Some(EntityCollider::Disabled.fields()),
        fields: Some(EntityCollider::Box { half: [900_000; 3] }.fields()),
    });
    let group =
        TransactionGroup::new(TransactionId(3), ordering(), vec![relocation, resize]).unwrap();
    assert_eq!(
        rejection(world.commit_group(&group).unwrap()),
        RejectCode::Conflict
    );
    assert_eq!(position(&world, id), origin);
    assert_eq!(world.seq(), CommitSeq::INITIAL);
    assert_eq!(
        world.store().query(BlockPos::new(7, 2, 2)),
        TerrainQuery::Ready(BlockStateId::EMPTY)
    );
}

#[test]
fn prepared_relocation_rejects_a_changed_collider_without_publishing() {
    for grouped in [false, true] {
        let mut world = world();
        load(&mut world, SectionPos::ZERO);
        let origin = Position::new(0.5, 2.5, 0.5);
        let (id, expected) = place(&mut world, EntityCollider::Disabled, origin);
        let stone = world.registry().default_state(&key("base:stone")).unwrap();
        world
            .store_mut()
            .edit(BlockPos::new(3, 2, 2), stone)
            .unwrap();
        let relocation = grant(id)
            .view(&world)
            .plan_relocation(
                id,
                expected,
                Position::new(2.5, 2.5, 2.5),
                DestinationPolicy::Clear,
                TransactionId(1),
                ordering(),
            )
            .unwrap();
        let prepared = if grouped {
            world
                .prepare_group(
                    &TransactionGroup::new(TransactionId(2), ordering(), vec![relocation]).unwrap(),
                )
                .unwrap()
        } else {
            world.prepare(&relocation).unwrap()
        };
        let handle = world.entities().handle_of(id).unwrap();
        let schemas = world.schemas().clone();
        world
            .entities_mut()
            .components_mut(handle)
            .unwrap()
            .insert(
                &schemas,
                &collider_schema(),
                EntityCollider::Box { half: [900_000; 3] }.fields(),
            )
            .unwrap();
        assert!(world.publish(prepared).is_err());
        assert_eq!(position(&world, id), origin);
        assert_eq!(world.seq(), CommitSeq::INITIAL);
    }
}

#[test]
fn ordinary_movement_still_rejects_a_delta_past_the_limit() {
    let mut world = world();
    load(&mut world, SectionPos::ZERO);
    load(&mut world, SectionPos::new(1, 0, 0));
    let handle = world
        .entities_mut()
        .spawn(Position::new(0.5, 1.0, 0.5))
        .unwrap();
    let entity = world.entities().get(handle).unwrap();
    let id = entity.id;
    let previous = entity.position;
    let mut too_far = Transaction::new(TransactionId(1), key("base:motion"), ordering());
    too_far.move_entity(handle, id, previous, [17.0, 0.0, 0.0], [0.3, 0.9, 0.3]);
    assert_eq!(
        rejection(world.commit(&too_far).unwrap()),
        RejectCode::InvalidData
    );
    assert_eq!(position(&world, id).to_bits(), previous.to_bits());
    let mut allowed = Transaction::new(TransactionId(2), key("base:motion"), ordering());
    allowed.move_entity(handle, id, previous, [16.0, 0.0, 0.0], [0.3, 0.9, 0.3]);
    world.commit(&allowed).unwrap();
    assert!(position(&world, id).x > previous.x);
}

#[test]
fn a_second_relocation_in_one_transaction_is_rejected() {
    let mut world = world();
    load(&mut world, SectionPos::ZERO);
    let origin = Position::new(0.5, 0.5, 0.5);
    let (id, expected) = place(&mut world, EntityCollider::Disabled, origin);
    let mut tx = Transaction::new(
        TransactionId(1),
        torse_sim::action::relocation_source(),
        ordering(),
    );
    tx.relocate_entity(
        id,
        expected,
        Position::new(1.5, 0.5, 0.5),
        DestinationPolicy::Clear,
    );
    tx.relocate_entity(
        id,
        expected,
        Position::new(2.5, 0.5, 0.5),
        DestinationPolicy::Clear,
    );
    assert_eq!(
        rejection(world.commit(&tx).unwrap()),
        RejectCode::QuotaExceeded
    );
    assert_eq!(position(&world, id).to_bits(), origin.to_bits());
    assert_eq!(world.seq(), CommitSeq::INITIAL);
}

#[test]
fn a_relocation_with_a_forged_source_is_rejected() {
    let mut world = world();
    load(&mut world, SectionPos::ZERO);
    let origin = Position::new(0.5, 0.5, 0.5);
    let (id, expected) = place(&mut world, EntityCollider::Disabled, origin);
    let mut forged = Transaction::new(TransactionId(1), key("test:game"), ordering());
    forged.relocate_entity(
        id,
        expected,
        Position::new(1.5, 0.5, 0.5),
        DestinationPolicy::Clear,
    );
    assert_eq!(
        rejection(world.commit(&forged).unwrap()),
        RejectCode::PermissionDenied
    );
    assert_eq!(position(&world, id).to_bits(), origin.to_bits());
    assert_eq!(world.seq(), CommitSeq::INITIAL);
}
