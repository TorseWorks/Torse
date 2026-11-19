//! User isolation is independent of client messages and survives session replacement.
mod support;
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::Directory;
use torse_codec::wire;
use torse_net::{MemoryTransport, Transport, TransportLimits, tls::TransportSecurity};
use torse_server::{authentication::*, bootstrap as base, execution::*, network::*};
use torse_sim::{
    action::{ActionTarget, ActorContext},
    durable::ReplyStatus,
};
use torse_types::*;

// This trusted test adapter supplies the result of a verified handshake. Real
// certificate, expiry and encrypted-record tests live with the TLS transport.
struct Verified {
    inner: MemoryTransport,
    security: TransportSecurity,
}
impl Transport for Verified {
    fn output_idle(&self) -> bool {
        self.inner.output_idle()
    }
    fn security(&self) -> TransportSecurity {
        self.security
    }
    fn attach_memory(&mut self, pool: MemoryPool) -> Result<(), Fault> {
        self.inner.attach_memory(pool)
    }
    fn send(&mut self, bytes: Vec<u8>) -> Result<(), Fault> {
        self.inner.send(bytes)
    }
    fn receive(&mut self) -> Result<Option<Vec<u8>>, Fault> {
        self.inner.receive()
    }
    fn poll(&mut self) -> Result<(), Fault> {
        self.inner.poll()
    }
    fn close(&mut self) {
        self.inner.close();
    }
}
fn create(dir: &Directory) -> base::Files {
    let (mut world, actions) = base::world().unwrap();
    base::fill_floor(&mut world).unwrap();
    base::spawn_players(&mut world, 2).unwrap();
    base::Files::create_file(
        dir.acquire(),
        world.coordinator().compatibility(),
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
fn grant(name: &str, player: u128, edit: bool) -> UserGrant {
    let user = base::key(name);
    let mut actor = ActorContext::new(user.clone());
    if edit {
        actor = actor.permission(base::key("world:permission/edit"));
    }
    for section in base::scope().sections {
        actor = actor.section(section);
    }
    UserGrant {
        user,
        actor,
        scope: base::scope(),
        player: Some(EntityId(player)),
    }
}
fn hub(server: &base::Files) -> NetworkHub<Verified> {
    let users = CertificateUsers::new([
        (Digest([1; 32]), grant("user:alice", 3, true)),
        (Digest([2; 32]), grant("user:bob", 4, false)),
    ])
    .unwrap();
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    hub.require_authentication(Arc::new(users)).unwrap();
    hub
}
fn add(
    hub: &mut NetworkHub<Verified>,
    security: TransportSecurity,
) -> Result<(u128, MemoryTransport), Fault> {
    let (host, client) = MemoryTransport::pair(TransportLimits {
        frame: 65536,
        queued_bytes: 262144,
        queued_frames: 16,
        poll_bytes: 65536,
    })?;
    let id = hub.add(
        Verified {
            inner: host,
            security,
        },
        PeerPolicy {
            actor: ActorContext::new(base::key("host:unverified")),
            scope: base::scope(),
        },
        Tick(0),
    )?;
    Ok((id, client))
}
fn send(client: &mut MemoryTransport, message: ClientMessage) {
    let mut bytes = Vec::new();
    wire::encode(&message, &mut bytes, base::budget(), base::codec_limits()).unwrap();
    client.send(bytes).unwrap();
}
fn hello(client: &mut MemoryTransport, server: &base::Files) {
    send(
        client,
        ClientMessage::Hello {
            version: torse_protocol::PROTOCOL_VERSION,
            compatibility: server.binding().compatibility,
        },
    );
}
fn receive(client: &mut MemoryTransport) -> Vec<ServerMessage> {
    let mut messages = Vec::new();
    while let Some(bytes) = client.receive().unwrap() {
        messages.push(wire::decode(&bytes, base::budget(), base::codec_limits()).unwrap());
    }
    messages
}
fn session(messages: &[ServerMessage]) -> SessionId {
    messages
        .iter()
        .find_map(|message| match message {
            ServerMessage::Welcome { session, .. } => Some(*session),
            _ => None,
        })
        .unwrap()
}
#[test]
fn certificate_users_get_distinct_players_and_permissions_and_cannot_replay_old_sessions() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let mut hub = hub(&server);
    let (alice, mut a) = add(&mut hub, TransportSecurity::MutualTls(Digest([1; 32]))).unwrap();
    let (bob, mut b) = add(&mut hub, TransportSecurity::MutualTls(Digest([2; 32]))).unwrap();
    hello(&mut a, &server);
    hello(&mut b, &server);
    hub.poll(&mut server).unwrap();
    let first = session(&receive(&mut a));
    let second = session(&receive(&mut b));
    assert_ne!(first, second);
    assert_eq!(hub.user(alice), Some(&base::key("user:alice")));
    assert_eq!(hub.user(bob), Some(&base::key("user:bob")));
    assert_eq!(
        server
            .authority()
            .unwrap()
            .player_state(first)
            .unwrap()
            .entity,
        EntityId(3)
    );
    assert_eq!(
        server
            .authority()
            .unwrap()
            .player_state(second)
            .unwrap()
            .entity,
        EntityId(4)
    );
    for client in [&mut a, &mut b] {
        send(
            client,
            ClientMessage::Request {
                seq: RequestSeq(1),
                call: torse_sim::durable::Call {
                    action: base::key("world:action/break"),
                    version: 1,
                    target: ActionTarget::Block(BlockPos::new(8, 0, 3)),
                    parameters: Default::default(),
                },
            },
        );
    }
    hub.poll(&mut server).unwrap();
    server.drive_tick(&mut hub, 1024).unwrap();
    hub.poll(&mut server).unwrap();
    assert!(receive(&mut a).iter().any(|message| matches!(message, ServerMessage::Result { reply, .. } if reply.status == ReplyStatus::Committed)));
    assert!(receive(&mut b).iter().any(|message| matches!(message, ServerMessage::Result { reply, .. } if reply.status != ReplyStatus::Committed)));
    let (duplicate, mut d) = add(&mut hub, TransportSecurity::MutualTls(Digest([1; 32]))).unwrap();
    hello(&mut d, &server);
    hub.poll(&mut server).unwrap();
    assert!(!hub.contains(duplicate));
    assert!(hub.contains(alice));
    hub.revoke(&mut server, alice).unwrap();
    let (reconnected, mut client) =
        add(&mut hub, TransportSecurity::MutualTls(Digest([1; 32]))).unwrap();
    hello(&mut client, &server);
    hub.poll(&mut server).unwrap();
    let fresh = session(&receive(&mut client));
    assert_ne!(fresh, first);
    send(
        &mut client,
        ClientMessage::Move(torse_protocol::MoveCommand {
            session: first,
            seq: 1,
            tick_hint: Tick(0),
            wish: [1, 0],
            jump: false,
        }),
    );
    hub.poll(&mut server).unwrap();
    assert!(!hub.contains(reconnected));
    assert!(hub.contains(bob));
}
#[test]
fn unverified_unknown_and_idle_peers_never_acquire_a_session() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let mut hub = hub(&server);
    assert!(hub.set_connection_timeout(Duration::from_secs(29)).is_err());
    assert!(add(&mut hub, TransportSecurity::Unprotected).is_err());
    for security in [
        TransportSecurity::Authenticating,
        TransportSecurity::MutualTls(Digest([9; 32])),
    ] {
        let (id, mut client) = add(&mut hub, security).unwrap();
        hello(&mut client, &server);
        hub.poll(&mut server).unwrap();
        assert!(!hub.contains(id));
    }
    let (id, _client) = add(&mut hub, TransportSecurity::Authenticating).unwrap();
    let before = server.durability();
    hub.poll_at(&mut server, Instant::now() + Duration::from_secs(16))
        .unwrap();
    assert!(!hub.contains(id));
    assert_eq!(server.durability(), before);
    let (id, mut client) = add(&mut hub, TransportSecurity::MutualTls(Digest([1; 32]))).unwrap();
    hello(&mut client, &server);
    hub.poll(&mut server).unwrap();
    receive(&mut client);
    assert!(
        hub.replace_policy(
            &mut server,
            id,
            PeerPolicy {
                actor: ActorContext::new(base::key("user:other")),
                scope: base::scope()
            }
        )
        .is_err()
    );
    assert_eq!(hub.user(id), Some(&base::key("user:alice")));
}
#[test]
fn duplicate_or_inconsistent_user_configuration_is_rejected() {
    let a = grant("user:alice", 3, true);
    let b = grant("user:bob", 3, true);
    assert!(CertificateUsers::new([(Digest([1; 32]), a.clone()), (Digest([2; 32]), b)]).is_err());
    assert!(
        CertificateUsers::new([
            (Digest([1; 32]), a.clone()),
            (Digest([1; 32]), grant("user:bob", 4, false))
        ])
        .is_err()
    );
    let mut inconsistent = a;
    inconsistent.user = base::key("user:other");
    assert!(CertificateUsers::new([(Digest([1; 32]), inconsistent)]).is_err());
    let dir = Directory::new();
    let path = dir.0.join("users.toml");
    std::fs::write(
        &path,
        "[[users]]\nid='user:alice'\ncertificate_sha256='bad'\nsections=[[0,0,0]]\n",
    )
    .unwrap();
    assert!(CertificateUsers::load(&path).is_err());
    std::fs::write(&path, vec![b' '; 65537]).unwrap();
    assert!(CertificateUsers::load(&path).is_err());
    let server = create(&dir);
    let users = CertificateUsers::new([(Digest([1; 32]), grant("user:alice", 3, true))]).unwrap();
    users
        .validate_world(
            server.authority().unwrap().world(),
            4,
            base::execution_limits().actor_sections,
        )
        .unwrap();
    assert!(
        users
            .validate_world(
                server.authority().unwrap().world(),
                1,
                base::execution_limits().actor_sections
            )
            .is_err()
    );
    let missing =
        CertificateUsers::new([(Digest([1; 32]), grant("user:alice", 99, true))]).unwrap();
    assert!(
        missing
            .validate_world(
                server.authority().unwrap().world(),
                4,
                base::execution_limits().actor_sections
            )
            .is_err()
    );
}

