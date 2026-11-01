//! End-to-end B6/B7 acceptance with real files, two clients and process aborts.
mod support;
use std::cell::Cell;
use std::rc::Rc;
use support::Directory;
use torse_codec::{StorageCodec, wire};
use torse_net::{MemoryTransport, ReplicaMirror, Transport, TransportLimits};
use torse_protocol::PROTOCOL_VERSION;
use torse_server::{bootstrap as base, execution::*, network::*, *};
use torse_sim::action::{ActionTarget, ActorContext};
use torse_sim::durable::*;
use torse_sim::{OrderingKey, Phase, SystemSpec, Transaction, WorldAuthority};
use torse_types::*;
use torse_world::*;

fn key(v: &str) -> ContentKey {
    base::key(v)
}
fn create(dir: &Directory, rules: Vec<Box<dyn DurableRule>>) -> base::Files {
    create_with_stack_data(dir, rules, ComponentFields::new())
}
fn create_with_stack_data(
    dir: &Directory,
    rules: Vec<Box<dyn DurableRule>>,
    data: ComponentFields,
) -> base::Files {
    let (mut world, actions) = base::world().unwrap();
    let catalog = torse_inventory::ItemCatalog::new([(key("base:stone"), 64)]).unwrap();
    let handle = world.entities().handle_of(EntityId(1)).unwrap();
    let fields = inventory(&world, EntityId(1));
    let mut stock = torse_inventory::Inventory::from_fields(&fields, &catalog).unwrap();
    let mut stack = stock.slots()[0].clone().unwrap();
    stack.data = data;
    stock.set(0, Some(stack), &catalog).unwrap();
    let schemas = world.schemas().clone();
    world
        .entities_mut()
        .components_mut(handle)
        .unwrap()
        .insert(&schemas, &torse_inventory::schema(), stock.fields())
        .unwrap();
    let compatibility = world.coordinator().compatibility();
    let config = base::file_policy();
    base::Files::create_file(
        dir.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        config,
        |binding| ExecutionHost::new(binding, world, actions, rules, base::execution_limits()),
    )
    .unwrap()
}
fn reopen(dir: &Directory, rules: Vec<Box<dyn DurableRule>>) -> Result<base::Files, Fault> {
    let (world, actions) = base::world().unwrap();
    let compatibility = world.coordinator().compatibility();
    let config = base::file_policy();
    base::Files::open_file(
        dir.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits())?,
        config,
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
}
fn actor() -> ActorContext {
    ActorContext::new(key("test:player"))
        .permission(key("inventory:permission/read"))
        .permission(key("inventory:permission/transfer"))
        .permission(key("world:permission/edit"))
        .target(ActionTarget::Entity(EntityId(1)))
        .target(ActionTarget::Entity(EntityId(2)))
        .target(ActionTarget::Block(BlockPos::new(8, 1, 1)))
}
fn viewer() -> Viewer {
    Viewer {
        session: SessionId(1),
        actor: ActorContextId(1),
    }
}
fn session(server: &mut base::Files) {
    server
        .register_session(viewer(), actor(), base::scope())
        .unwrap();
}

#[test]
fn scope_replacement_invalidates_queued_old_views_and_preserves_request_sequence() {
    use torse_sim::replica::ReplicaScope;
    let dir = Directory::new();
    let mut server = create(&dir, Vec::new());
    session(&mut server);
    let near = ReplicaScope {
        sections: [SectionPos::ZERO].into(),
    };
    let far = ReplicaScope {
        sections: [SectionPos::new(1, 0, 0)].into(),
    };
    let old: Vec<_> = (0..2)
        .map(|_| {
            server
                .open_replica(SessionId(1), near.clone(), base::budget())
                .unwrap()
                .subscription
        })
        .collect();
    server
        .register_session(
            Viewer {
                session: SessionId(2),
                actor: ActorContextId(2),
            },
            actor(),
            near.clone(),
        )
        .unwrap();
    let control = server
        .open_replica(SessionId(2), near.clone(), base::budget())
        .unwrap();
    server.submit(request(&server, 1, transfer()));
    tick(&mut server);
    assert!(
        !server
            .take_replica(control.subscription, base::budget())
            .unwrap()
            .is_empty()
    );
    let replacement = ActorContext::new(key("test:player"))
        .permission(key("world:permission/edit"))
        .section(SectionPos::new(1, 0, 0));
    let invalidated = server
        .replace_session_scope(SessionId(1), replacement, far.clone())
        .unwrap();
    assert_eq!(invalidated, old);
    for id in &old {
        assert_eq!(
            server.take_replica(*id, base::budget()).unwrap_err().code,
            FaultCode::ResetRequired
        );
        server.close_replica(*id).unwrap();
    }
    assert!(
        server
            .open_replica(SessionId(1), near, base::budget())
            .is_err()
    );
    let seed = server
        .open_replica(SessionId(1), far, base::budget())
        .unwrap();
    assert_eq!(seed.state.sections.len(), 1);
    assert!(seed.state.entities.is_empty());
    assert_eq!(
        server.submit(request(&server, 1, transfer())),
        Admission::Duplicate
    );
    let place = |target| Call {
        action: key("world:action/place"),
        version: 1,
        target: ActionTarget::Block(target),
        parameters: [(
            key("world:field/block"),
            ComponentValue::Content(key("base:stone")),
        )]
        .into(),
    };
    let denied = request(&server, 2, place(BlockPos::new(8, 1, 1)));
    let denied_id = denied.id;
    let accepted = request(&server, 3, place(BlockPos::new(17, 1, 1)));
    let accepted_id = accepted.id;
    server.submit(denied);
    server.submit(accepted);
    tick(&mut server);
    assert_eq!(
        server.receipt(denied_id).unwrap().unwrap().reply.status,
        ReplyStatus::Rejected(RejectCode::PermissionDenied)
    );
    assert_eq!(
        server.receipt(accepted_id).unwrap().unwrap().reply.status,
        ReplyStatus::Committed
    );
    assert!(
        !server
            .take_replica(seed.subscription, base::budget())
            .unwrap()
            .is_empty()
    );
    for id in old {
        assert_eq!(
            server.take_replica(id, base::budget()).unwrap_err().code,
            FaultCode::ResetRequired
        );
    }
}

