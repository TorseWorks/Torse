//! Control samples stay off the world-action path.
mod support;
use std::sync::{Arc, Mutex};
use support::Directory;
use torse_codec::wire;
use torse_net::{MemoryTransport, Transport, TransportLimits};
use torse_protocol::{CONTROL_API_VERSION, PROTOCOL_VERSION};
use torse_server::{bootstrap as base, execution::ExecutionHost, network::*, *};
use torse_sim::action::ActorContext;
use torse_types::*;

struct Sample {
    peer: u128,
    viewer: Viewer,
    seq: u64,
    payload: Vec<u8>,
}

struct Recorder {
    events: Mutex<Vec<Sample>>,
    established: Mutex<Vec<(u128, Viewer)>>,
    disconnected: Mutex<Vec<(u128, Viewer)>>,
}

impl Recorder {
    fn on_established(&self, peer: u128, viewer: Viewer) {
        self.established.lock().expect("lock").push((peer, viewer));
    }
    fn on_disconnected(&self, peer: u128, viewer: Viewer) {
        self.disconnected.lock().expect("lock").push((peer, viewer));
    }
    fn on_control(
        &self,
        peer: u128,
        viewer: Viewer,
        seq: u64,
        payload: &[u8],
    ) -> Result<(), Fault> {
        self.events.lock().expect("lock").push(Sample {
            peer,
            viewer,
            seq,
            payload: payload.to_vec(),
        });
        Ok(())
    }
}

fn create(dir: &Directory) -> base::Files {
    let (world, actions) = base::world().unwrap();
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

fn transports() -> (MemoryTransport, MemoryTransport) {
    MemoryTransport::pair(TransportLimits {
        frame: 8_388_608,
        queued_frames: 32,
        queued_bytes: 16_777_216,
        poll_bytes: 262_144,
    })
    .unwrap()
}

fn transports_with_queue(queued_frames: usize) -> (MemoryTransport, MemoryTransport) {
    MemoryTransport::pair(TransportLimits {
        frame: 8_388_608,
        queued_frames,
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

fn hello(client: &mut impl Transport, server: &base::Files) {
    send(
        client,
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            compatibility: server.binding().compatibility,
        },
    );
}

fn budget(policy: ControlPolicy) -> ControlBudget {
    ControlBudget {
        messages_per_tick: 8,
        queue_messages: 1,
        queue_bytes: 16,
        max_message_bytes: 8,
        policy,
    }
}

fn attach(
    server: &base::Files,
    policy: ControlPolicy,
) -> (NetworkHub<MemoryTransport>, Arc<Recorder>) {
    attach_with_budget(server, budget(policy))
}

fn attach_with_budget(
    server: &base::Files,
    control_budget: ControlBudget,
) -> (NetworkHub<MemoryTransport>, Arc<Recorder>) {
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let recorder = Arc::new(Recorder {
        events: Mutex::new(Vec::new()),
        established: Mutex::new(Vec::new()),
        disconnected: Mutex::new(Vec::new()),
    });
    let hook = recorder.clone();
    hub.set_control(control_budget, Box::new(Hook(hook)))
        .unwrap();
    (hub, recorder)
}

struct Hook(Arc<Recorder>);

impl ControlDispatch for Hook {
    fn on_established(&mut self, peer: u128, viewer: Viewer) {
        self.0.on_established(peer, viewer);
    }
    fn on_disconnected(&mut self, peer: u128, viewer: Viewer) {
        self.0.on_disconnected(peer, viewer);
    }
    fn on_control(
        &mut self,
        peer: u128,
        viewer: Viewer,
        seq: u64,
        payload: &[u8],
    ) -> Result<(), Fault> {
        self.0.on_control(peer, viewer, seq, payload)
    }
}

#[test]
fn negotiated_control_uses_the_host_viewer_and_leaves_the_world_unchanged() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let (mut hub, recorder) = attach(&server, ControlPolicy::Reject);
    let (host, mut client) = transports();
    let peer = hub
        .add(
            host,
            PeerPolicy {
                actor: ActorContext::new(base::key("test:player")),
                scope: base::scope(),
            },
            server.authority().unwrap().resume_point().tick,
        )
        .unwrap();
    hello(&mut client, &server);
    send(
        &mut client,
        ClientMessage::ControlOffer {
            api: CONTROL_API_VERSION,
        },
    );
    send(
        &mut client,
        ClientMessage::Control {
            seq: 1,
            payload: vec![9, 9, 9, 9],
        },
    );
    let before = server.authority().unwrap().resume_point();
    hub.poll(&mut server).unwrap();
    let after = server.authority().unwrap().resume_point();
    assert_eq!(before, after);
    let messages = receive(&mut client);
    let session = messages.iter().find_map(|message| match message {
        ServerMessage::Welcome { session, .. } => Some(*session),
        _ => None,
    });
    assert!(messages.iter().any(|message| matches!(
        message,
        ServerMessage::ControlReady { api } if *api == CONTROL_API_VERSION
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        ServerMessage::ControlAck {
            seq: 1,
            accepted: true
        }
    )));
    let established = recorder.established.lock().expect("lock");
    assert_eq!(established.len(), 1);
    assert_eq!(established[0].0, peer);
    assert_eq!(established[0].1.session, session.expect("welcome"));
    let events = recorder.events.lock().expect("lock");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].peer, peer);
    assert_eq!(events[0].seq, 1);
    assert_eq!(events[0].viewer, established[0].1);
    assert_eq!(events[0].payload, vec![9, 9, 9, 9]);
}

