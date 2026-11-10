//! Authoritative walking over bounded transports and a real durable journal.
mod support;
use support::Directory;
use torse_codec::wire;
use torse_net::{MemoryTransport, ReplicaMirror, Transport, TransportLimits};
use torse_protocol::{MoveCommand, PROTOCOL_VERSION};
use torse_server::{bootstrap as base, execution::*, network::*, *};
use torse_sim::{action::ActorContext, durable::ExecutionChanges, player::MoveInput};
use torse_types::*;
use torse_world::*;

fn create(dir: &Directory) -> base::Files {
    create_with(dir, |_| {})
}
fn create_with(
    dir: &Directory,
    initialize: impl FnOnce(&mut torse_sim::WorldAuthority),
) -> base::Files {
    let (mut world, actions) = base::world().unwrap();
    base::fill_floor(&mut world).unwrap();
    base::spawn_players(&mut world, 2).unwrap();
    initialize(&mut world);
    let compatibility = world.coordinator().compatibility();
    base::Files::create_file(
        dir.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            ExecutionHost::new(
                binding,
                world,
                actions,
                Vec::new(),
                base::execution_limits(),
            )
        },
    )
    .unwrap()
}
fn reopen(dir: &Directory) -> base::Files {
    let (world, actions) = base::world().unwrap();
    base::Files::open_file(
        dir.acquire(),
        world.coordinator().compatibility(),
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                world.registry(),
                world.schemas(),
                actions,
                Vec::new(),
                base::execution_limits(),
            ))
        },
    )
    .unwrap()
}
fn add(
    hub: &mut NetworkHub<MemoryTransport>,
    server: &base::Files,
    entity: Option<EntityId>,
) -> (u128, MemoryTransport) {
    let (host, client) = MemoryTransport::pair(TransportLimits {
        frame: 8_388_608,
        queued_frames: 256,
        queued_bytes: 16_777_216,
        poll_bytes: 262_144,
    })
    .unwrap();
    let id = hub
        .add(
            host,
            PeerPolicy {
                actor: ActorContext::new(base::key("test:player")),
                scope: base::scope(),
            },
            server.authority().unwrap().resume_point().tick,
        )
        .unwrap();
    if let Some(entity) = entity {
        hub.assign_player(id, entity).unwrap();
    }
    (id, client)
}
fn send(client: &mut MemoryTransport, message: ClientMessage) {
    let mut bytes = Vec::new();
    wire::encode(&message, &mut bytes, base::budget(), base::codec_limits()).unwrap();
    client.send(bytes).unwrap();
}
fn receive(client: &mut MemoryTransport) -> Vec<ServerMessage> {
    let mut out = Vec::new();
    while let Some(bytes) = client.receive().unwrap() {
        out.push(wire::decode(&bytes, base::budget(), base::codec_limits()).unwrap());
    }
    out
}
fn hello(client: &mut MemoryTransport, server: &base::Files) {
    send(
        client,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            compatibility: server.binding().compatibility,
        },
    );
}
fn movement(client: &mut MemoryTransport, session: u128, seq: u64, wish: [i8; 2], jump: bool) {
    send(
        client,
        ClientMessage::Move(MoveCommand {
            session: SessionId(session),
            seq,
            tick_hint: Tick(u64::MAX),
            wish,
            jump,
        }),
    );
}
fn pose(server: &base::Files, id: u128) -> Position {
    let entities = server.authority().unwrap().world().entities();
    entities
        .get(entities.handle_of(EntityId(id)).unwrap())
        .unwrap()
        .position
}
fn tick(server: &mut base::Files, hub: &mut NetworkHub<MemoryTransport>) {
    server.drive_tick(hub, 2048).unwrap();
    hub.poll(server).unwrap();
}