#[test]
fn rejected_scope_replacement_retains_authority_and_live_replica_queues() {
    use torse_sim::replica::ReplicaScope;
    let dir = Directory::new();
    let mut server = create(&dir, Vec::new());
    session(&mut server);
    let old = server
        .open_replica(SessionId(1), base::scope(), base::budget())
        .unwrap();
    let oversized = ReplicaScope {
        sections: (0..5).map(|x| SectionPos::new(x, 0, 0)).collect(),
    };
    assert!(
        server
            .replace_session_scope(SessionId(1), ActorContext::new(key("test:none")), oversized)
            .is_err()
    );
    let mut oversized_actor = ActorContext::new(key("test:none"));
    for x in 0..=torse_sim::action::MAX_ACTOR_SECTIONS {
        oversized_actor = oversized_actor.section(SectionPos::new(x as i64, 0, 0));
    }
    assert!(
        server
            .replace_session_scope(SessionId(1), oversized_actor, base::scope())
            .is_err()
    );
    assert!(
        server
            .replace_session_scope(SessionId(99), actor(), base::scope())
            .is_err()
    );
    let queued = request(&server, 1, transfer());
    let id = queued.id;
    server.submit(queued);
    tick(&mut server);
    assert_eq!(
        server.receipt(id).unwrap().unwrap().reply.status,
        ReplyStatus::Committed
    );
    assert!(
        !server
            .take_replica(old.subscription, base::budget())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        server.submit(request(&server, 1, transfer())),
        Admission::Duplicate
    );
}

#[test]
fn network_scope_replacement_reseeds_and_keeps_the_session_alive() {
    use torse_sim::replica::ReplicaScope;
    let dir = Directory::new();
    let mut server = create(&dir, Vec::new());
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (peer, mut client) = add(&mut hub, &server, actor());
    hello(&mut client, server.binding());
    hub.poll(&mut server).unwrap();
    receive(&mut client);
    send(
        &mut client,
        ClientMessage::Subscribe {
            sections: base::scope().sections.into_iter().collect(),
        },
    );
    hub.poll(&mut server).unwrap();
    let old = seed(receive(&mut client));
    let far = ReplicaScope {
        sections: [SectionPos::new(1, 0, 0)].into(),
    };
    hub.replace_policy(
        &mut server,
        peer,
        PeerPolicy {
            actor: ActorContext::new(key("test:observer")),
            scope: far,
        },
    )
    .unwrap();
    let fresh = seed(receive(&mut client));
    assert_ne!(fresh.subscription, old.subscription);
    assert_eq!(fresh.state.sections.len(), 1);
    assert!(fresh.state.entities.is_empty());
    assert_eq!(
        server
            .take_replica(old.subscription, base::budget())
            .unwrap_err()
            .code,
        FaultCode::ResetRequired
    );
    let oversized = ReplicaScope {
        sections: (0..5).map(|x| SectionPos::new(x, 0, 0)).collect(),
    };
    assert!(
        hub.replace_policy(
            &mut server,
            peer,
            PeerPolicy {
                actor: actor(),
                scope: oversized
            }
        )
        .is_err()
    );
    server.drive_tick(&mut hub, 1024).unwrap();
    hub.poll(&mut server).unwrap();
    assert!(hub.contains(peer));
    send(&mut client, ClientMessage::Ping);
    hub.poll(&mut server).unwrap();
    assert!(hub.contains(peer));
}

#[test]
fn failed_network_reseed_closes_connection_and_revokes_replaced_session() {
    let dir = Directory::new();
    let mut server = create(&dir, Vec::new());
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (host, mut client) = MemoryTransport::pair(TransportLimits {
        frame: 8_388_608,
        queued_frames: 1,
        queued_bytes: 16_777_216,
        poll_bytes: 262_144,
    })
    .unwrap();
    let peer = hub
        .add(
            host,
            PeerPolicy {
                actor: actor(),
                scope: base::scope(),
            },
            Tick(0),
        )
        .unwrap();
    hello(&mut client, server.binding());
    hub.poll(&mut server).unwrap();
    receive(&mut client);
    send(
        &mut client,
        ClientMessage::Subscribe {
            sections: base::scope().sections.into_iter().collect(),
        },
    );
    hub.poll(&mut server).unwrap();
    // Keep the old seed queued so replacement output has no available slot.
    assert!(
        hub.replace_policy(
            &mut server,
            peer,
            PeerPolicy {
                actor: ActorContext::new(key("test:observer")),
                scope: base::scope()
            }
        )
        .is_err()
    );
    assert!(!hub.contains(peer));
    assert!(client.receive().is_err());
    assert!(
        server
            .open_replica(SessionId(1), base::scope(), base::budget())
            .is_err()
    );
}
fn transfer() -> Call {
    Call {
        action: key("inventory:action/transfer"),
        version: 2,
        target: ActionTarget::Entity(EntityId(1)),
        parameters: [
            (key("inventory:field/source_slot"), ComponentValue::U32(0)),
            (
                key("inventory:field/destination"),
                ComponentValue::Entity(EntityId(2)),
            ),
            (
                key("inventory:field/destination_slot"),
                ComponentValue::U32(0),
            ),
            (key("inventory:field/count"), ComponentValue::U32(1)),
        ]
        .into(),
    }
}
fn request(server: &base::Files, seq: u64, call: Call) -> WorldRequest<Call> {
    WorldRequest {
        id: RequestId {
            world: base::WORLD,
            epoch: server.binding().epoch,
            session: SessionId(1),
            seq: RequestSeq(seq),
        },
        actor: ActorContextId(1),
        input: call,
    }
}
fn inventory(world: &WorldAuthority, id: EntityId) -> ComponentFields {
    world
        .entities()
        .get(world.entities().handle_of(id).unwrap())
        .unwrap()
        .components
        .component(&torse_inventory::schema())
        .unwrap()
        .clone()
}
fn tick(server: &mut base::Files) {
    server.drive_tick(&mut NoReplicas, 1024).unwrap();
}

struct ClockRule(Rc<Cell<u32>>);
impl DurableRule for ClockRule {
    fn spec(&self) -> SystemSpec {
        SystemSpec {
            id: key("test:clock"),
            phase: Phase::Simulation,
            rule_priority: 0,
            before: Vec::new(),
            after: Vec::new(),
        }
    }
    fn plan(&self, _: &WorldAuthority, tick: Tick) -> Result<Vec<WorkPlan>, Fault> {
        if tick == Tick(0) {
            Ok(vec![WorkPlan {
                transaction: None,
                schedule: vec![EventDraft {
                    tick: Tick(2),
                    rule: key("test:clock"),
                    payload: ComponentFields::new(),
                }],
                cancel: Vec::new(),
            }])
        } else {
            Ok(Vec::new())
        }
    }
    fn event(&self, world: &WorldAuthority, tick: Tick, _: &TimedEvent) -> Result<WorkPlan, Fault> {
        self.0.set(self.0.get() + 1);
        let mut tx = Transaction::new(
            TransactionId(99),
            key("test:clock"),
            OrderingKey {
                phase: Phase::Simulation,
                rule_priority: 0,
                stable_task_key: 0,
                local_sequence: 0,
            },
        );
        let pos = BlockPos::new(8, 1, 1);
        let state = world.registry().default_state(&key("base:stone")).unwrap();
        tx.read_block(pos, world.store().query(pos).ready().unwrap())
            .write_block(pos, state);
        Ok(WorkPlan {
            transaction: WorkPlan::transaction(tx).transaction,
            schedule: vec![EventDraft {
                tick: Tick(tick.0 + 100),
                rule: key("test:clock"),
                payload: ComponentFields::new(),
            }],
            cancel: Vec::new(),
        })
    }
}
fn rules(counter: Rc<Cell<u32>>) -> Vec<Box<dyn DurableRule>> {
    vec![Box::new(ClockRule(counter))]
}