#[test]
fn each_user_subscription_is_checked_against_its_own_scope() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let near = SectionPos::ZERO;
    let far = SectionPos::new(1, 0, 0);
    let mut a = grant("user:alice", 3, true);
    a.scope.sections = [near].into();
    let mut b = grant("user:bob", 4, false);
    b.scope.sections = [far].into();
    b.player = None;
    let users = CertificateUsers::new([(Digest([1; 32]), a), (Digest([2; 32]), b)]).unwrap();
    users
        .validate_world(
            server.authority().unwrap().world(),
            4,
            base::execution_limits().actor_sections,
        )
        .unwrap();
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    hub.require_authentication(Arc::new(users)).unwrap();
    let (_, mut alice) = add(&mut hub, TransportSecurity::MutualTls(Digest([1; 32]))).unwrap();
    let (bob, mut client) = add(&mut hub, TransportSecurity::MutualTls(Digest([2; 32]))).unwrap();
    hello(&mut alice, &server);
    hello(&mut client, &server);
    hub.poll(&mut server).unwrap();
    receive(&mut alice);
    receive(&mut client);
    send(
        &mut alice,
        ClientMessage::Subscribe {
            sections: vec![near],
        },
    );
    send(
        &mut client,
        ClientMessage::Subscribe {
            sections: vec![far],
        },
    );
    hub.poll(&mut server).unwrap();
    for (messages, expected) in [(receive(&mut alice), near), (receive(&mut client), far)] {
        let seed = messages
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::Seed(seed) => Some(seed),
                _ => None,
            })
            .unwrap();
        assert_eq!(seed.state.sections.len(), 1);
        assert_eq!(seed.state.sections[0].pos, expected);
    }
    send(
        &mut client,
        ClientMessage::Subscribe {
            sections: vec![near],
        },
    );
    hub.poll(&mut server).unwrap();
    if hub.contains(bob) {
        assert!(
            receive(&mut client)
                .iter()
                .all(|message| !matches!(message, ServerMessage::Seed(_)))
        );
    }
    assert!(
        server
            .authority()
            .unwrap()
            .world()
            .store()
            .section(near)
            .is_some()
    );
}