fn enable_follow(
    dir: &Directory,
    server: &mut base::Files,
    hub: &mut NetworkHub<MemoryTransport>,
    radius: u32,
) {
    let root = dir.0.join("sections");
    std::fs::create_dir_all(&root).unwrap();
    let pages = torse_server::section_pages::SectionPages::new(
        root,
        base::budget(),
        base::codec_limits(),
        64,
    )
    .unwrap();
    let initial: Vec<_> = server
        .authority()
        .unwrap()
        .world()
        .store()
        .positions()
        .collect();
    server
        .enable_residency(
            torse_server::residency::ResidencyConfig {
                max_resident: 4,
                max_io_bytes: 32 * 1024 * 1024,
            },
            pages,
            None,
            &initial,
        )
        .unwrap();
    hub.enable_player_follow(
        server,
        PlayerFollow {
            radius,
            max_sections: 3,
        },
    )
    .unwrap();
}

#[test]
fn following_unions_peer_interests_and_removes_disconnected_intentions() {
    let dir = Directory::new();
    let mut server = create_with(&dir, |world| {
        let handle = world.entities().handle_of(EntityId(4)).unwrap();
        world
            .entities_mut()
            .move_to(handle, Position::new(20.5, 3.0, 4.5))
            .unwrap();
    });
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    enable_follow(&dir, &mut server, &mut hub, 0);
    let (_, mut first) = add(&mut hub, &server, Some(EntityId(3)));
    let (second_id, mut second) = add(&mut hub, &server, Some(EntityId(4)));
    hello(&mut first, &server);
    hello(&mut second, &server);
    hub.poll(&mut server).unwrap();
    for client in [&mut first, &mut second] {
        assert!(
            receive(client)
                .iter()
                .any(|m| matches!(m, ServerMessage::Seed(s) if s.state.sections.len() == 1))
        );
    }
    assert_eq!(
        server.residency_interest().unwrap(),
        &base::scope().sections
    );
    hub.revoke(&mut server, second_id).unwrap();
    hub.poll(&mut server).unwrap();
    assert_eq!(
        server.residency_interest().unwrap(),
        &[SectionPos::ZERO].into()
    );
}

#[test]
fn following_retries_replica_pressure_without_replaying_movement() {
    let dir = Directory::new();
    let pool = MemoryPool::new(
        MemoryBudget::new(512 * 1024 * 1024)
            .with_class_limit(MemoryClass::Replica, 256 * 1024 * 1024),
    );
    let mut server = create(&dir);
    server
        .attach_memory(pool.clone(), 48 * 1024 * 1024)
        .unwrap();
    let mut hub =
        NetworkHub::with_memory(server.binding(), base::network_limits(), pool.clone()).unwrap();
    enable_follow(&dir, &mut server, &mut hub, 1);
    let (_, mut client) = add(&mut hub, &server, Some(EntityId(3)));
    let held = pool
        .reserve(
            MemoryClass::Replica,
            pool.budget().class_limit(MemoryClass::Replica)
                - pool.usage().current[MemoryClass::Replica.index()],
            EvictionRank::Pinned,
        )
        .unwrap();
    hello(&mut client, &server);
    let position = pose(&server, 3);
    for _ in 0..4 {
        hub.poll(&mut server).unwrap();
        assert!(
            !receive(&mut client)
                .iter()
                .any(|m| matches!(m, ServerMessage::Seed(_)))
        );
    }
    assert_eq!(pose(&server, 3), position);
    drop(held);
    hub.poll(&mut server).unwrap();
    assert!(
        receive(&mut client)
            .iter()
            .any(|m| matches!(m, ServerMessage::Seed(_)))
    );
    assert_eq!(pose(&server, 3), position);
    drop(hub);
    drop(server);
    assert_eq!(pool.usage().total, 0);
}