#[test]
fn requests_are_durable_deduplicated_and_recover_with_world_changes() {
    let dir = Directory::new();
    let data: ComponentFields = [
        (key("test:durability"), ComponentValue::U32(23)),
        (
            key("extension:unknown"),
            ComponentValue::Record(
                [(
                    key("extension:label"),
                    ComponentValue::Text("durable instance".into()),
                )]
                .into(),
            ),
        ),
    ]
    .into();
    let mut server = create_with_stack_data(&dir, Vec::new(), data.clone());
    session(&mut server);
    let queued = request(&server, 1, transfer());
    let id = queued.id;
    assert!(matches!(server.submit(queued), Admission::Queued { .. }));
    assert_eq!(
        server.submit(request(&server, 1, transfer())),
        Admission::Duplicate
    );
    assert!(server.receipt(id).unwrap().is_none());
    tick(&mut server);
    let before = server.authority().unwrap().world().seq();
    assert_eq!(
        server.receipt(id).unwrap().unwrap().reply.status,
        ReplyStatus::Committed
    );
    assert_eq!(
        server.submit(request(&server, 1, transfer())),
        Admission::Duplicate
    );
    let mut different = transfer();
    different.version = 3;
    assert_eq!(
        server.submit(request(&server, 1, different)),
        Admission::Rejected(RejectCode::DuplicateMismatch)
    );
    assert_eq!(server.authority().unwrap().world().seq(), before);
    let expected = inventory(server.authority().unwrap().world(), EntityId(2));
    let catalog = torse_inventory::ItemCatalog::new([(key("base:stone"), 64)]).unwrap();
    let stock = torse_inventory::Inventory::from_fields(&expected, &catalog).unwrap();
    let stack = stock.slots()[0].as_ref().unwrap();
    assert_eq!(stack.count, 1);
    assert_eq!(stack.data, data);
    let source = inventory(server.authority().unwrap().world(), EntityId(1));
    assert!(
        torse_inventory::Inventory::from_fields(&source, &catalog)
            .unwrap()
            .slots()[0]
            .is_none()
    );
    drop(server);
    let mut recovered = reopen(&dir, Vec::new()).unwrap();
    assert_eq!(
        inventory(recovered.authority().unwrap().world(), EntityId(2)),
        expected
    );
    assert_eq!(
        recovered.receipt(id).unwrap().unwrap().reply.status,
        ReplyStatus::Committed
    );
    assert_eq!(
        recovered.submit(WorldRequest {
            id,
            actor: ActorContextId(1),
            input: transfer()
        }),
        Admission::Rejected(RejectCode::StaleSession)
    );
    assert_ne!(recovered.binding().epoch, id.epoch);
    assert_eq!(
        inventory(recovered.authority().unwrap().world(), EntityId(2)),
        expected
    );
}

#[test]
fn events_and_partial_phase_tasks_resume_without_rerunning_completed_rules() {
    let dir = Directory::new();
    let calls = Rc::new(Cell::new(0));
    let mut server = create(&dir, rules(calls.clone()));
    tick(&mut server);
    assert_eq!(server.authority().unwrap().runtime().events.len(), 1);
    tick(&mut server);
    assert_eq!(calls.get(), 0);
    // Stop immediately after freezing P3's due-event output, before executing it.
    loop {
        let a = server.authority().unwrap();
        if a.resume_point().tick == Tick(2)
            && a.resume_point().next_phase == 3
            && a.runtime().work.is_some()
        {
            break;
        }
        server.drive_one(&mut NoReplicas).unwrap();
    }
    assert_eq!(calls.get(), 1);
    drop(server);
    let new_calls = Rc::new(Cell::new(0));
    let mut recovered = reopen(&dir, rules(new_calls.clone())).unwrap();
    tick(&mut recovered);
    assert_eq!(new_calls.get(), 0);
    assert!(
        matches!(recovered.authority().unwrap().world().store().query(BlockPos::new(8,1,1)),TerrainQuery::Ready(s) if !s.is_empty())
    );
    let events = &recovered.authority().unwrap().runtime().events;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.tick, Tick(102));
    let seq = recovered.authority().unwrap().world().seq();
    recovered.shutdown(Tick(3), &mut NoReplicas).unwrap();
    let reopened = reopen(&dir, rules(Rc::new(Cell::new(0)))).unwrap();
    assert_eq!(reopened.authority().unwrap().world().seq(), seq);
    assert_eq!(reopened.authority().unwrap().runtime().events.len(), 1);
}

#[test]
fn recovery_rejects_changed_rules_before_modifying_files() {
    let dir = Directory::new();
    let mut server = create(&dir, rules(Rc::new(Cell::new(0))));
    tick(&mut server);
    drop(server);
    let before = std::fs::read(dir.0.join("journal.log")).unwrap();
    assert!(reopen(&dir, Vec::new()).is_err());
    assert_eq!(std::fs::read(dir.0.join("journal.log")).unwrap(), before);
}

#[test]
fn shutdown_settles_accepted_input_and_keeps_future_events() {
    let dir = Directory::new();
    let mut server = create(&dir, rules(Rc::new(Cell::new(0))));
    session(&mut server);
    let q = request(&server, 1, transfer());
    let id = q.id;
    server.submit(q);
    server.shutdown(Tick(0), &mut NoReplicas).unwrap();
    let reopened = reopen(&dir, rules(Rc::new(Cell::new(0)))).unwrap();
    assert_eq!(
        reopened.receipt(id).unwrap().unwrap().reply.status,
        ReplyStatus::Committed
    );
    assert_eq!(
        reopened.authority().unwrap().runtime().events[0].event.tick,
        Tick(2)
    );
}

