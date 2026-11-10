//! Client side of the versioned protocol: connect, mirror, and request.
//!
//! The connection owns one transport and one [`ReplicaMirror`]. The window
//! polls it every frame; inbound frames decode into [`NetEvent`]s and the
//! caller applies world data through the scene. Frames are bounded by the
//! same limits the headless client uses.

use std::time::Instant;
use torse_codec::CodecLimits;
use torse_codec::wire;
use torse_net::terrain::{TerrainChanges, TerrainLimits, TerrainMirror};
use torse_net::{ReplicaMirror, Transport};
use torse_protocol::PROTOCOL_VERSION;
use torse_sim::replica::{ReplicaChanges, ReplicaSnapshot};
use torse_types::{
    Admission, BlockPos, Compatibility, ContentKey, Fault, OutputBudget, RejectCode, RequestSeq,
    SessionId,
};
use torse_types::{EvictionRank, FaultCode, MemoryClass, MemoryLease, MemoryPool, SubscriptionId};
#[path = "prediction.rs"]
mod prediction;

/// The concrete message shapes this demo exchanges; the type parameters all
/// live in shared crates, so the client binds them itself.
pub type ClientMessage = torse_protocol::ClientMessage<torse_sim::durable::Call>;
pub type ServerMessage =
    torse_protocol::ServerMessage<torse_sim::durable::Reply, ReplicaSnapshot, ReplicaChanges>;

/// One decoded server message the window acts on.
#[derive(Debug)]
pub enum NetEvent {
    Player(torse_protocol::PlayerUpdate),
    /// Handshake finished; the session is live.
    Welcomed {
        session: SessionId,
    },
    /// The authorized baseline arrived; the mirror now holds a full snapshot.
    Baseline(ReplicaSnapshot),
    /// One increment was validated and applied to the mirror.
    Increment(ReplicaChanges),
    Terrain(TerrainChanges),
    /// Admission for a submitted request.
    Admitted {
        seq: RequestSeq,
        rejected: Option<RejectCode>,
    },
    /// A request result arrived.
    Finished {
        seq: RequestSeq,
        committed: bool,
    },
    /// The server closed the connection with a reason.
    Closed(String),
}
#[derive(Debug)]
pub struct IncomingEvent {
    pub event: NetEvent,
    _memory: Option<MemoryLease>,
}

/// Live client connection to a standalone server.
pub struct Connection<T: Transport> {
    prediction: prediction::Prediction,
    transport: T,
    mirror: Option<ReplicaMirror>,
    terrain: Option<TerrainMirror>,
    terrain_limits: TerrainLimits,
    subscription_pending: bool,
    request_seq: u64,
    budget: OutputBudget,
    limits: CodecLimits,
    last_ping: Option<Instant>,
    retired: Option<SubscriptionId>,
    tls_configuration: Option<MemoryLease>,
    memory: Option<MemoryLease>,
}

impl<T: Transport> Connection<T> {
    /// Wrap a connected transport with the demo frame budget.
    pub fn new(transport: T) -> Self {
        Self {
            prediction: Default::default(),
            transport,
            mirror: None,
            terrain: None,
            terrain_limits: TerrainLimits {
                max_sections: 4096,
                max_bytes: 64 * 1024 * 1024,
            },
            subscription_pending: false,
            request_seq: 0,
            budget: OutputBudget {
                max_items: 200_000,
                max_bytes: 8_388_608,
            },
            limits: codec_limits(),
            last_ping: None,
            retired: None,
            tls_configuration: None,
            memory: None,
        }
    }