#[test]
fn following_retires_old_replica_loads_new_window_and_preserves_move_sequences() {
    let dir = Directory::new();
    let third = SectionPos::new(2, 0, 0);
    let mut server = create_with(&dir, |world| {
        let handle = world.entities().handle_of(EntityId(3)).unwrap();
        world
            .entities_mut()
            .move_to(handle, Position::new(15.6, 3.0, 4.5))
            .unwrap();
        let floor = world.store().section(SectionPos::ZERO).unwrap().clone();
        world.store_mut().request(third).unwrap();
        world.store_mut().install(third, floor).unwrap();
        world.store_mut().finish_load(third).unwrap();
    });
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    enable_follow(&dir, &mut server, &mut hub, 1);
    let (id, mut client) = add(&mut hub, &server, Some(EntityId(3)));
    let approved = torse_sim::replica::ReplicaScope {
        sections: [SectionPos::ZERO, SectionPos::new(1, 0, 0), third].into(),
    };
    hub.replace_policy(
        &mut server,
        id,
        PeerPolicy {
            actor: ActorContext::new(base::key("test:player")),
            scope: approved,
        },
    )
    .unwrap();
    hello(&mut client, &server);
    hub.poll(&mut server).unwrap();
    let old = receive(&mut client)
        .into_iter()
        .find_map(|m| match m {
            ServerMessage::Seed(seed) => {
                assert_eq!(seed.state.sections.len(), 2);
                Some(seed.subscription)
            }
            _ => None,
        })
        .unwrap();
    for _ in 0..200 {
        if server
            .authority()
            .unwrap()
            .parked_sections()
            .contains_key(&third)
        {
            break;
        }
        tick(&mut server, &mut hub);
        receive(&mut client);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        server
            .authority()
            .unwrap()
            .parked_sections()
            .contains_key(&third)
    );
    let mut replacement = None;
    let mut confirmed = 0;
    for seq in 1..=2 {
        movement(&mut client, 1, seq, [127, 0], false);
        hub.poll(&mut server).unwrap();
        // A previous idle step may already occupy this tick; the queued input
        // must still settle once, with its own monotonically confirmed sequence.
        tick(&mut server, &mut hub);
        tick(&mut server, &mut hub);
        for message in receive(&mut client) {
            match message {
                ServerMessage::Seed(seed) => replacement = Some(seed),
                ServerMessage::Player(player) => confirmed = confirmed.max(player.input_seq),
                _ => {}
            }
        }
    }
    assert_eq!(confirmed, 2);
    assert!((pose(&server, 3).x - 16.03).abs() < 1e-8);
    assert!(server.take_replica(old, base::budget()).is_err());
    let x = pose(&server, 3).x;
    for _ in 0..12 {
        hub.poll(&mut server).unwrap();
    }
    assert_eq!(pose(&server, 3).x, x);
    for _ in 0..200 {
        hub.poll(&mut server).unwrap();
        for message in receive(&mut client) {
            if let ServerMessage::Seed(seed) = message {
                replacement = Some(seed);
            }
        }
        if replacement.is_some() {
            break;
        }
        tick(&mut server, &mut hub);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let seed = replacement.expect("new window baseline");
    assert_eq!(seed.state.sections.len(), 3);
    assert_ne!(seed.subscription, old);
    send(
        &mut client,
        ClientMessage::Subscribe {
            sections: vec![SectionPos::new(99, 0, 0)],
        },
    );
    hub.poll(&mut server).unwrap();
    assert!(
        receive(&mut client)
            .iter()
            .any(|m| matches!(m, ServerMessage::Rejected(RejectCode::PermissionDenied)))
    );
    hub.revoke(&mut server, id).unwrap();
    let (_, mut reconnected) = add(&mut hub, &server, Some(EntityId(3)));
    hello(&mut reconnected, &server);
    hub.poll(&mut server).unwrap();
    assert!(
        receive(&mut reconnected)
            .iter()
            .any(|m| matches!(m, ServerMessage::Player(p) if p.input_seq == 0))
    );
}

#[test]
fn two_players_publish_collision_resolved_moves_with_independent_input_sequences() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (_, mut first) = add(&mut hub, &server, Some(EntityId(3)));
    let (_, mut second) = add(&mut hub, &server, Some(EntityId(4)));
    for client in [&mut first, &mut second] {
        hello(client, &server);
        send(
            client,
            ClientMessage::Subscribe {
                sections: base::scope().sections.into_iter().collect(),
            },
        );
    }
    movement(&mut first, 1, 1, [127, 0], false);
    send(
        &mut first,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: torse_sim::durable::Call {
                action: base::key("test:unknown-action"),
                version: 1,
                target: torse_sim::action::ActionTarget::Entity(EntityId(3)),
                parameters: Default::default(),
            },
        },
    );
    movement(&mut second, 3, 1, [127, 127], false);
    hub.poll(&mut server).unwrap();
    let seeds = [&mut first, &mut second].map(|client| {
        let messages = receive(client);
        if messages.iter().any(|m| {
            matches!(
                m,
                ServerMessage::Welcome {
                    session: SessionId(1),
                    ..
                }
            )
        }) {
            assert!(messages.iter().any(|m| matches!(
                m,
                ServerMessage::Admission {
                    seq: RequestSeq(1),
                    admission: Admission::Queued { .. }
                }
            )));
        }
        assert!(
            messages
                .iter()
                .all(|m| !matches!(m, ServerMessage::Player(p) if p.input_seq != 0))
        );
        messages
            .into_iter()
            .find_map(|m| {
                if let ServerMessage::Seed(seed) = m {
                    Some(ReplicaMirror::from_seed(seed).unwrap())
                } else {
                    None
                }
            })
            .unwrap()
    });
    let before = pose(&server, 3);
    tick(&mut server, &mut hub);
    let after = pose(&server, 3);
    assert!(
        (after.x - before.x - 0.215).abs() < 1e-8,
        "{before:?} -> {after:?}"
    );
    let diagonal = pose(&server, 4);
    assert!(((diagonal.x - 6.5).hypot(diagonal.z - 4.5) - 0.215).abs() < 1e-8);
    for (client, mut mirror) in [&mut first, &mut second].into_iter().zip(seeds) {
        let messages = receive(client);
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, ServerMessage::Player(p) if p.input_seq == 1 && !p.reset))
        );
        for message in messages {
            if let ServerMessage::Batch(batch) = message {
                mirror.apply(batch).unwrap();
            }
        }
        assert_eq!(
            mirror
                .state()
                .entities
                .iter()
                .find(|e| e.id == EntityId(3))
                .unwrap()
                .position_bits,
            after.to_bits()
        );
        assert_eq!(
            mirror
                .state()
                .entities
                .iter()
                .find(|e| e.id == EntityId(4))
                .unwrap()
                .position_bits,
            diagonal.to_bits()
        );
    }
    // Falling continues while input is silent, and eventually settles on the floor.
    for _ in 0..24 {
        tick(&mut server, &mut hub);
        receive(&mut first);
        receive(&mut second);
    }
    assert!(
        (pose(&server, 3).y - 1.9).abs() < 1e-5,
        "{:?}",
        pose(&server, 3)
    );
    let entities = server.authority().unwrap().world().entities();
    let entity = entities
        .get(entities.handle_of(EntityId(3)).unwrap())
        .unwrap();
    assert_eq!(
        PlayerMotion::of(entity).unwrap().unwrap(),
        PlayerMotion {
            vertical_micros: 0,
            grounded: true
        }
    );
    let saved = pose(&server, 3);
    drop(hub);
    drop(server);
    let reopened = reopen(&dir);
    assert_eq!(pose(&reopened, 3), saved);
    let entity = reopened
        .authority()
        .unwrap()
        .world()
        .entities()
        .iter()
        .find(|(_, e)| e.id == EntityId(3))
        .unwrap()
        .1;
    assert!(PlayerMotion::of(entity).unwrap().unwrap().grounded);
}