fn transports() -> (MemoryTransport, MemoryTransport) {
    MemoryTransport::pair(TransportLimits {
        frame: 8_388_608,
        queued_frames: 32,
        queued_bytes: 16_777_216,
        poll_bytes: 262_144,
    })
    .unwrap()
}
fn send(client: &mut impl Transport, message: ClientMessage) {
    let mut bytes = Vec::new();
    wire::encode(&message, &mut bytes, base::budget(), base::codec_limits()).unwrap();
    client.send(bytes).unwrap();
}
fn receive(client: &mut impl Transport) -> Vec<ServerMessage> {
    let mut out = Vec::new();
    while let Some(bytes) = client.receive().unwrap() {
        out.push(wire::decode(&bytes, base::budget(), base::codec_limits()).unwrap());
    }
    out
}
fn hello(client: &mut impl Transport, binding: Binding) {
    send(
        client,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            compatibility: binding.compatibility,
        },
    );
}
fn seed(messages: Vec<ServerMessage>) -> ReplicaMirror {
    messages
        .into_iter()
        .find_map(|m| {
            if let ServerMessage::Seed(s) = m {
                Some(ReplicaMirror::from_seed(s).unwrap())
            } else {
                None
            }
        })
        .expect("seed")
}
fn add(
    hub: &mut NetworkHub<MemoryTransport>,
    server: &base::Files,
    actor: ActorContext,
) -> (u128, MemoryTransport) {
    let (host, client) = transports();
    let id = hub
        .add(
            host,
            PeerPolicy {
                actor,
                scope: base::scope(),
            },
            server.authority().unwrap().resume_point().tick,
        )
        .unwrap();
    (id, client)
}

#[test]
fn heartbeat_keeps_observer_alive_without_requests_and_silent_peer_expires() {
    let dir = Directory::new();
    let mut server = create(&dir, Vec::new());
    let mut limits = base::network_limits();
    limits.idle_ticks = 3;
    let mut hub = NetworkHub::new(server.binding(), limits).unwrap();
    let (live, mut observer) = add(&mut hub, &server, actor());
    let (silent, mut quiet) = add(&mut hub, &server, actor());
    for client in [&mut observer, &mut quiet] {
        hello(client, server.binding());
    }
    hub.poll(&mut server).unwrap();
    receive(&mut observer);
    receive(&mut quiet);
    send(
        &mut observer,
        ClientMessage::Subscribe {
            sections: base::scope().sections.into_iter().collect(),
        },
    );
    hub.poll(&mut server).unwrap();
    let _mirror = seed(receive(&mut observer));
    for _ in 0..16 {
        server.drive_tick(&mut hub, 1024).unwrap();
        let cursor = server.authority().unwrap().cursor();
        let runtime = server.authority().unwrap().runtime().clone();
        send(&mut observer, ClientMessage::Ping);
        hub.poll(&mut server).unwrap();
        assert!(hub.contains(live));
        assert!(receive(&mut observer).is_empty());
        assert_eq!(server.authority().unwrap().cursor(), cursor);
        assert_eq!(server.authority().unwrap().runtime(), &runtime);
    }
    assert!(!hub.contains(silent));
    // Heartbeats neither consume the request sequence nor its per-tick quota.
    send(
        &mut observer,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: transfer(),
        },
    );
    hub.poll(&mut server).unwrap();
    assert!(receive(&mut observer).iter().any(|m| matches!(
        m,
        ServerMessage::Admission {
            seq: RequestSeq(1),
            admission: Admission::Queued { .. }
        }
    )));
    // Stopping heartbeats restores normal resource reclamation.
    for _ in 0..4 {
        server.drive_tick(&mut hub, 1024).unwrap();
        hub.poll(&mut server).unwrap();
    }
    assert!(!hub.contains(live));
}

#[test]
fn heartbeat_before_hello_and_old_or_future_protocols_cannot_hold_a_peer_slot() {
    let dir = Directory::new();
    let mut server = create(&dir, Vec::new());
    let mut limits = base::network_limits();
    limits.peers = 1;
    let mut hub = NetworkHub::new(server.binding(), limits).unwrap();
    for message in [
        ClientMessage::Ping,
        ClientMessage::Hello {
            version: 1,
            compatibility: server.binding().compatibility,
        },
        ClientMessage::Hello {
            version: 2,
            compatibility: server.binding().compatibility,
        },
        ClientMessage::Hello {
            version: PROTOCOL_VERSION + 1,
            compatibility: server.binding().compatibility,
        },
    ] {
        let (id, mut client) = add(&mut hub, &server, actor());
        let before = server.authority().unwrap().cursor();
        send(&mut client, message);
        hub.poll(&mut server).unwrap();
        assert!(!hub.contains(id));
        assert_eq!(server.authority().unwrap().cursor(), before);
    }
    let (id, mut client) = add(&mut hub, &server, actor());
    hello(&mut client, server.binding());
    hub.poll(&mut server).unwrap();
    assert!(hub.contains(id));
    assert!(matches!(
        receive(&mut client)[0],
        ServerMessage::Welcome { .. }
    ));
}

#[test]
fn heartbeat_tag_appends_to_wire_v1_and_idle_timing_has_three_interval_margin() {
    assert_eq!(PROTOCOL_VERSION, 3);
    let mut bytes = Vec::new();
    wire::encode(
        &ClientMessage::Ping,
        &mut bytes,
        base::budget(),
        base::codec_limits(),
    )
    .unwrap();
    assert_eq!(bytes, [1, 0, 5]);
    assert!(matches!(
        wire::decode::<ClientMessage>(&bytes, base::budget(), base::codec_limits()).unwrap(),
        ClientMessage::Ping
    ));
    for bad in [vec![1, 0, 6], vec![1, 0, 5, 0], vec![1, 0]] {
        assert!(wire::decode::<ClientMessage>(&bad, base::budget(), base::codec_limits()).is_err());
    }
    let mut limits = base::network_limits();
    limits.validate_timing(base::TICK_PERIOD).unwrap();
    limits.idle_ticks = 600;
    limits.validate_timing(base::TICK_PERIOD).unwrap();
    limits.idle_ticks = 599;
    assert!(limits.validate_timing(base::TICK_PERIOD).is_err());
    assert!(limits.validate_timing(std::time::Duration::ZERO).is_err());
    limits.idle_ticks = u64::MAX;
    assert!(limits.validate_timing(std::time::Duration::MAX).is_err());
}