#[test]
fn full_queue_coalesces_to_the_newest_sample_or_rejects_it() {
    for policy in [ControlPolicy::Coalesce, ControlPolicy::Reject] {
        let dir = Directory::new();
        let mut server = create(&dir);
        let (mut hub, recorder) = attach(&server, policy);
        let (host, mut client) = transports();
        hub.add(
            host,
            PeerPolicy {
                actor: ActorContext::new(base::key("test:player")),
                scope: base::scope(),
            },
            server.authority().unwrap().resume_point().tick,
        )
        .unwrap();
        hello(&mut client, &server);
        send(
            &mut client,
            ClientMessage::ControlOffer {
                api: CONTROL_API_VERSION,
            },
        );
        send(
            &mut client,
            ClientMessage::Control {
                seq: 1,
                payload: vec![1],
            },
        );
        send(
            &mut client,
            ClientMessage::Control {
                seq: 2,
                payload: vec![2],
            },
        );
        hub.poll(&mut server).unwrap();
        let messages = receive(&mut client);
        let accepted: Vec<_> = messages
            .iter()
            .filter_map(|message| match message {
                ServerMessage::ControlAck { seq, accepted } => Some((*seq, *accepted)),
                _ => None,
            })
            .collect();
        let payloads: Vec<_> = recorder
            .events
            .lock()
            .expect("lock")
            .iter()
            .map(|event| event.payload.clone())
            .collect();
        if policy == ControlPolicy::Coalesce {
            assert_eq!(accepted, vec![(1, false), (2, true)]);
            assert_eq!(payloads, vec![vec![2]]);
        } else {
            assert_eq!(accepted, vec![(2, false), (1, true)]);
            assert_eq!(payloads, vec![vec![1]]);
        }
    }
}

#[test]
fn coalescing_a_multi_entry_queue_discards_every_stale_sample() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let (mut hub, recorder) = attach_with_budget(
        &server,
        ControlBudget {
            messages_per_tick: 4,
            queue_messages: 3,
            queue_bytes: 16,
            max_message_bytes: 8,
            policy: ControlPolicy::Coalesce,
        },
    );
    let (host, mut client) = transports();
    hub.add(
        host,
        PeerPolicy {
            actor: ActorContext::new(base::key("test:player")),
            scope: base::scope(),
        },
        server.authority().unwrap().resume_point().tick,
    )
    .unwrap();
    hello(&mut client, &server);
    send(
        &mut client,
        ClientMessage::ControlOffer {
            api: CONTROL_API_VERSION,
        },
    );
    for seq in 1..=4 {
        send(
            &mut client,
            ClientMessage::Control {
                seq,
                payload: vec![seq as u8],
            },
        );
    }
    hub.poll(&mut server).unwrap();
    let accepted: Vec<_> = receive(&mut client)
        .into_iter()
        .filter_map(|message| match message {
            ServerMessage::ControlAck { seq, accepted } => Some((seq, accepted)),
            _ => None,
        })
        .collect();
    assert_eq!(
        accepted,
        vec![(1, false), (2, false), (3, false), (4, true)]
    );
    let payloads: Vec<_> = recorder
        .events
        .lock()
        .expect("lock")
        .iter()
        .map(|event| event.payload.clone())
        .collect();
    assert_eq!(payloads, vec![vec![4]]);
}

#[test]
fn missing_dispatcher_and_disconnect_do_not_widen_the_connection() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (host, mut client) = transports();
    let peer = hub
        .add(
            host,
            PeerPolicy {
                actor: ActorContext::new(base::key("test:player")),
                scope: base::scope(),
            },
            server.authority().unwrap().resume_point().tick,
        )
        .unwrap();
    hello(&mut client, &server);
    send(
        &mut client,
        ClientMessage::ControlOffer {
            api: CONTROL_API_VERSION,
        },
    );
    hub.poll(&mut server).unwrap();
    assert!(receive(&mut client).iter().any(|message| {
        matches!(
            message,
            ServerMessage::Rejected(RejectCode::PermissionDenied)
        )
    }));
    assert!(hub.contains(peer));

    let dir = Directory::new();
    let mut server = create(&dir);
    let (mut hub, recorder) = attach(&server, ControlPolicy::Reject);
    let (host, mut client) = transports();
    let peer = hub
        .add(
            host,
            PeerPolicy {
                actor: ActorContext::new(base::key("test:player")),
                scope: base::scope(),
            },
            server.authority().unwrap().resume_point().tick,
        )
        .unwrap();
    hello(&mut client, &server);
    send(
        &mut client,
        ClientMessage::ControlOffer {
            api: CONTROL_API_VERSION,
        },
    );
    hub.poll(&mut server).unwrap();
    let _ = receive(&mut client);
    send(&mut client, ClientMessage::Close);
    hub.poll(&mut server).unwrap();
    assert!(!hub.contains(peer));
    assert_eq!(recorder.disconnected.lock().expect("lock").len(), 1);
}

#[test]
fn rejected_control_ready_does_not_disconnect_an_unestablished_extension() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let (mut hub, recorder) = attach(&server, ControlPolicy::Reject);
    let (host, mut client) = transports_with_queue(1);
    let peer = hub
        .add(
            host,
            PeerPolicy {
                actor: ActorContext::new(base::key("test:player")),
                scope: base::scope(),
            },
            server.authority().unwrap().resume_point().tick,
        )
        .unwrap();

    hello(&mut client, &server);
    hub.poll(&mut server).unwrap();
    // Leave Welcome in the one-frame outgoing queue, then make ControlReady
    // fail to enqueue. The enclosing poll retires the peer.
    send(
        &mut client,
        ClientMessage::ControlOffer {
            api: CONTROL_API_VERSION,
        },
    );
    hub.poll(&mut server).unwrap();

    assert!(!hub.contains(peer));
    assert!(recorder.established.lock().expect("lock").is_empty());
    assert!(recorder.disconnected.lock().expect("lock").is_empty());
}