#[test]
fn overflow_duplicates_and_poll_frequency_never_increase_tick_speed() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let mut limits = base::network_limits();
    limits.frames_per_poll = 128;
    limits.requests_per_tick = 128;
    limits.movement_inputs = 2;
    limits.movement_bytes = 1024;
    let mut hub = NetworkHub::new(server.binding(), limits).unwrap();
    let (_, mut client) = add(&mut hub, &server, Some(EntityId(3)));
    hello(&mut client, &server);
    for seq in [4, 2, 3, 3, 5] {
        movement(&mut client, 1, seq, [127, 0], false);
    }
    hub.poll(&mut server).unwrap();
    assert!(
        receive(&mut client)
            .iter()
            .any(|m| matches!(m, ServerMessage::Player(p) if p.reset && p.input_seq == 0))
    );
    let before = pose(&server, 3);
    for _ in 0..8 {
        hub.poll(&mut server).unwrap();
    }
    assert_eq!(pose(&server, 3), before);
    tick(&mut server, &mut hub);
    let after = pose(&server, 3);
    assert!((after.x - before.x - 0.215).abs() < 1e-8);
    assert!(
        receive(&mut client)
            .iter()
            .any(|m| matches!(m, ServerMessage::Player(p) if p.input_seq == 4))
    );
    for _ in 0..16 {
        movement(&mut client, 1, 4, [-127, 0], true);
    }
    hub.poll(&mut server).unwrap();
    receive(&mut client);
    tick(&mut server, &mut hub);
    receive(&mut client);
    assert!((pose(&server, 3).x - before.x - 0.43).abs() < 1e-8);
    tick(&mut server, &mut hub);
    receive(&mut client);
    assert!((pose(&server, 3).x - before.x - 0.43).abs() < 1e-8);
}