#[test]
fn two_clients_share_durable_authority_without_leaking_inventory() {
    let dir = Directory::new();
    let mut server = create(&dir, Vec::new());
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (_, mut owner) = add(&mut hub, &server, actor());
    let (_, mut observer) = add(&mut hub, &server, ActorContext::new(key("test:observer")));
    hello(&mut owner, server.binding());
    hello(&mut observer, server.binding());
    hub.poll(&mut server).unwrap();
    assert!(matches!(
        receive(&mut owner)[0],
        ServerMessage::Welcome { .. }
    ));
    receive(&mut observer);
    for client in [&mut owner, &mut observer] {
        send(
            client,
            ClientMessage::Subscribe {
                sections: base::scope().sections.into_iter().collect(),
            },
        );
    }
    hub.poll(&mut server).unwrap();
    let mut mirror = seed(receive(&mut owner));
    let observer_mirror = seed(receive(&mut observer));
    assert!(
        observer_mirror
            .state
            .entities
            .iter()
            .all(|e| e.components.is_empty())
    );
    send(
        &mut owner,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: transfer(),
        },
    );
    send(
        &mut owner,
        ClientMessage::Request {
            seq: RequestSeq(2),
            call: Call {
                action: key("inventory:action/inspect"),
                version: 2,
                target: ActionTarget::Entity(EntityId(1)),
                parameters: ComponentFields::new(),
            },
        },
    );
    hub.poll(&mut server).unwrap();
    assert!(
        receive(&mut owner)
            .iter()
            .all(|m| matches!(m, ServerMessage::Admission { .. }))
    );
    let mut messages = Vec::new();
    for _ in 0..64 {
        server.drive_one(&mut hub).unwrap();
        hub.poll(&mut server).unwrap();
        messages.extend(receive(&mut owner));
        let host = server.authority().unwrap();
        if host.at_tick_boundary() && host.resume_point().tick == Tick(1) {
            break;
        }
    }
    assert_eq!(server.authority().unwrap().resume_point().tick, Tick(1));
    let mut results = 0;
    for message in messages {
        match message {
            ServerMessage::Batch(batch) => {
                assert!(batch.through.seq.0 <= server.durability().through.0);
                mirror.apply(batch).unwrap();
            }
            ServerMessage::Result { seq, reply } => {
                assert_eq!(
                    reply.status,
                    if seq == RequestSeq(1) {
                        ReplyStatus::Committed
                    } else {
                        ReplyStatus::ReadOnly
                    }
                );
                results += 1;
            }
            ServerMessage::Admission {
                admission: Admission::Rejected(RejectCode::SequenceExpired),
                ..
            } => panic!("unfinished task was prematurely expired"),
            _ => {}
        }
    }
    assert_eq!(results, 2);
    assert_eq!(
        mirror
            .state
            .entities
            .iter()
            .find(|e| e.id == EntityId(2))
            .unwrap()
            .components[&torse_inventory::schema()],
        inventory(server.authority().unwrap().world(), EntityId(2))
    );
    assert!(receive(&mut observer).is_empty());
    let seq = server.authority().unwrap().world().seq();
    send(
        &mut owner,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: transfer(),
        },
    );
    hub.poll(&mut server).unwrap();
    let duplicate = receive(&mut owner);
    assert!(duplicate.iter().any(|m| matches!(
        m,
        ServerMessage::Admission {
            admission: Admission::Duplicate,
            ..
        }
    )));
    assert!(
        duplicate
            .iter()
            .any(|m| matches!(m, ServerMessage::Result { .. }))
    );
    assert_eq!(seq, server.authority().unwrap().world().seq());
}

#[test]
fn revoke_purges_already_queued_transport_frames_and_reconnect_gets_new_seed() {
    let dir = Directory::new();
    let mut server = create(&dir, Vec::new());
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (id, mut owner) = add(&mut hub, &server, actor());
    hello(&mut owner, server.binding());
    hub.poll(&mut server).unwrap();
    receive(&mut owner);
    send(
        &mut owner,
        ClientMessage::Subscribe {
            sections: base::scope().sections.into_iter().collect(),
        },
    );
    hub.poll(&mut server).unwrap();
    let old = seed(receive(&mut owner));
    send(
        &mut owner,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: transfer(),
        },
    );
    hub.poll(&mut server).unwrap();
    receive(&mut owner);
    server.drive_tick(&mut hub, 1024).unwrap();
    hub.poll(&mut server).unwrap();
    // Success/result and private delta are queued but the client has not read them.
    hub.revoke(&mut server, id).unwrap();
    assert!(owner.receive().is_err());
    let (_, mut replacement) = add(&mut hub, &server, ActorContext::new(key("test:observer")));
    hello(&mut replacement, server.binding());
    hub.poll(&mut server).unwrap();
    receive(&mut replacement);
    send(
        &mut replacement,
        ClientMessage::Subscribe {
            sections: base::scope().sections.into_iter().collect(),
        },
    );
    hub.poll(&mut server).unwrap();
    let fresh = seed(receive(&mut replacement));
    assert_ne!(old.subscription, fresh.subscription);
    assert!(fresh.state.entities.iter().all(|e| e.components.is_empty()));
    assert_eq!(fresh.through.seq.0, server.durability().through.0);
}

#[test]
fn protocol_mismatch_actor_forgery_and_unapproved_ranges_are_rejected() {
    let dir = Directory::new();
    let mut server = create(&dir, Vec::new());
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (bad, mut client) = add(&mut hub, &server, actor());
    send(
        &mut client,
        ClientMessage::Hello {
            version: 999,
            compatibility: server.binding().compatibility,
        },
    );
    hub.poll(&mut server).unwrap();
    assert!(!hub.contains(bad));
    let (_, mut client) = add(&mut hub, &server, ActorContext::new(key("test:guest")));
    hello(&mut client, server.binding());
    hub.poll(&mut server).unwrap();
    receive(&mut client);
    send(
        &mut client,
        ClientMessage::Subscribe {
            sections: vec![SectionPos::new(99, 0, 0)],
        },
    );
    hub.poll(&mut server).unwrap();
    assert!(matches!(
        receive(&mut client)[0],
        ServerMessage::Rejected(RejectCode::PermissionDenied)
    ));
    send(
        &mut client,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: transfer(),
        },
    );
    hub.poll(&mut server).unwrap();
    receive(&mut client);
    server.drive_tick(&mut hub, 1024).unwrap();
    hub.poll(&mut server).unwrap();
    assert!(receive(&mut client).iter().any(|m| matches!(
        m,
        ServerMessage::Result {
            reply: Reply {
                status: ReplyStatus::Rejected(RejectCode::PermissionDenied),
                ..
            },
            ..
        }
    )));
}

