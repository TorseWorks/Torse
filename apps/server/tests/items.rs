//! The demonstration item policy uses the ordinary durable and authorized paths.
mod support;
use support::Directory;
use torse_inventory::Inventory;
use torse_items::{edit_call, fixture_catalog, key};
use torse_server::{bootstrap as base, execution::*, *};
use torse_sim::action::{ActionTarget, ActorContext};
use torse_sim::durable::ReplyStatus;
use torse_types::*;

fn stock(server: &base::Files, id: EntityId) -> Inventory {
    let world = server.authority().unwrap().world();
    let catalog = fixture_catalog(world.registry()).unwrap();
    let entity = world
        .entities()
        .get(world.entities().handle_of(id).unwrap())
        .unwrap();
    Inventory::from_fields(
        entity
            .components
            .component(&torse_inventory::schema())
            .unwrap(),
        catalog.stack_rules(),
    )
    .unwrap()
}

#[test]
fn physical_drops_recover_and_pickup_is_scoped_and_durable() {
    let directory = Directory::new();
    let (mut world, actions) = base::world().unwrap();
    base::fill_floor(&mut world).unwrap();
    let player = base::spawn_players(&mut world, 1).unwrap()[0];
    let pos = BlockPos::new(4, 2, 6);
    let stone = world.registry().default_state(&key("base:stone")).unwrap();
    world.store_mut().edit(pos, stone).unwrap();
    let compatibility = world.coordinator().compatibility();
    let rules = vec![Box::new(torse_server::item_drops::DropPhysics::new(
        fixture_catalog(world.registry())
            .unwrap()
            .with_world_drops(),
    )) as Box<dyn DurableRule>];
    let mut server = base::Files::create_file(
        directory.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| ExecutionHost::new(binding, world, actions, rules, base::execution_limits()),
    )
    .unwrap();
    let actor = [
        "world:permission/edit",
        "items:permission/use",
        "inventory:permission/insert",
        "items:permission/pickup",
        "world:permission/spawn",
        "world:permission/despawn",
    ]
    .into_iter()
    .fold(
        ActorContext::new(key("test:owner"))
            .target(ActionTarget::Entity(player))
            .section(SectionPos::ZERO),
        |actor, p| actor.permission(key(p)),
    );
    server
        .register_session(
            Viewer {
                session: SessionId(1),
                actor: ActorContextId(1),
            },
            actor,
            base::scope(),
        )
        .unwrap();
    let before = stock(&server, player);
    let first = RequestId {
        world: base::WORLD,
        epoch: server.binding().epoch,
        session: SessionId(1),
        seq: RequestSeq(1),
    };
    assert!(matches!(
        server.submit(WorldRequest {
            id: first,
            actor: ActorContextId(1),
            input: edit_call(player, false, 2, pos, 0)
        }),
        Admission::Queued { .. }
    ));
    for _ in 0..25 {
        server.drive_tick(&mut NoReplicas, 1024).unwrap();
    }
    assert_eq!(
        server.receipt(first).unwrap().unwrap().reply.status,
        ReplyStatus::Committed
    );
    assert_eq!(stock(&server, player), before);
    let drop_id = server
        .authority()
        .unwrap()
        .world()
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
    drop(server);
    let (world, actions) = base::world().unwrap();
    let rules = vec![Box::new(torse_server::item_drops::DropPhysics::new(
        fixture_catalog(world.registry())
            .unwrap()
            .with_world_drops(),
    )) as Box<dyn DurableRule>];
    let mut server = base::Files::open_file(
        directory.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                world.registry(),
                world.schemas(),
                actions,
                rules,
                base::execution_limits(),
            ))
        },
    )
    .unwrap();
    let actor = [
        "items:permission/pickup",
        "inventory:permission/insert",
        "world:permission/despawn",
    ]
    .into_iter()
    .fold(
        ActorContext::new(key("test:owner"))
            .target(ActionTarget::Entity(player))
            .section(SectionPos::ZERO),
        |actor, p| actor.permission(key(p)),
    );
    server
        .register_session(
            Viewer {
                session: SessionId(2),
                actor: ActorContextId(2),
            },
            actor,
            base::scope(),
        )
        .unwrap();
    let pickup = RequestId {
        world: base::WORLD,
        epoch: server.binding().epoch,
        session: SessionId(2),
        seq: RequestSeq(1),
    };
    let call = torse_items::pickup_call(player, drop_id);
    assert!(matches!(
        server.submit(WorldRequest {
            id: pickup,
            actor: ActorContextId(2),
            input: call.clone()
        }),
        Admission::Queued { .. }
    ));
    server.drive_tick(&mut NoReplicas, 1024).unwrap();
    assert_eq!(
        server.receipt(pickup).unwrap().unwrap().reply.status,
        ReplyStatus::Committed
    );
    assert!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .handle_of(drop_id)
            .is_none()
    );
    assert_eq!(
        stock(&server, player).slots()[4].as_ref().unwrap().item,
        key("base:stone")
    );
    assert_eq!(
        server.submit(WorldRequest {
            id: pickup,
            actor: ActorContextId(2),
            input: call
        }),
        Admission::Duplicate
    );
}
#[test]
fn item_groups_are_durable_deduplicated_and_projected_only_to_the_owner() {
    let directory = Directory::new();
    let (mut world, actions) = base::world().unwrap();
    let player = base::spawn_players(&mut world, 1).unwrap()[0];
    let compatibility = world.coordinator().compatibility();
    let mut server = base::Files::create_file(
        directory.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| ExecutionHost::new(binding, world, actions, vec![], base::execution_limits()),
    )
    .unwrap();
    let actor = [
        "world:permission/edit",
        "items:permission/use",
        "inventory:permission/insert",
        "inventory:permission/consume",
        "inventory:permission/swap",
    ]
    .into_iter()
    .fold(
        ActorContext::new(key("test:owner"))
            .target(ActionTarget::Entity(player))
            .section(SectionPos::ZERO),
        |actor, permission| actor.permission(key(permission)),
    );
    let viewer = Viewer {
        session: SessionId(1),
        actor: ActorContextId(1),
    };
    server
        .register_session(viewer, actor, base::scope())
        .unwrap();
    server
        .register_session(
            Viewer {
                session: SessionId(2),
                actor: ActorContextId(2),
            },
            ActorContext::new(key("test:observer")),
            base::scope(),
        )
        .unwrap();
    let owner = server
        .open_replica(SessionId(1), base::scope(), base::budget())
        .unwrap();
    let observer = server
        .open_replica(SessionId(2), base::scope(), base::budget())
        .unwrap();
    assert!(
        owner
            .state
            .entities
            .iter()
            .find(|entity| entity.id == player)
            .unwrap()
            .components
            .contains_key(&torse_inventory::schema())
    );
    assert!(
        !observer
            .state
            .entities
            .iter()
            .find(|entity| entity.id == player)
            .unwrap()
            .components
            .contains_key(&torse_inventory::schema())
    );
    let before = stock(&server, player);
    let pos = BlockPos::new(8, 8, 8);
    let request = WorldRequest {
        id: RequestId {
            world: base::WORLD,
            epoch: server.binding().epoch,
            session: SessionId(1),
            seq: RequestSeq(1),
        },
        actor: ActorContextId(1),
        input: edit_call(player, true, 2, pos, 0),
    };
    let duplicate = || WorldRequest {
        id: request.id,
        actor: request.actor,
        input: request.input.clone(),
    };
    assert!(matches!(
        server.submit(duplicate()),
        Admission::Queued { .. }
    ));
    assert_eq!(server.submit(duplicate()), Admission::Duplicate);
    server.drive_tick(&mut NoReplicas, 1024).unwrap();
    assert_eq!(
        server.receipt(request.id).unwrap().unwrap().reply.status,
        ReplyStatus::Committed
    );
    let after = stock(&server, player);
    assert_eq!(
        after.slots()[2].as_ref().unwrap().count,
        before.slots()[2].as_ref().unwrap().count - 1
    );
    assert_eq!(server.submit(duplicate()), Admission::Duplicate);
    let own_delta = server
        .take_replica(owner.subscription, base::budget())
        .unwrap();
    let other_delta = server
        .take_replica(observer.subscription, base::budget())
        .unwrap();
    assert!(
        own_delta
            .iter()
            .flat_map(|batch| &batch.changes.entities)
            .any(|entity| entity.id == player
                && entity.components.contains_key(&torse_inventory::schema()))
    );
    assert!(
        other_delta
            .iter()
            .flat_map(|batch| &batch.changes.entities)
            .all(|entity| !entity.components.contains_key(&torse_inventory::schema()))
    );
    // Drop without a shutdown checkpoint: reopen must replay the mixed journal tail.
    drop(server);
    let (world, actions) = base::world().unwrap();
    let recovered = base::Files::open_file(
        directory.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                world.registry(),
                world.schemas(),
                actions,
                vec![],
                base::execution_limits(),
            ))
        },
    )
    .unwrap();
    assert_eq!(stock(&recovered, player), after);
    assert!(
        !recovered
            .authority()
            .unwrap()
            .world()
            .store()
            .query(pos)
            .ready()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        recovered.receipt(request.id).unwrap().unwrap().reply.status,
        ReplyStatus::Committed
    );
    assert_ne!(recovered.binding().epoch, request.id.epoch);
}