#[test]
fn reconnect_revokes_frozen_movement_and_old_sessions_cannot_move_the_new_player() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (id, mut client) = add(&mut hub, &server, Some(EntityId(3)));
    hello(&mut client, &server);
    movement(&mut client, 1, 1, [127, 0], true);
    hub.poll(&mut server).unwrap();
    receive(&mut client);
    // Publish the simulation plan, but disconnect before its task commits.
    while !server
        .authority()
        .unwrap()
        .runtime()
        .work
        .as_ref()
        .is_some_and(|w| {
            w.tasks.iter().any(|t| {
                t.request
                    .as_ref()
                    .is_some_and(|r| r.input.action == movement_action())
            })
        })
    {
        server.drive_one(&mut hub).unwrap();
    }
    let before = pose(&server, 3);
    hub.revoke(&mut server, id).unwrap();
    tick(&mut server, &mut hub);
    assert_eq!(pose(&server, 3), before);
    let (new_id, mut new_client) = add(&mut hub, &server, Some(EntityId(3)));
    hello(&mut new_client, &server);
    hub.poll(&mut server).unwrap();
    let messages = receive(&mut new_client);
    assert!(messages.iter().any(|m| matches!(
        m,
        ServerMessage::Welcome {
            session: SessionId(3),
            ..
        }
    )));
    movement(&mut new_client, 1, 2, [127, 0], false);
    hub.poll(&mut server).unwrap();
    assert!(!hub.contains(new_id));
    tick(&mut server, &mut hub);
    assert_eq!(pose(&server, 3), before);
}

#[test]
fn frozen_scope_replacement_and_reserved_action_cannot_bypass_player_authority() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (id, mut client) = add(&mut hub, &server, Some(EntityId(3)));
    hello(&mut client, &server);
    movement(&mut client, 1, 1, [127, 0], false);
    send(
        &mut client,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: movement_call(
                EntityId(4),
                MoveInput {
                    seq: 1,
                    wish: [127, 0, 0],
                    jump: true,
                },
            ),
        },
    );
    hub.poll(&mut server).unwrap();
    assert!(receive(&mut client).iter().any(|m| matches!(
        m,
        ServerMessage::Admission {
            admission: Admission::Rejected(RejectCode::PermissionDenied),
            ..
        }
    )));
    while !server
        .authority()
        .unwrap()
        .runtime()
        .work
        .as_ref()
        .is_some_and(|w| {
            w.tasks.iter().any(|t| {
                t.request
                    .as_ref()
                    .is_some_and(|r| r.input.action == movement_action())
            })
        })
    {
        server.drive_one(&mut hub).unwrap();
    }
    let before = pose(&server, 3);
    hub.replace_policy(
        &mut server,
        id,
        PeerPolicy {
            actor: ActorContext::new(base::key("test:player")),
            scope: torse_sim::replica::ReplicaScope {
                sections: [SectionPos::new(1, 0, 0)].into(),
            },
        },
    )
    .unwrap();
    tick(&mut server, &mut hub);
    assert_eq!(pose(&server, 3), before);
    assert!(!hub.contains(id));
}