#[test]
fn wire_roundtrips_runtime_and_replica_and_rejects_truncation_and_budget_overflow() {
    let (world, actions) = base::world().unwrap();
    let binding = Binding {
        world: base::WORLD,
        epoch: WorldEpoch(1),
        compatibility: world.coordinator().compatibility(),
    };
    let mut host = ExecutionHost::new(
        binding,
        world,
        actions,
        Vec::new(),
        base::execution_limits(),
    )
    .unwrap();
    let snapshot = host.capture_snapshot(base::budget()).unwrap();
    let codec = base::Codec::new(base::codec_limits()).unwrap();
    let mut bytes = Vec::new();
    codec
        .encode_snapshot(&snapshot, &mut bytes, base::budget())
        .unwrap();
    let decoded = codec.decode_snapshot(&bytes, base::budget()).unwrap();
    assert_eq!(decoded.state, snapshot.state);
    for end in 0..bytes.len() {
        assert!(
            codec
                .decode_snapshot(&bytes[..end], base::budget())
                .is_err(),
            "prefix {end}"
        );
    }
    let mut appended = vec![9, 8, 7];
    assert!(
        codec
            .encode_snapshot(
                &snapshot,
                &mut appended,
                OutputBudget {
                    max_items: 1,
                    max_bytes: 1
                }
            )
            .is_err()
    );
    assert_eq!(appended, vec![9, 8, 7]);
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(codec.decode_snapshot(&trailing, base::budget()).is_err());
    assert!(
        wire::decode::<Vec<Call>>(
            &[1, 0, 255, 255, 255, 255],
            base::budget(),
            base::codec_limits()
        )
        .is_err()
    );
    assert!(
        wire::decode::<ReplyStatus>(&[1, 0, 255], base::budget(), base::codec_limits()).is_err()
    );
}

struct CrashHost(ExecutionHost, bool);
impl HostAuthority for CrashHost {
    type Model = ExecutionModel;
    type Prepared = ExecutionPrepared;
    fn binding(&self) -> Binding {
        self.0.binding()
    }
    fn cursor(&self) -> CommitCursor {
        self.0.cursor()
    }
    fn resume_point(&self) -> ResumePoint {
        self.0.resume_point()
    }
    fn is_tick_start(&self, p: ResumePoint) -> bool {
        self.0.is_tick_start(p)
    }
    fn at_tick_boundary(&self) -> bool {
        self.0.at_tick_boundary()
    }
    fn submit(&mut self, r: WorldRequest<Call>) -> Admission {
        self.0.submit(r)
    }
    fn prepare_next(&mut self) -> Result<Option<ExecutionPrepared>, Fault> {
        self.0.prepare_next()
    }
    fn publish(&mut self, p: ExecutionPrepared) -> Result<Publication, Fault> {
        let changed = !p.record().changes.world.component_edits.is_empty();
        let result = self.0.publish(p)?;
        if self.1 && changed {
            std::process::exit(73);
        }
        Ok(result)
    }
    fn capture_snapshot(
        &mut self,
        b: OutputBudget,
    ) -> Result<WorldSnapshot<ExecutionSnapshot>, Fault> {
        self.0.capture_snapshot(b)
    }
    fn stop_admission(&mut self, t: Tick) -> Result<(), Fault> {
        self.0.stop_admission(t)
    }
    fn shutdown_ready(&self) -> bool {
        self.0.shutdown_ready()
    }
}

#[test]
fn execution_crash_child() {
    let Ok(path) = std::env::var("TORSE_EXECUTION_CRASH_PATH") else {
        return;
    };
    let early = std::env::var("TORSE_EXECUTION_CRASH_MODE").unwrap() == "publish";
    let (world, actions) = base::world().unwrap();
    let compatibility = world.coordinator().compatibility();
    let policy = base::file_policy();
    let directory = WorldDirectory::acquire(path, base::WORLD, 8192).unwrap();
    let mut server = FileServer::<CrashHost, base::Codec>::create_file(
        directory,
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        policy,
        |binding| {
            let mut host = ExecutionHost::new(
                binding,
                world,
                actions,
                Vec::new(),
                base::execution_limits(),
            )?;
            host.register_session(viewer(), actor(), base::scope())?;
            host.submit(WorldRequest {
                id: RequestId {
                    world: base::WORLD,
                    epoch: binding.epoch,
                    session: SessionId(1),
                    seq: RequestSeq(1),
                },
                actor: ActorContextId(1),
                input: transfer(),
            });
            Ok(CrashHost(host, early))
        },
    )
    .unwrap();
    for _ in 0..64 {
        server.drive_one(&mut NoReplicas).unwrap();
        if !early
            && server
                .authority()
                .unwrap()
                .0
                .runtime()
                .windows
                .iter()
                .any(|w| !w.receipts.is_empty())
        {
            std::process::exit(74);
        }
    }
    panic!("crash point not reached");
}

#[test]
fn process_abort_before_accept_and_after_durability_never_duplicates_request_effects() {
    for mode in ["publish", "durable"] {
        let dir = Directory::new();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "execution_crash_child", "--nocapture"])
            .env("TORSE_EXECUTION_CRASH_PATH", &dir.0)
            .env("TORSE_EXECUTION_CRASH_MODE", mode)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(if mode == "publish" { 73 } else { 74 }));
        let mut server = reopen(&dir, Vec::new()).unwrap();
        tick(&mut server);
        let old = RequestId {
            world: base::WORLD,
            epoch: WorldEpoch(1),
            session: SessionId(1),
            seq: RequestSeq(1),
        };
        let receipt = server.receipt(old).unwrap().unwrap().clone();
        assert_eq!(receipt.reply.status, ReplyStatus::Committed);
        let catalog = torse_inventory::ItemCatalog::new([(key("base:stone"), 64)]).unwrap();
        let first = torse_inventory::Inventory::from_fields(
            &inventory(server.authority().unwrap().world(), EntityId(1)),
            &catalog,
        )
        .unwrap();
        let second = torse_inventory::Inventory::from_fields(
            &inventory(server.authority().unwrap().world(), EntityId(2)),
            &catalog,
        )
        .unwrap();
        assert!(first.slots()[0].is_none());
        assert_eq!(second.slots()[0].as_ref().unwrap().count, 1);
        drop(server);
        let mut server = reopen(&dir, Vec::new()).unwrap();
        tick(&mut server);
        assert_eq!(server.receipt(old).unwrap().unwrap(), &receipt);
        assert_eq!(
            torse_inventory::Inventory::from_fields(
                &inventory(server.authority().unwrap().world(), EntityId(2)),
                &catalog
            )
            .unwrap(),
            second
        );
    }
}