#[test]
fn explicit_visual_reset_cannot_replace_the_authenticated_user() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let mut hub = hub(&server);
    let (alice, mut client) = add(&mut hub, TransportSecurity::MutualTls(Digest([1; 32]))).unwrap();
    hello(&mut client, &server);
    send(
        &mut client,
        ClientMessage::Subscribe {
            sections: base::scope().sections.iter().copied().collect(),
        },
    );
    hub.poll(&mut server).unwrap();
    receive(&mut client);
    let pages = torse_server::section_pages::SectionPages::new(
        &dir.0,
        base::budget(),
        base::codec_limits(),
        100,
    )
    .unwrap();
    hub.enable_visual_interest(
        &server,
        alice,
        pages,
        base::scope(),
        base::scope(),
        SectionPos::ZERO,
        torse_server::visual::VisualTerrainLimits {
            max_sections: 4,
            cache_bytes: 256 * 1024,
            io_bytes: base::budget().max_bytes * 4,
            batch_bytes: 128 * 1024,
            control_bytes: 64 * 1024,
            pages_per_poll: 1,
        },
    )
    .unwrap();
    assert!(
        hub.reset_policy(
            &mut server,
            alice,
            PeerPolicy {
                actor: grant("user:bob", 4, false).actor,
                scope: base::scope()
            }
        )
        .is_err()
    );
    assert_eq!(hub.user(alice), Some(&base::key("user:alice")));
    assert!(hub.contains(alice));
    assert!(hub.visual_terrain_diagnostics(alice).is_some());
}