    fn send(&mut self, message: ClientMessage) -> Result<(), Fault> {
        let _memory = self
            .memory
            .as_ref()
            .map(|lease| {
                lease.pool().reserve(
                    MemoryClass::Codec,
                    self.budget.max_bytes.saturating_mul(2),
                    EvictionRank::Pinned,
                )
            })
            .transpose()?;
        self.send_reserved(message)
    }
    fn send_reserved(&mut self, message: ClientMessage) -> Result<(), Fault> {
        let mut bytes = Vec::new();
        wire::encode(&message, &mut bytes, self.budget, self.limits)?;
        self.transport.send(bytes)
    }
    pub fn attach_memory(&mut self, pool: MemoryPool) -> Result<(), Fault> {
        if self.memory.is_some() || self.mirror.is_some() {
            return Err(Fault::invalid_data(
                "connection memory requires a fresh mirror",
            ));
        }
        let memory = pool.reserve(
            MemoryClass::Replica,
            self.budget.max_bytes.saturating_mul(3),
            EvictionRank::Pinned,
        )?;
        self.transport.attach_memory(pool)?;
        self.memory = Some(memory);
        Ok(())
    }
    pub fn snapshot(&self) -> Option<&ReplicaSnapshot> {
        self.mirror.as_ref().map(ReplicaMirror::state)
    }
    pub fn terrain(&self) -> Option<&TerrainMirror> {
        self.terrain.as_ref()
    }
    /// Configure before the first display update; changing a live cache requires reconnecting.
    pub fn set_terrain_limits(&mut self, limits: TerrainLimits) -> Result<(), Fault> {
        if self.terrain.is_some()
            || limits.max_sections == 0
            || limits.max_sections > 16384
            || limits.max_bytes < 4096
        {
            return Err(Fault::invalid_data("invalid display cache configuration"));
        }
        self.terrain_limits = limits;
        Ok(())
    }