struct Controlled<J> {
    inner: J,
    busy: Rc<Cell<bool>>,
    fail: Rc<Cell<bool>>,
}
impl<J: torse_storage::CommitJournal<ExecutionModel>> torse_storage::CommitJournal<ExecutionModel>
    for Controlled<J>
{
    type Staged = J::Staged;
    fn try_stage(
        &mut self,
        r: &CommitRecord<ExecutionChanges>,
    ) -> Result<Self::Staged, StageError> {
        if self.busy.get() {
            Err(StageError::Busy)
        } else {
            self.inner.try_stage(r)
        }
    }
    fn accept(&mut self, s: Self::Staged) -> Result<(), Fault> {
        self.inner.accept(s)
    }
    fn flush_through(&mut self, c: CommitCursor) -> Result<Durability, Fault> {
        if self.fail.get() {
            Err(Fault::new(
                FaultCode::StorageUnavailable,
                "injected flush failure",
            ))
        } else {
            self.inner.flush_through(c)
        }
    }
    fn durability(&self) -> Durability {
        self.inner.durability()
    }
}

#[test]
fn blocked_journal_consumes_nothing_and_failed_flush_closes_all_output_gates() {
    let dir = Directory::new();
    let directory = dir.acquire();
    let (world, actions) = base::world().unwrap();
    let binding = Binding {
        world: base::WORLD,
        epoch: directory.epoch(),
        compatibility: world.coordinator().compatibility(),
    };
    let codec = base::Codec::new(base::codec_limits()).unwrap();
    let policy = base::file_policy();
    let busy = Rc::new(Cell::new(false));
    let fail = Rc::new(Cell::new(false));
    let journal = Controlled {
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
    server
        .register_session(viewer(), actor(), base::scope())
        .unwrap();
    let id = RequestId {
        world: base::WORLD,
        epoch: binding.epoch,
        session: SessionId(1),
        seq: RequestSeq(1),
    };
    server.submit(WorldRequest {
        id,
        actor: ActorContextId(1),
        input: transfer(),
    });
    busy.set(true);
    assert!(matches!(
        server.drive_one(&mut NoReplicas).unwrap(),
        CommitStep::Blocked(StageError::Busy)
    ));
    assert_eq!(server.authority().unwrap().cursor().seq, CommitSeq(0));
    assert!(server.receipt(id).unwrap().is_none());
    busy.set(false);
    loop {
        let a = server.authority().unwrap();
        if a.resume_point().next_phase == 2 && a.runtime().work.is_some() {
            break;
        }
        server.drive_one(&mut NoReplicas).unwrap();
    }
    let seed = server
        .open_replica(SessionId(1), base::scope(), base::budget())
        .unwrap();
    fail.set(true);
    assert!(server.drive_one(&mut NoReplicas).is_err());
    assert_eq!(server.state(), ServerState::Faulted);
    assert!(server.authority().is_err());
    assert!(server.receipt(id).is_err());
    assert!(
        server
            .take_replica(seed.subscription, base::budget())
            .is_err()
    );
}

#[test]
fn replay_rejects_unrecorded_event_effects_and_fake_execution_progress() {
    let (world, actions) = base::world().unwrap();
    let registry = world.registry().clone();
    let schemas = world.schemas().clone();
    let binding = Binding {
        world: base::WORLD,
        epoch: WorldEpoch(1),
        compatibility: world.coordinator().compatibility(),
    };
    let mut host = ExecutionHost::new(
        binding,
        world,
        actions,
        Vec::new(),
        base::execution_limits(),
    )
    .unwrap();
    let snapshot = host.capture_snapshot(base::budget()).unwrap();
    let p = host.prepare_next().unwrap().unwrap();
    let record = CommitRecord {
        meta: p.record().meta,
        changes: p.record().changes.clone(),
    };
    host.publish(p).unwrap();
    let p = host.prepare_next().unwrap().unwrap();
    let mut invalid = CommitRecord {
        meta: p.record().meta,
        changes: p.record().changes.clone(),
    };
    let RuntimeUpdate::Full(state) = &mut invalid.changes.runtime else {
        panic!("phase finish must be full");
    };
    state.next_event += 1;
    let (_, actions) = base::world().unwrap();
    let mut target = ExecutionRecovery::new(
        binding,
        &registry,
        &schemas,
        actions,
        Vec::new(),
        base::execution_limits(),
    );
    target.restore_snapshot(snapshot).unwrap();
    target.replay(record).unwrap();
    assert!(target.replay(invalid).is_err());
}

#[test]
fn two_real_tcp_clients_receive_atomic_public_and_authorized_deltas() {
    use std::net::{TcpListener, TcpStream};
    use torse_net::TcpTransport;
    let dir = Directory::new();
    let mut server = create(&dir, Vec::new());
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let limits = TransportLimits {
        frame: 8_388_608,
        queued_frames: 32,
        queued_bytes: 16_777_216,
        poll_bytes: 262_144,
    };
    let mut clients = Vec::new();
    for context in [actor(), ActorContext::new(key("test:observer"))] {
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        hub.add(
            TcpTransport::new(peer, limits).unwrap(),
            PeerPolicy {
                actor: context,
                scope: base::scope(),
            },
            Tick(0),
        )
        .unwrap();
        clients.push(TcpTransport::new(stream, limits).unwrap());
    }
    fn pump(
        clients: &mut [TcpTransport],
        hub: &mut NetworkHub<TcpTransport>,
        server: &mut base::Files,
    ) {
        for _ in 0..12 {
            for c in clients.iter_mut() {
                c.poll().unwrap();
            }
            hub.poll(server).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        for c in clients {
            c.poll().unwrap();
        }
    }
    for client in &mut clients {
        hello(client, server.binding());
    }
    pump(&mut clients, &mut hub, &mut server);
    for client in &mut clients {
        assert!(matches!(receive(client)[0], ServerMessage::Welcome { .. }));
        send(
            client,
            ClientMessage::Subscribe {
                sections: base::scope().sections.into_iter().collect(),
            },
        );
    }
    pump(&mut clients, &mut hub, &mut server);
    let mut owner = seed(receive(&mut clients[0]));
    let mut observer = seed(receive(&mut clients[1]));
    send(
        &mut clients[0],
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: transfer(),
        },
    );
    send(
        &mut clients[0],
        ClientMessage::Request {
            seq: RequestSeq(2),
            call: Call {
                action: key("world:action/place"),
                version: 1,
                target: ActionTarget::Block(BlockPos::new(8, 1, 1)),
                parameters: [(
                    key("world:field/block"),
                    ComponentValue::Content(key("base:stone")),
                )]
                .into(),
            },
        },
    );
    pump(&mut clients, &mut hub, &mut server);
    receive(&mut clients[0]);
    server.drive_tick(&mut hub, 1024).unwrap();
    pump(&mut clients, &mut hub, &mut server);
    let mut successes = 0;
    for message in receive(&mut clients[0]) {
        match message {
            ServerMessage::Batch(batch) => owner.apply(batch).unwrap(),
            ServerMessage::Result { reply, .. } => {
                assert_eq!(reply.status, ReplyStatus::Committed);
                successes += 1;
            }
            _ => {}
        }
    }
    for message in receive(&mut clients[1]) {
        if let ServerMessage::Batch(batch) = message {
            observer.apply(batch).unwrap();
        }
    }
    assert_eq!(successes, 2);
    assert_eq!(owner.state.sections, observer.state.sections);
    assert!(owner.seq.0 > observer.seq.0);
    assert!(
        observer
            .state
            .entities
            .iter()
            .all(|e| e.components.is_empty())
    );
}

#[test]
fn slow_peer_is_disconnected_without_blocking_world_or_healthy_peer() {
    let dir = Directory::new();
    let mut server = create(&dir, Vec::new());
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (slow_host, mut slow) = MemoryTransport::pair(TransportLimits {
        queued_frames: 1,
        ..TransportLimits {
            frame: 8_388_608,
            queued_frames: 32,
            queued_bytes: 16_777_216,
            poll_bytes: 262_144,
        }
    })
    .unwrap();
    let slow_id = hub
        .add(
            slow_host,
            PeerPolicy {
                actor: actor(),
                scope: base::scope(),
            },
            Tick(0),
        )
        .unwrap();
    let (healthy_id, mut healthy) = add(&mut hub, &server, actor());
    hello(&mut slow, server.binding());
    hello(&mut healthy, server.binding());
    hub.poll(&mut server).unwrap();
    receive(&mut slow);
    receive(&mut healthy);
    for client in [&mut slow, &mut healthy] {
        send(
            client,
            ClientMessage::Subscribe {
                sections: base::scope().sections.into_iter().collect(),
            },
        );
    }
    hub.poll(&mut server).unwrap();
    receive(&mut healthy); // Leave slow's seed queued.
    send(
        &mut healthy,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: transfer(),
        },
    );
    hub.poll(&mut server).unwrap();
    receive(&mut healthy);
    server.drive_tick(&mut hub, 1024).unwrap();
    hub.poll(&mut server).unwrap();
    assert!(!hub.contains(slow_id));
    assert!(hub.contains(healthy_id));
    assert!(slow.receive().is_err());
    assert!(receive(&mut healthy).iter().any(|m| matches!(
        m,
        ServerMessage::Result {
            reply: Reply {
                status: ReplyStatus::Committed,
                ..
            },
            ..
        }
    )));
}