#[test]
fn authenticated_large_grants_follow_host_limits_instead_of_a_global_ceiling() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let mut large = grant("user:alice", 3, true);
    for x in -3..=3 {
        for z in -3..=3 {
            for y in -4..20 {
                large.actor = large.actor.section(SectionPos::new(x, y, z));
            }
        }
    }
    assert_eq!(large.actor.section_count(), 1176);
    assert!(
        CertificateUsers::new_with_limits([(Digest([1; 32]), large.clone())], 4, 1175).is_err()
    );
    assert!(
        CertificateUsers::new_with_limits([(Digest([1; 32]), large.clone())], 1, 1176).is_err()
    );
    let users = CertificateUsers::new_with_limits([(Digest([1; 32]), large)], 4, 1176).unwrap();
    users
        .validate_world(server.authority().unwrap().world(), 4, 1176)
        .unwrap();
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    hub.require_authentication(Arc::new(users)).unwrap();
    // Construction permits the configured policy; admission still uses the host's live limit.
    let (_, mut refused) = add(&mut hub, TransportSecurity::MutualTls(Digest([1; 32]))).unwrap();
    hello(&mut refused, &server);
    hub.poll(&mut server).unwrap();
    assert!(
        refused.receive().is_err(),
        "over-limit authentication closes before admission"
    );
    server.set_actor_section_limit(1176).unwrap();
    let (peer, mut client) = add(&mut hub, TransportSecurity::MutualTls(Digest([1; 32]))).unwrap();
    hello(&mut client, &server);
    hub.poll(&mut server).unwrap();
    session(&receive(&mut client));
    assert!(hub.contains(peer));
    assert_eq!(hub.user(peer), Some(&base::key("user:alice")));
}

#[test]
fn explicit_policy_file_limits_accept_full_height_grants_and_preserve_parser_bounds() {
    let dir = Directory::new();
    let path = dir.0.join("large-users.toml");
    let sections: Vec<_> = (-3..=3)
        .flat_map(|x| (-3..=3).flat_map(move |z| (-4..20).map(move |y| format!("[{x},{y},{z}]"))))
        .collect();
    let text = format!(
        "[[users]]\nid='user:alice'\ncertificate_sha256='{}'\nsections=[{}]\n",
        "01".repeat(32),
        sections.join(",")
    );
    assert!(text.len() < 65536);
    std::fs::write(&path, text).unwrap();
    assert!(CertificateUsers::load(&path).is_err());
    let users = CertificateUsers::load_with_limits(&path, 1176, 1176).unwrap();
    let grant = users
        .verify(TransportSecurity::MutualTls(Digest([1; 32])))
        .unwrap();
    assert_eq!(grant.actor.section_count(), 1176);
    assert_eq!(grant.scope.sections.len(), 1176);
    assert!(CertificateUsers::load_with_limits(&path, 1175, 1176).is_err());
    assert!(CertificateUsers::load_with_limits(&path, 1176, 1175).is_err());
    assert!(CertificateUsers::load_with_limits(&path, 0, 1176).is_err());
    std::fs::write(&path, vec![b' '; 65537]).unwrap();
    assert!(CertificateUsers::load_with_limits(&path, 1176, 1176).is_err());
}