#[test]
fn restart_rejects_old_epoch_frozen_intent_even_when_session_ids_are_reused() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (_, mut client) = add(&mut hub, &server, Some(EntityId(3)));
    hello(&mut client, &server);
    movement(&mut client, 1, 1, [127, 0], true);
    hub.poll(&mut server).unwrap();
    receive(&mut client);
    while !server
        .authority()
        .unwrap()
        .runtime()
        .work
        .as_ref()
        .is_some_and(|w| {
            w.tasks.iter().any(|t| {
                t.request
                    .as_ref()
                    .is_some_and(|r| r.input.action == movement_action())
            })
        })
    {
        server.drive_one(&mut hub).unwrap();
    }
    let before = pose(&server, 3);
    let epoch = server.binding().epoch;
    drop(hub);
    drop(server);
    let mut server = reopen(&dir);
    assert!(server.binding().epoch.0 > epoch.0);
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (_, mut client) = add(&mut hub, &server, Some(EntityId(3)));
    hello(&mut client, &server);
    hub.poll(&mut server).unwrap();
    receive(&mut client);
    tick(&mut server, &mut hub);
    receive(&mut client);
    assert_eq!(pose(&server, 3), before);
}

#[test]
fn body_collision_jump_and_interest_replacement_preserve_the_live_session() {
    let dir = Directory::new();
    let mut server = create_with(&dir, |world| {
        let stone = world
            .registry()
            .default_state(&base::key("base:stone"))
            .unwrap();
        for y in 1..5 {
            for z in 4..6 {
                world
                    .store_mut()
                    .edit(BlockPos::new(5, y, z), stone)
                    .unwrap();
            }
        }
    });
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (id, mut client) = add(&mut hub, &server, Some(EntityId(3)));
    hello(&mut client, &server);
    send(
        &mut client,
        ClientMessage::Subscribe {
            sections: base::scope().sections.into_iter().collect(),
        },
    );
    movement(&mut client, 1, 1, [127, 0], false);
    hub.poll(&mut server).unwrap();
    let old_seed = receive(&mut client)
        .into_iter()
        .find_map(|message| {
            if let ServerMessage::Seed(seed) = message {
                Some(seed)
            } else {
                None
            }
        })
        .unwrap();
    for _ in 0..20 {
        tick(&mut server, &mut hub);
        receive(&mut client);
    }
    let floor = pose(&server, 3);
    assert!((floor.x - 4.7).abs() < 1e-5);
    assert!((floor.y - 1.9).abs() < 1e-5);
    hub.replace_policy(
        &mut server,
        id,
        PeerPolicy {
            actor: ActorContext::new(base::key("test:player")),
            scope: torse_sim::replica::ReplicaScope {
                sections: [SectionPos::ZERO].into(),
            },
        },
    )
    .unwrap();
    let replacement = receive(&mut client)
        .into_iter()
        .find_map(|message| {
            if let ServerMessage::Interest(batch) = message {
                Some(batch)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(replacement.subscription, old_seed.subscription);
    assert!(
        server
            .take_replica(old_seed.subscription, base::budget())
            .unwrap()
            .is_empty()
    );
    assert!(replacement.changes.sections.is_empty());
    assert_eq!(
        replacement.changes.removed_sections,
        vec![SectionPos::new(1, 0, 0)]
    );
    movement(&mut client, 1, 2, [0, 0], true);
    hub.poll(&mut server).unwrap();
    receive(&mut client);
    tick(&mut server, &mut hub);
    receive(&mut client);
    tick(&mut server, &mut hub);
    assert!(pose(&server, 3).y > floor.y + 0.3);
    assert!(receive(&mut client).iter().any(|m| matches!(m, ServerMessage::Player(p) if p.session == SessionId(1) && p.input_seq == 2 && !p.reset)));
    assert!(hub.contains(id));
}

struct FlushControl<J> {
    inner: J,
    busy: std::rc::Rc<std::cell::Cell<bool>>,
    fail: std::rc::Rc<std::cell::Cell<bool>>,
}
impl<J: torse_storage::CommitJournal<ExecutionModel>> torse_storage::CommitJournal<ExecutionModel>
    for FlushControl<J>
{
    type Staged = J::Staged;
    fn try_stage(
        &mut self,
        record: &CommitRecord<ExecutionChanges>,
    ) -> Result<Self::Staged, StageError> {
        if self.busy.get() {
            Err(StageError::Busy)
        } else {
            self.inner.try_stage(record)
        }
    }
    fn accept(&mut self, staged: Self::Staged) -> Result<(), Fault> {
        self.inner.accept(staged)
    }
    fn flush_through(&mut self, cursor: CommitCursor) -> Result<Durability, Fault> {
        if self.fail.get() {
            Err(Fault::new(
                FaultCode::StorageUnavailable,
                "injected movement flush failure",
            ))
        } else {
            self.inner.flush_through(cursor)
        }
    }
    fn durability(&self) -> Durability {
        self.inner.durability()
    }
}
#[test]
fn movement_never_acknowledges_unreserved_or_failed_durable_publication() {
    let dir = Directory::new();
    let directory = dir.acquire();
    let (mut world, actions) = base::world().unwrap();
    base::fill_floor(&mut world).unwrap();
    base::spawn_players(&mut world, 1).unwrap();
    let binding = Binding {
        world: base::WORLD,
        epoch: directory.epoch(),
        compatibility: world.coordinator().compatibility(),
    };
    let codec = base::Codec::new(base::codec_limits()).unwrap();
    let policy = base::file_policy();
    let busy = std::rc::Rc::new(std::cell::Cell::new(false));
    let fail = std::rc::Rc::new(std::cell::Cell::new(false));
    let journal = FlushControl {
        inner: torse_storage::MemoryJournal::memory(
            binding,
            CommitCursor::initial(base::WORLD),
            codec,
            policy.journal,
            64_000_000,
        )
        .unwrap(),
        busy: busy.clone(),
        fail: fail.clone(),
    };
    let checkpoints = torse_storage::Checkpoints::open(
        binding,
        codec,
        policy.checkpoint,
        torse_storage::MemoryCheckpointBackend::new(64_000_000),
    )
    .unwrap();
    let host = ExecutionHost::new(
        binding,
        world,
        actions,
        Vec::new(),
        base::execution_limits(),
    )
    .unwrap();
    let mut server =
        Server::initialize(directory, host, journal, checkpoints, policy.server).unwrap();
    let mut hub = NetworkHub::new(binding, base::network_limits()).unwrap();
    let (host, mut client) = MemoryTransport::pair(TransportLimits {
        frame: 8_388_608,
        queued_frames: 32,
        queued_bytes: 16_777_216,
        poll_bytes: 262_144,
    })
    .unwrap();
    let peer = hub
        .add(
            host,
            PeerPolicy {
                actor: ActorContext::new(base::key("test:player")),
                scope: base::scope(),
            },
            Tick(0),
        )
        .unwrap();
    hub.assign_player(peer, EntityId(3)).unwrap();
    send(
        &mut client,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            compatibility: binding.compatibility,
        },
    );
    movement(&mut client, 1, 1, [127, 0], true);
    hub.poll(&mut server).unwrap();
    receive(&mut client);
    busy.set(true);
    assert!(matches!(
        server.drive_one(&mut hub).unwrap(),
        CommitStep::Blocked(StageError::Busy)
    ));
    hub.poll(&mut server).unwrap();
    assert!(receive(&mut client).is_empty());
    busy.set(false);
    while !server
        .authority()
        .unwrap()
        .runtime()
        .work
        .as_ref()
        .is_some_and(|w| {
            w.tasks.iter().any(|t| {
                t.request
                    .as_ref()
                    .is_some_and(|r| r.input.action == movement_action())
            })
        })
    {
        server.drive_one(&mut hub).unwrap();
    }
    fail.set(true);
    assert!(server.drive_one(&mut hub).is_err());
    assert_eq!(server.state(), ServerState::Faulted);
    assert!(hub.poll(&mut server).is_err());
    assert!(receive(&mut client).is_empty());
}