    /// Start the handshake with the shared content compatibility.
    pub fn hello(&mut self, compatibility: Compatibility) -> Result<(), Fault> {
        self.send(ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            compatibility,
        })
    }

    /// Subscribe to a fixed section set the host already approved.
    pub fn subscribe(&mut self, sections: Vec<torse_types::SectionPos>) -> Result<(), Fault> {
        if self.subscription_pending {
            return Err(Fault::resource_limit("subscription replacement pending"));
        }
        if let Some(subscription) = self.mirror.as_ref().map(|mirror| mirror.subscription)
            && self.retired != Some(subscription)
        {
            self.send(ClientMessage::Unsubscribe { subscription })?;
            self.retired = Some(subscription);
        }
        self.send(ClientMessage::Subscribe { sections })?;
        self.subscription_pending = true;
        Ok(())
    }

    /// Submit one action call; sequence numbers start at one and never repeat.
    pub fn request(&mut self, call: torse_sim::durable::Call) -> Result<RequestSeq, Fault> {
        let next = self
            .request_seq
            .checked_add(1)
            .ok_or_else(|| Fault::resource_limit("request sequence exhausted"))?;
        let seq = RequestSeq(next);
        self.send(ClientMessage::Request { seq, call })?;
        self.request_seq = next;
        Ok(seq)
    }

    pub fn player_position(
        &self,
        world: &torse_sim::WorldAuthority,
    ) -> Option<torse_world::Position> {
        self.prediction.position(world)
    }
    pub fn player_entity(&self) -> Option<torse_types::EntityId> {
        self.prediction.entity()
    }
    pub fn refresh_player(&mut self, world: &torse_sim::WorldAuthority) -> Result<(), Fault> {
        self.prediction.refresh(world)
    }
    pub fn move_player(
        &mut self,
        world: &torse_sim::WorldAuthority,
        wish: [i8; 2],
        jump: bool,
    ) -> Result<Option<torse_world::Position>, Fault> {
        if self.retired.is_some() {
            return Ok(None);
        }
        let _memory = if let Some(pool) = self.memory.as_ref().map(MemoryLease::pool) {
            match pool.reserve(MemoryClass::Codec, 262_144, EvictionRank::Pinned) {
                Ok(lease) => Some(lease),
                Err(error) if error.code == FaultCode::ResourceLimit => return Ok(None),
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        if let Some((session, input)) = self.prediction.step(world, wish, jump)? {
            self.send_reserved(ClientMessage::Move(torse_protocol::MoveCommand {
                session,
                seq: input.seq,
                tick_hint: world.coordinator().resume_point().tick,
                wish: [input.wish[0], input.wish[2]],
                jump: input.jump,
            }))?;
        }
        Ok(self.prediction.position(world))
    }

    /// Poll the transport and translate every completed frame.
    pub fn poll(&mut self) -> Result<Vec<IncomingEvent>, Fault> {
        self.poll_at(Instant::now())
    }

    fn poll_at(&mut self, now: Instant) -> Result<Vec<IncomingEvent>, Fault> {
        self.transport.poll()?;
        let mut events = Vec::new();
        loop {
            let memory = if let Some(pool) = self.memory.as_ref().map(MemoryLease::pool) {
                match pool.reserve(
                    MemoryClass::Codec,
                    self.budget.max_bytes.saturating_mul(4),
                    EvictionRank::Pinned,
                ) {
                    Ok(lease) => Some(lease),
                    Err(error) if error.code == FaultCode::ResourceLimit => break,
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            let Some(bytes) = self.transport.receive()? else {
                break;
            };
            let message: ServerMessage = wire::decode(&bytes, self.budget, self.limits)?;
            match self.handle(message, now)? {
                Some(event) => events.push(IncomingEvent {
                    event,
                    _memory: memory,
                }),
                None => continue,
            }
        }
        if self.last_ping.is_some_and(|last| {
            now.saturating_duration_since(last) >= torse_protocol::HEARTBEAT_INTERVAL
        }) {
            match self.send(ClientMessage::Ping) {
                Ok(()) => self.last_ping = Some(now),
                Err(error) if error.code == FaultCode::ResourceLimit => {}
                Err(error) => return Err(error),
            }
            self.transport.poll()?;
        }
        Ok(events)
    }

    fn handle(&mut self, message: ServerMessage, now: Instant) -> Result<Option<NetEvent>, Fault> {
        match message {
            ServerMessage::Welcome { session, .. } => {
                self.prediction.welcome(session);
                self.last_ping = Some(now);
                Ok(Some(NetEvent::Welcomed { session }))
            }
            ServerMessage::Seed(seed) => {
                // Clone the snapshot first; from_seed consumes the envelope.
                let snapshot = seed.state.clone();
                let mirror = ReplicaMirror::from_seed(seed)?;
                if let Some(terrain) = self.terrain.as_mut() {
                    terrain.observe_snapshot(mirror.state(), mirror.through);
                }
                self.mirror = Some(mirror);
                self.subscription_pending = false;
                self.retired = None;
                Ok(Some(NetEvent::Baseline(snapshot)))
            }
            ServerMessage::Batch(batch) => {
                let seq = batch.seq;
                let mut changes = batch.changes.clone();
                let mirror = self
                    .mirror
                    .as_mut()
                    .ok_or_else(|| Fault::invalid_data("batch before baseline"))?;
                changes.presentation = mirror.apply_with_presentation(batch)?;
                if let Some(terrain) = self.terrain.as_mut() {
                    terrain.observe_authority(&changes, mirror.through);
                }
                let _ = seq;
                Ok(Some(NetEvent::Increment(changes)))
            }
            ServerMessage::Interest(batch) => {
                let changes = batch.changes.clone();
                let mirror = self
                    .mirror
                    .as_mut()
                    .ok_or_else(|| Fault::invalid_data("interest before baseline"))?;
                mirror.apply_interest(batch)?;
                if let Some(terrain) = self.terrain.as_mut() {
                    terrain.observe_authority(&changes, mirror.through);
                }
                Ok(Some(NetEvent::Increment(changes)))
            }
            ServerMessage::Terrain(update) => {
                let near = self
                    .mirror
                    .as_ref()
                    .ok_or_else(|| Fault::invalid_data("terrain before baseline"))?;
                if update.binding != near.binding {
                    return Err(Fault::new(
                        FaultCode::ResetRequired,
                        "terrain binding mismatch",
                    ));
                }
                if self.terrain.is_none() {
                    let mut terrain = TerrainMirror::new(
                        near.binding,
                        near.state().side,
                        near.state().bounds,
                        self.terrain_limits,
                        self.memory.as_ref().map(MemoryLease::pool).cloned(),
                    )?;
                    let changes = terrain.apply(update)?;
                    terrain.observe_snapshot(near.state(), near.through);
                    self.terrain = Some(terrain);
                    return Ok(Some(NetEvent::Terrain(changes)));
                }
                let changes = self
                    .terrain
                    .as_mut()
                    .expect("terrain initialized")
                    .apply(update)?;
                Ok(Some(NetEvent::Terrain(changes)))
            }
            ServerMessage::Admission { seq, admission } => {
                let rejected = match admission {
                    Admission::Queued { .. } | Admission::Duplicate => None,
                    Admission::Rejected(code) => Some(code),
                };
                Ok(Some(NetEvent::Admitted { seq, rejected }))
            }
            ServerMessage::Result { seq, reply } => {
                // A rejected reply is a completed request the caller must see
                // as denied, not silently dropped. ReadOnly results are a
                // legitimate query outcome, not a rejection.
                use torse_sim::durable::ReplyStatus;
                let committed = !matches!(reply.status, ReplyStatus::Rejected(_));
                Ok(Some(NetEvent::Finished { seq, committed }))
            }
            ServerMessage::Rejected(reason) => {
                self.last_ping = None;
                Ok(Some(NetEvent::Closed(format!("rejected: {reason:?}"))))
            }
            ServerMessage::Player(update) => {
                self.prediction.receive(update);
                Ok(Some(NetEvent::Player(update)))
            }
            ServerMessage::ControlReady { .. } | ServerMessage::ControlAck { .. } => Ok(None),
        }
    }
}

impl Connection<torse_net::TcpTransport> {
    /// Connect over TCP to a standalone server.
    pub fn connect(
        address: &str,
        tls: Option<&TlsClient>,
        pool: &MemoryPool,
    ) -> Result<Connection<torse_net::TcpTransport>, Fault> {
        use std::net::ToSocketAddrs;
        let endpoint = address
            .to_socket_addrs()
            .map_err(|_| Fault::invalid_data("server address resolution failed"))?
            .next()
            .ok_or_else(|| Fault::invalid_data("empty server address"))?;
        if tls.is_none() && !endpoint.ip().is_loopback() {
            return Err(Fault::incompatible("remote connections require mutual TLS"));
        }
        let stream =
            std::net::TcpStream::connect_timeout(&endpoint, std::time::Duration::from_secs(5))
                .map_err(|error| Fault::invalid_data(error.to_string()))?;
        let limits = torse_net::TransportLimits {
            frame: 8_388_608,
            queued_frames: 32,
            queued_bytes: 16_777_216,
            poll_bytes: 262_144,
        };
        let tls_configuration = tls
            .map(|tls| -> Result<_, Fault> {
                pool.reserve(
                    MemoryClass::Resource,
                    torse_net::tls::configuration_reservation(&[
                        std::path::Path::new(&tls.ca),
                        std::path::Path::new(&tls.certificate),
                        std::path::Path::new(&tls.key),
                    ])?,
                    EvictionRank::Pinned,
                )
            })
            .transpose()?;
        let transport = match tls {
            Some(tls) => torse_net::TcpTransport::tls_client(
                stream,
                limits,
                torse_net::tls::client_config(
                    std::path::Path::new(&tls.ca),
                    std::path::Path::new(&tls.certificate),
                    std::path::Path::new(&tls.key),
                )?,
                &tls.server_name,
            )?,
            None => torse_net::TcpTransport::new(stream, limits)?,
        };
        let mut connection = Connection::new(transport);
        connection.tls_configuration = tls_configuration;
        Ok(connection)
    }
}

#[derive(Clone, Debug)]
pub struct TlsClient {
    pub ca: String,
    pub certificate: String,
    pub key: String,
    pub server_name: String,
}

/// Build the same place or break call the local scene commits, for the
/// network request path.
pub fn edit_call(
    place: bool,
    block: &ContentKey,
    target: BlockPos,
    half: u32,
) -> torse_sim::durable::Call {
    let action = if place {
        "world:action/place"
    } else {
        "world:action/break"
    };
    let mut parameters = torse_world::ComponentFields::new();
    if place {
        parameters.insert(
            ContentKey::parse("world:field/block").expect("static key"),
            torse_world::ComponentValue::Content(block.clone()),
        );
        parameters.insert(
            ContentKey::parse("world:field/half").expect("static key"),
            torse_world::ComponentValue::U32(half),
        );
    }
    torse_sim::durable::Call {
        action: ContentKey::parse(action).expect("static key"),
        version: 1,
        target: torse_sim::action::ActionTarget::Block(target),
        parameters,
    }
}

fn codec_limits() -> CodecLimits {
    CodecLimits {
        max_frame_bytes: 8_388_608,
        max_field_bytes: 262_144,
        max_depth: 12,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torse_net::{MemoryTransport, TransportLimits};

    #[test]
    fn terrain_wire_delivery_keeps_the_near_mirror_and_rejects_replayed_updates() {
        use torse_types::*;
        use torse_world::*;
        let (client, mut server) = MemoryTransport::pair(default_limits()).unwrap();
        let mut connection = Connection::new(client);
        connection
            .set_terrain_limits(TerrainLimits {
                max_sections: 8,
                max_bytes: 1024 * 1024,
            })
            .unwrap();
        let binding = Binding {
            world: WorldId(7),
            epoch: WorldEpoch(1),
            compatibility: crate::scene::Scene::compatibility().unwrap(),
        };
        let at = CommitCursor::initial(binding.world);
        let near = ReplicaSnapshot {
            side: 16,
            bounds: WorldBounds::new(0, 32).unwrap(),
            states: vec![],
            sections: vec![SnapshotSection {
                pos: SectionPos::ZERO,
                cells: SnapshotCells::Uniform(BlockStateId::EMPTY),
            }],
            entities: vec![],
        };
        connection
            .handle(
                ServerMessage::Seed(torse_sim::replica::WorldReplicaSeed {
                    binding,
                    subscription: SubscriptionId(1),
                    baseline: at,
                    state: near.clone(),
                }),
                Instant::now(),
            )
            .unwrap();
        let pos = SectionPos::new(2, 0, 0);
        let mut far = near.clone();
        far.sections[0].pos = pos;
        let update = TerrainUpdate {
            binding,
            generation: 1,
            seq: 1,
            through: at,
            reset: Some(TerrainReset {
                sections: vec![pos],
                retained: vec![],
            }),
            pages: vec![TerrainPage {
                at,
                digest: Digest([1; 32]),
                terrain: far.clone(),
            }],
            unavailable: vec![],
        };
        let mut bytes = Vec::new();
        wire::encode(
            &ServerMessage::Terrain(update.clone()),
            &mut bytes,
            connection.budget,
            connection.limits,
        )
        .unwrap();
        server.send(bytes).unwrap();
        assert!(matches!(
            connection.poll().unwrap()[0].event,
            NetEvent::Terrain(_)
        ));
        assert_eq!(connection.snapshot(), Some(&near));
        assert_eq!(connection.terrain().unwrap().page(pos), Some(&far));
        assert_eq!(
            connection
                .handle(ServerMessage::Terrain(update), Instant::now())
                .unwrap_err()
                .code,
            FaultCode::ResetRequired
        );
        assert_eq!(connection.snapshot(), Some(&near));
        assert_eq!(connection.terrain().unwrap().page(pos), Some(&far));
    }

    #[test]
    fn decode_pressure_retains_frames_and_events_hold_temporary_reservations() {
        let pool = MemoryPool::new(torse_types::MemoryBudget::new(256 * 1024 * 1024));
        let (client, mut server) = MemoryTransport::pair(default_limits()).unwrap();
        let mut connection = Connection::new(client);
        connection.attach_memory(pool.clone()).unwrap();
        let mut bytes = Vec::new();
        wire::encode(
            &ServerMessage::Welcome {
                binding: torse_types::Binding {
                    world: torse_types::WorldId(7),
                    epoch: torse_types::WorldEpoch(1),
                    compatibility: crate::scene::Scene::compatibility().unwrap(),
                },
                session: SessionId(1),
            },
            &mut bytes,
            connection.budget,
            connection.limits,
        )
        .unwrap();
        server.send(bytes).unwrap();
        let resident = pool.usage().total;
        let temporary = pool
            .reserve(
                MemoryClass::Resource,
                256 * 1024 * 1024 - resident,
                EvictionRank::Pinned,
            )
            .unwrap();
        assert!(connection.poll().unwrap().is_empty());
        drop(temporary);
        let events = connection.poll().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].event,
            NetEvent::Welcomed {
                session: SessionId(1)
            }
        ));
        assert!(pool.usage().current[MemoryClass::Codec.index()] >= 32 * 1024 * 1024);
        drop(events);
        assert_eq!(pool.usage().current[MemoryClass::Codec.index()], 0);
        drop(connection);
        assert_eq!(pool.usage().total, 0);
    }

    #[test]
    fn heartbeat_waits_for_welcome_and_sends_once_per_interval_without_catchup() {
        let (client, mut server) = MemoryTransport::pair(default_limits()).unwrap();
        let mut connection = Connection::new(client);
        let start = Instant::now();
        let interval = torse_protocol::HEARTBEAT_INTERVAL;
        connection.poll_at(start + interval * 5).unwrap();
        assert!(server.receive().unwrap().is_none());
        let binding = torse_types::Binding {
            world: torse_types::WorldId(7),
            epoch: torse_types::WorldEpoch(1),
            compatibility: Compatibility {
                authority_schema: 1,
                rules_digest: torse_types::Digest([0; 32]),
                content_digest: torse_types::Digest([0; 32]),
            },
        };
        let mut bytes = Vec::new();
        wire::encode(
            &ServerMessage::Welcome {
                binding,
                session: SessionId(1),
            },
            &mut bytes,
            connection.budget,
            connection.limits,
        )
        .unwrap();
        server.send(bytes).unwrap();
        connection.poll_at(start).unwrap();
        connection
            .poll_at(start + interval - std::time::Duration::from_nanos(1))
            .unwrap();
        assert!(server.receive().unwrap().is_none());
        for now in [start + interval, start + interval * 10] {
            connection.poll_at(now).unwrap();
            let bytes = server.receive().unwrap().unwrap();
            assert!(matches!(
                wire::decode::<ClientMessage>(&bytes, connection.budget, connection.limits)
                    .unwrap(),
                ClientMessage::Ping
            ));
            connection.poll_at(now).unwrap();
            assert!(server.receive().unwrap().is_none());
        }
        assert_eq!(connection.request_seq, 0);
        server.close();
        assert!(connection.poll_at(start + interval * 11).is_err());
    }

    #[test]
    fn request_sequences_start_at_one_and_increase() {
        let (client, _server) = MemoryTransport::pair(default_limits()).expect("pair");
        let mut connection = Connection::new(client);
        let call = edit_call(
            false,
            &ContentKey::parse("base:stone").expect("key"),
            BlockPos::new(1, 1, 1),
            0,
        );
        let first = connection.request(call.clone()).expect("send");
        let second = connection.request(call).expect("send");
        assert_eq!(first.0, 1);
        assert_eq!(second.0, 2);
    }

    #[test]
    fn subscription_replacement_unsubscribes_the_old_generation_before_subscribing() {
        use torse_types::*;
        let (client, mut server) = MemoryTransport::pair(default_limits()).unwrap();
        let mut connection = Connection::new(client);
        let binding = Binding {
            world: WorldId(1),
            epoch: WorldEpoch(1),
            compatibility: Compatibility {
                authority_schema: 1,
                rules_digest: Digest([0; 32]),
                content_digest: Digest([0; 32]),
            },
        };
        connection.mirror = Some(
            ReplicaMirror::from_seed(torse_sim::replica::WorldReplicaSeed {
                binding,
                subscription: SubscriptionId(7),
                baseline: CommitCursor::initial(binding.world),
                state: ReplicaSnapshot {
                    side: 16,
                    bounds: torse_world::WorldBounds::new(0, 32).unwrap(),
                    states: Vec::new(),
                    sections: Vec::new(),
                    entities: Vec::new(),
                },
            })
            .unwrap(),
        );
        connection.subscribe(vec![SectionPos::ZERO]).unwrap();
        assert!(matches!(
            wire::decode::<ClientMessage>(
                &server.receive().unwrap().unwrap(),
                connection.budget,
                connection.limits
            )
            .unwrap(),
            ClientMessage::Unsubscribe {
                subscription: SubscriptionId(7)
            }
        ));
        assert!(matches!(
            wire::decode::<ClientMessage>(
                &server.receive().unwrap().unwrap(),
                connection.budget,
                connection.limits
            )
            .unwrap(),
            ClientMessage::Subscribe { .. }
        ));
        assert!(connection.subscribe(Vec::new()).is_err());
        assert!(server.receive().unwrap().is_none());
        assert_eq!(connection.request_seq, 0);
    }

    #[test]
    fn read_only_results_are_not_rejections() {
        use torse_sim::durable::{Reply, ReplyStatus};
        use torse_types::{CommitCursor, CommitSeq, OutputBudget, WorldId};

        let (client, mut server) = MemoryTransport::pair(default_limits()).expect("pair");
        let mut connection = Connection::new(client);
        let reply = Reply {
            status: ReplyStatus::ReadOnly,
            fields: Default::default(),
            at: CommitCursor {
                world: WorldId(1),
                seq: CommitSeq(1),
            },
        };
        let frame = {
            let mut bytes = Vec::new();
            wire::encode(
                &ServerMessage::Result {
                    seq: RequestSeq(1),
                    reply,
                },
                &mut bytes,
                OutputBudget {
                    max_items: 1000,
                    max_bytes: 65_536,
                },
                codec_limits(),
            )
            .expect("encode");
            bytes
        };
        server.send(frame).expect("send");
        let events: Vec<_> = connection
            .poll()
            .expect("poll")
            .into_iter()
            .map(|event| event.event)
            .collect();
        match &events[..] {
            [NetEvent::Finished { seq, committed }] => {
                assert_eq!(seq.0, 1);
                assert!(committed, "read-only is a legitimate outcome");
            }
            other => panic!("unexpected events {other:?}"),
        }
    }

    fn default_limits() -> TransportLimits {
        TransportLimits {
            frame: 8_388_608,
            queued_frames: 32,
            queued_bytes: 16_777_216,
            poll_bytes: 262_144,
        }
    }
}