#[test]
fn receipt_expiry_and_input_limits_never_readmit_an_old_operation() {
    let dir = Directory::new();
    let (world, actions) = base::world().unwrap();
    let compatibility = world.coordinator().compatibility();
    let mut limits = base::execution_limits();
    limits.receipt_window = 2;
    limits.pending_requests = 1;
    let mut server = base::Files::create_file(
        dir.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| ExecutionHost::new(binding, world, actions, Vec::new(), limits),
    )
    .unwrap();
    session(&mut server);
    let inspect = Call {
        action: key("inventory:action/inspect"),
        version: 2,
        target: ActionTarget::Entity(EntityId(1)),
        parameters: ComponentFields::new(),
    };
    let first = request(&server, 1, inspect.clone());
    let old_id = first.id;
    assert!(matches!(server.submit(first), Admission::Queued { .. }));
    assert_eq!(
        server.submit(request(&server, 2, inspect.clone())),
        Admission::Rejected(RejectCode::QuotaExceeded)
    );
    tick(&mut server);
    for seq in 2..=4 {
        assert!(matches!(
            server.submit(request(&server, seq, inspect.clone())),
            Admission::Queued { .. }
        ));
        tick(&mut server);
    }
    assert!(server.receipt(old_id).unwrap().is_none());
    assert_eq!(
        server.submit(request(&server, 1, inspect.clone())),
        Admission::Rejected(RejectCode::SequenceExpired)
    );
    let mut huge = inspect.clone();
    huge.parameters.insert(
        key("test:huge"),
        ComponentValue::List(vec![ComponentValue::U32(1); 2048]),
    );
    assert_eq!(
        server.submit(request(&server, 5, huge)),
        Admission::Rejected(RejectCode::QuotaExceeded)
    );
    assert!(matches!(
        server.submit(request(&server, 5, inspect)),
        Admission::Queued { .. }
    ));
    tick(&mut server);
    assert_eq!(server.state(), ServerState::Running);
}

#[test]
fn forged_actor_bytes_cannot_be_appended_to_a_client_request() {
    let dir = Directory::new();
    let mut server = create(&dir, Vec::new());
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (id, mut client) = add(&mut hub, &server, actor());
    hello(&mut client, server.binding());
    hub.poll(&mut server).unwrap();
    receive(&mut client);
    let message = ClientMessage::Request {
        seq: RequestSeq(1),
        call: transfer(),
    };
    let mut bytes = Vec::new();
    wire::encode(&message, &mut bytes, base::budget(), base::codec_limits()).unwrap();
    bytes.extend_from_slice(&u128::MAX.to_le_bytes());
    client.send(bytes).unwrap();
    hub.poll(&mut server).unwrap();
    assert!(!hub.contains(id));
    assert_eq!(server.authority().unwrap().cursor().seq, CommitSeq(0));
    assert!(client.receive().is_err());
}

#[test]
fn shutdown_rejects_future_admitted_input_at_a_durable_boundary() {
    for phase in 0..6 {
        let dir = Directory::new();
        let mut server = create(&dir, Vec::new());
        session(&mut server);
        loop {
            let host = server.authority().unwrap();
            if host.resume_point().next_phase == phase && host.runtime().work.is_some() {
                break;
            }
            server.drive_one(&mut NoReplicas).unwrap();
        }
        let q = request(&server, 1, transfer());
        let id = q.id;
        assert_eq!(
            server.submit(q),
            Admission::Queued {
                target_tick: Tick(1)
            }
        );
        server.shutdown(Tick(0), &mut NoReplicas).unwrap();
        let server = reopen(&dir, Vec::new()).unwrap();
        assert_eq!(
            server.receipt(id).unwrap().unwrap().reply.status,
            ReplyStatus::Rejected(RejectCode::Stopping)
        );
        assert_eq!(server.authority().unwrap().resume_point().tick, Tick(1));
    }
}
