//! Shared public 256-section overlap fixture for tests and measurements.
use crate::support::Directory;
use torse_codec::wire;
use torse_net::{MemoryTransport, ReplicaMirror, Transport, TransportLimits};
use torse_server::{bootstrap as base, execution::ExecutionHost, network::*};
use torse_sim::{
    action::{ActionTarget, ActorContext},
    replica::*,
};
use torse_types::*;
use torse_world::*;

pub fn scope(start: i64) -> ReplicaScope {
    ReplicaScope {
        sections: (start..start + 256)
            .map(|x| SectionPos::new(x, 0, 0))
            .collect(),
    }
}
pub fn actor() -> ActorContext {
    ActorContext::new(base::key("test:player"))
        .permission(base::key("world:permission/edit"))
        .target(ActionTarget::Entity(EntityId(1)))
        .target(ActionTarget::Entity(EntityId(2)))
        .target(ActionTarget::Block(BlockPos::new(640, 1, 1)))
}
pub fn create(dir: &Directory) -> base::Files {
    let (mut world, actions) = base::world().unwrap();
    let stone = world
        .registry()
        .default_state(&base::key("base:stone"))
        .unwrap();
    for x in 2..288 {
        let pos = SectionPos::new(x, 0, 0);
        let mut section = Section::empty(16).unwrap();
        if (32..48).contains(&x) {
            section.set(LocalPos { x: 0, y: 0, z: 0 }, stone).unwrap();
        }
        world.store_mut().request(pos).unwrap();
        world.store_mut().install(pos, section).unwrap();
        world.store_mut().finish_load(pos).unwrap();
    }
    let compatibility = world.coordinator().compatibility();
    base::Files::create_file(
        dir.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            let mut limits = base::execution_limits();
            limits.replicas.max_sections = 288;
            ExecutionHost::new(binding, world, actions, Vec::new(), limits)
        },
    )
    .unwrap()
}
pub fn connect(
    server: &mut base::Files,
    scope: ReplicaScope,
) -> (
    NetworkHub<MemoryTransport>,
    u128,
    MemoryTransport,
    ReplicaMirror,
    usize,
) {
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (id, client, mirror, bytes) = attach(&mut hub, server, scope);
    (hub, id, client, mirror, bytes)
}

pub fn attach(
    hub: &mut NetworkHub<MemoryTransport>,
    server: &mut base::Files,
    scope: ReplicaScope,
) -> (u128, MemoryTransport, ReplicaMirror, usize) {
    let (host, mut client) = MemoryTransport::pair(TransportLimits {
        frame: 8_388_608,
        queued_frames: 32,
        queued_bytes: 16_777_216,
        poll_bytes: 262_144,
    })
    .unwrap();
    let id = hub
        .add(
            host,
            PeerPolicy {
                actor: actor(),
                scope: scope.clone(),
            },
            Tick(0),
        )
        .unwrap();
    send(
        &mut client,
        ClientMessage::Hello {
            version: torse_protocol::PROTOCOL_VERSION,
            compatibility: server.binding().compatibility,
        },
    );
    hub.poll(server).unwrap();
    drain(&mut client);
    send(
        &mut client,
        ClientMessage::Subscribe {
            sections: scope.sections.into_iter().collect(),
        },
    );
    hub.poll(server).unwrap();
    let (messages, bytes) = drain(&mut client);
    let mirror = messages
        .into_iter()
        .find_map(|message| {
            if let ServerMessage::Seed(seed) = message {
                Some(ReplicaMirror::from_seed(seed).unwrap())
            } else {
                None
            }
        })
        .expect("initial seed");
    (id, client, mirror, bytes)
}
pub fn send(client: &mut MemoryTransport, message: ClientMessage) {
    let mut bytes = Vec::new();
    wire::encode(&message, &mut bytes, base::budget(), base::codec_limits()).unwrap();
    client.send(bytes).unwrap();
}
pub fn drain(client: &mut MemoryTransport) -> (Vec<ServerMessage>, usize) {
    let mut messages = Vec::new();
    let mut total = 0;
    while let Some(bytes) = client.receive().unwrap() {
        total += bytes.len();
        messages.push(wire::decode(&bytes, base::budget(), base::codec_limits()).unwrap());
    }
    (messages, total)
}
pub fn reference(
    server: &mut base::Files,
    actor: ActorContext,
    scope: ReplicaScope,
) -> ReplicaSnapshot {
    let viewer = Viewer {
        session: SessionId(90),
        actor: ActorContextId(90),
    };
    if server
        .replace_session_scope(viewer.session, actor.clone(), scope.clone())
        .is_err()
    {
        server
            .register_session(viewer, actor, scope.clone())
            .unwrap();
    }
    let seed = server
        .open_replica(viewer.session, scope, base::budget())
        .unwrap();
    server.close_replica(seed.subscription).unwrap();
    seed.state
}
