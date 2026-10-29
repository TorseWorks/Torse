//! Authenticated session/application assembly. Polling does bounded work outside
//! the commit path. Only durable reads, seeds, results and deltas enter transports.
use crate::authentication::IdentityVerifier;
use crate::execution::{ExecutionHost, ExecutionModel};
use crate::{HostAuthority, HostCheckpointStore, InvalidationSink, Server};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use torse_codec::{CodecLimits, wire};
use torse_net::{SessionRegistry, Transport};
use torse_protocol::PROTOCOL_VERSION;
use torse_sim::action::ActorContext;
use torse_sim::durable::Receipt;
use torse_sim::replica::{ReplicaScope, WorldReplicaBatch, WorldReplicaSeed};
use torse_storage::CommitJournal;
use torse_types::*;
#[path = "network_control.rs"]
mod control;
#[path = "network_interest.rs"]
mod interest;
#[path = "network_player.rs"]
mod player;
pub use control::{ControlBudget, ControlDispatch, ControlPolicy};
pub use interest::PlayerFollow;

pub type ClientMessage = torse_protocol::ClientMessage<torse_sim::durable::Call>;
pub type ServerMessage = torse_protocol::ServerMessage<
    torse_sim::durable::Reply,
    torse_sim::replica::ReplicaSnapshot,
    torse_sim::replica::ReplicaChanges,
>;

impl<J: CommitJournal<ExecutionModel>, S: HostCheckpointStore<ExecutionModel>>
    Server<ExecutionHost, J, S>
{
    /// Reserve journal/checkpoint capacity and connect the recovered authority to one account.
    pub fn attach_memory(&mut self, pool: MemoryPool, storage_bytes: u64) -> Result<(), Fault> {
        self.authority()?;
        if self.storage_memory.is_some() {
            return Err(Fault::invalid_data("server memory already attached"));
        }
        let storage = pool.reserve(MemoryClass::Codec, storage_bytes, EvictionRank::Pinned)?;
        self.authority.attach_memory(pool)?;
        self.storage_memory = Some(storage);
        Ok(())
    }
    pub fn register_session(
        &mut self,
        viewer: Viewer,
        actor: ActorContext,
        scope: ReplicaScope,
    ) -> Result<(), Fault> {
        self.authority()?;
        self.authority.register_session(viewer, actor, scope)
    }
    /// Retire old authorized projections and return their independent control
    /// signal. Hosts must clear downstream copies before further sends.
    pub fn replace_session_scope(
        &mut self,
        session: SessionId,
        actor: ActorContext,
        scope: ReplicaScope,
    ) -> Result<Vec<SubscriptionId>, Fault> {
        self.authority()?;
        self.authority.replace_session_scope(session, actor, scope)
    }

    pub fn open_replica(
        &mut self,
        session: SessionId,
        scope: ReplicaScope,
        budget: OutputBudget,
    ) -> Result<WorldReplicaSeed, Fault> {
        self.authority()?;
        let seed = self.authority.open_replica(session, scope, budget)?;
        if seed.binding != self.binding() || seed.baseline.seq.0 > self.durability().through.0 {
            return Err(Fault::new(
                FaultCode::InvalidState,
                "undurable replica seed",
            ));
        }
        Ok(seed)
    }
    pub fn take_replica(
        &mut self,
        id: SubscriptionId,
        budget: OutputBudget,
    ) -> Result<Vec<WorldReplicaBatch>, Fault> {
        self.authority()?;
        let batches = self.authority.take_replica(id, budget)?;
        if batches
            .iter()
            .any(|b| b.binding != self.binding() || b.through.seq.0 > self.durability().through.0)
        {
            return Err(Fault::new(
                FaultCode::InvalidState,
                "undurable replica batch",
            ));
        }
        Ok(batches)
    }
    pub fn close_replica(&mut self, id: SubscriptionId) -> Result<(), Fault> {
        self.authority()?;
        self.authority.close_replica(id)
    }
    pub fn disconnect(&mut self, session: SessionId) -> Result<Vec<SubscriptionId>, Fault> {
        self.authority()?;
        let ids = self.authority.revoke_session(session);
        for id in &ids {
            self.authority.close_replica(*id)?;
        }
        Ok(ids)
    }
    pub fn receipt(&self, id: RequestId) -> Result<Option<&Receipt>, Fault> {
        let receipt = self.authority()?.receipt(id);
        if receipt.is_some_and(|r| r.reply.at.seq.0 > self.durability().through.0) {
            return Err(Fault::new(
                FaultCode::InvalidState,
                "undurable request result",
            ));
        }
        Ok(receipt)
    }
    pub fn set_runtime_optimization(&mut self, enabled: bool) -> Result<(), Fault> {
        self.authority()?;
        self.authority.set_runtime_optimization(enabled);
        Ok(())
    }
    pub fn set_projection_optimization(&mut self, enabled: bool) -> Result<(), Fault> {
        self.authority()?;
        self.authority.set_projection_optimization(enabled);
        Ok(())
    }
    /// Finish one logical tick, including a recovered partial tick, within an
    /// explicit work budget. This never skips a tick or invokes future events.
    pub fn drive_tick(
        &mut self,
        sink: &mut impl InvalidationSink,
        max_steps: u32,
    ) -> Result<u32, Fault> {
        let target = self
            .authority()?
            .resume_point()
            .tick
            .0
            .checked_add(1)
            .ok_or_else(|| Fault::invalid_data("tick exhausted"))?;
        let mut steps = 0;
        if max_steps > 0
            && let crate::residency::ResidencyProgress::Published(step) =
                self.poll_residency(sink)?
        {
            match step {
                crate::CommitStep::Durable(_) => steps = 1,
                crate::CommitStep::Blocked(_) => return Ok(0),
                crate::CommitStep::Idle => {}
            }
        }
        while steps < max_steps {
            let batch = self.drive_batch(sink, self.commit_batch_limit.min(max_steps - steps))?;
            steps += batch.committed;
            if batch.idle {
                return Ok(steps);
            }
            if let Some(error) = batch.blocked
                && (!matches!(error, StageError::Busy) || batch.committed == 0)
            {
                return Err(Fault::resource_limit(
                    "journal blocked; tick remains recoverable",
                ));
            }
            let authority = self.authority()?;
            if authority.at_tick_boundary() && authority.resume_point().tick.0 == target {
                return Ok(steps);
            }
        }
        Err(Fault::resource_limit("tick step budget exhausted"))
    }
}

#[derive(Clone, Copy)]
pub struct NetworkLimits {
    pub peers: usize,
    pub movement_inputs: usize,
    /// Conservative queue-node reservation, charged at 512 bytes per input.
    pub movement_bytes: u64,
    pub requests_per_tick: u32,
    pub frames_per_poll: usize,
    pub outstanding: usize,
    pub subscriptions: usize,
    pub idle_ticks: u64,
    pub budget: OutputBudget,
    pub codec: CodecLimits,
}
impl NetworkLimits {
    /// Validate an application's wall-clock schedule. Tests and virtual-time
    /// hosts may use smaller tick windows with their own heartbeat schedule.
    pub fn validate_timing(&self, tick_period: std::time::Duration) -> Result<(), Fault> {
        let window = tick_period
            .as_nanos()
            .checked_mul(u128::from(self.idle_ticks));
        let minimum = torse_protocol::HEARTBEAT_INTERVAL.as_nanos() * 3;
        if tick_period.is_zero() || window.is_none_or(|window| window < minimum) {
            return Err(Fault::invalid_data(
                "idle window requires three heartbeat intervals",
            ));
        }
        Ok(())
    }
}
/// Trusted application policy, constructed before an unauthenticated peer can
/// submit data. Authentication plugins may construct a different policy per peer.
pub struct PeerPolicy {
    pub actor: ActorContext,
    pub scope: ReplicaScope,
}
struct Peer<T> {
    user: Option<ContentKey>,
    player: Option<player::Control>,
    transport: T,
    policy: PeerPolicy,
    viewer: Option<Viewer>,
    subscriptions: BTreeSet<u128>,
    waiting: BTreeSet<u64>,
    closed: bool,
    last_tick: Tick,
    last_activity: Instant,
    deferred: Option<ReplicaScope>,
    codec: Option<MemoryLease>,
    memory: Option<MemoryLease>,
    follow_scope: Option<ReplicaScope>,
    follow_ready: bool,
    control_link: Option<control::PeerLink>,
}
pub struct NetworkHub<T> {
    peers: BTreeMap<u128, Peer<T>>,
    next: u128,
    sessions: SessionRegistry,
    limits: NetworkLimits,
    binding: Binding,
    memory: Option<MemoryPool>,
    codec: Option<MemoryLease>,
    authentication: Option<Arc<dyn IdentityVerifier>>,
    _authentication_memory: Option<MemoryLease>,
    connection_timeout: Option<Duration>,
    follow: Option<PlayerFollow>,
    control: Option<control::ControlPlane>,
}
impl<T: Transport> NetworkHub<T> {
    pub fn new(binding: Binding, limits: NetworkLimits) -> Result<Self, Fault> {
        if limits.frames_per_poll == 0
            || limits.movement_inputs == 0
            || limits.movement_inputs > 4096
            || limits.movement_bytes < limits.movement_inputs as u64 * 512
            || limits.outstanding == 0
            || limits.subscriptions == 0
            || limits.idle_ticks == 0
            || limits.budget.max_items == 0
            || limits.budget.max_bytes == 0
        {
            return Err(Fault::invalid_data("network limits"));
        }
        Ok(Self {
            peers: BTreeMap::new(),
            next: 1,
            sessions: SessionRegistry::new(
                limits
                    .peers
                    .checked_mul(2)
                    .ok_or_else(|| Fault::resource_limit("session capacity overflow"))?,
                limits.requests_per_tick,
            )?,
            limits,
            binding,
            memory: None,
            codec: None,
            authentication: None,
            _authentication_memory: None,
            connection_timeout: None,
            follow: None,
            control: None,
        })
    }
    pub fn with_memory(
        binding: Binding,
        limits: NetworkLimits,
        memory: MemoryPool,
    ) -> Result<Self, Fault> {
        Ok(Self {
            codec: Some(
                memory.reserve(
                    MemoryClass::Codec,
                    limits
                        .budget
                        .max_bytes
                        .saturating_mul(2)
                        .saturating_add(65_536),
                    EvictionRank::Pinned,
                )?,
            ),
            memory: Some(memory),
            ..Self::new(binding, limits)?
        })
    }
    pub fn add(&mut self, mut transport: T, policy: PeerPolicy, tick: Tick) -> Result<u128, Fault> {
        if self.authentication.is_some()
            && transport.security() == torse_net::tls::TransportSecurity::Unprotected
        {
            transport.close();
            return Err(Fault::incompatible("authenticated hub requires mutual TLS"));
        }
        if self.peers.len() >= self.limits.peers || self.next == u128::MAX {
            return Err(Fault::resource_limit("peer capacity"));
        }
        let memory = self
            .memory
            .as_ref()
            .map(|pool| {
                transport.attach_memory(pool.clone())?;
                let bytes = policy
                    .actor
                    .accounted_bytes()
                    .saturating_mul(2)
                    .saturating_add(policy.scope.sections.len() as u64 * 8192)
                    .saturating_add(self.limits.movement_bytes)
                    .saturating_add(self.limits.outstanding as u64 * 512)
                    .saturating_add(self.control_reserve());
                pool.reserve(MemoryClass::Executor, bytes, EvictionRank::Pinned)
            })
            .transpose()?;
        let id = self.next;
        self.next += 1;
        self.peers.insert(
            id,
            Peer {
                user: None,
                player: None,
                transport,
                policy,
                viewer: None,
                subscriptions: BTreeSet::new(),
                waiting: BTreeSet::new(),
                closed: false,
                last_tick: tick,
                last_activity: Instant::now(),
                deferred: None,
                codec: None,
                memory,
                follow_scope: None,
                follow_ready: false,
                control_link: None,
            },
        );
        Ok(id)
    }
    /// Install before accepting peers. A user may have only one live connection.
    pub fn require_authentication(
        &mut self,
        verifier: Arc<dyn IdentityVerifier>,
    ) -> Result<(), Fault> {
        if !self.peers.is_empty() || self.authentication.is_some() {
            return Err(Fault::invalid_data("authentication requires a fresh hub"));
        }
        let memory = self
            .memory
            .as_ref()
            .map(|pool| {
                pool.reserve(
                    MemoryClass::Resource,
                    verifier.accounted_bytes(),
                    EvictionRank::Pinned,
                )
            })
            .transpose()?;
        self.authentication = Some(verifier);
        self.connection_timeout
            .get_or_insert(Duration::from_secs(60));
        self._authentication_memory = memory;
        Ok(())
    }
    pub fn user(&self, id: u128) -> Option<&ContentKey> {
        self.peers.get(&id)?.user.as_ref()
    }
    pub fn set_connection_timeout(&mut self, timeout: Duration) -> Result<(), Fault> {
        if timeout < torse_protocol::HEARTBEAT_INTERVAL * 3 {
            return Err(Fault::invalid_data(
                "connection timeout requires three heartbeat intervals",
            ));
        }
        self.connection_timeout = Some(timeout);
        Ok(())
    }
    pub fn contains(&self, id: u128) -> bool {
        self.peers.contains_key(&id)
    }
    pub fn revoke<J: CommitJournal<ExecutionModel>, S: HostCheckpointStore<ExecutionModel>>(
        &mut self,
        server: &mut Server<ExecutionHost, J, S>,
        id: u128,
    ) -> Result<(), Fault> {
        if let Some(peer) = self.peers.get_mut(&id) {
            peer.closed = true;
            peer.transport.close();
        }
        self.cleanup(server)
    }
    /// Trusted application-driven scope replacement. Re-seed live subscriptions
    /// before retiring sections so residency invalidation cannot close this peer.
    /// Failure after replacing grants closes and revokes the connection.
    pub fn replace_policy<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        &mut self,
        server: &mut Server<ExecutionHost, J, S>,
        id: u128,
        policy: PeerPolicy,
    ) -> Result<(), Fault> {
        let control_bytes = self.control_reserve();
        let peer = self
            .peers
            .get_mut(&id)
            .ok_or_else(|| Fault::invalid_data("unknown peer"))?;
        if peer.closed {
            return Err(Fault::invalid_data("closed peer"));
        }
        if peer
            .user
            .as_ref()
            .is_some_and(|user| user != policy.actor.principal())
        {
            return Err(Fault::invalid_data(
                "scope replacement cannot change the authenticated user",
            ));
        }
        let policy_bytes = policy
            .actor
            .accounted_bytes()
            .saturating_mul(2)
            .saturating_add(policy.scope.sections.len() as u64 * 8192)
            .saturating_add(self.limits.movement_bytes)
            .saturating_add(control_bytes)
            .saturating_add(self.limits.outstanding as u64 * 512);
        let mut growth = peer
            .memory
            .as_ref()
            .map(|lease| {
                lease.pool().reserve(
                    MemoryClass::Executor,
                    policy_bytes.saturating_sub(lease.bytes()),
                    EvictionRank::Pinned,
                )
            })
            .transpose()?;
        if let Some(viewer) = peer.viewer {
            let invalidated = server.replace_session_scope(
                viewer.session,
                policy.actor.clone(),
                policy.scope.clone(),
            )?;
            // After authorization changes, a failed replacement cannot leave a
            // connection running with its old policy or projection. Retire all
            // old IDs, including subscriptions opened directly by the host.
            let replacement = (|| -> Result<(), Fault> {
                peer.subscriptions.extend(invalidated.iter().map(|id| id.0));
                for old in std::mem::take(&mut peer.subscriptions) {
                    server.close_replica(SubscriptionId(old))?;
                }
                if self.follow.is_some() && peer.player.is_some() {
                    return Ok(());
                }
                let seed = server.open_replica(
                    viewer.session,
                    policy.scope.clone(),
                    self.limits.budget,
                )?;
                peer.subscriptions.insert(seed.subscription.0);
                Self::send(peer, ServerMessage::Seed(seed), self.limits)
            })();
            if let Err(error) = replacement {
                peer.closed = true;
                peer.transport.close();
                self.cleanup(server)?;
                return Err(error);
            }
        }
        peer.policy = policy;
        peer.deferred = None;
        peer.follow_scope = None;
        peer.follow_ready = false;
        if let (Some(memory), Some(growth)) = (&mut peer.memory, &mut growth) {
            memory.absorb(growth, policy_bytes)?;
        }
        Ok(())
    }

    fn send(
        peer: &mut Peer<T>,
        message: ServerMessage,
        limits: NetworkLimits,
    ) -> Result<(), Fault> {
        let _memory = if peer.codec.is_none() {
            peer.memory
                .as_ref()
                .map(|lease| {
                    lease.pool().reserve(
                        MemoryClass::Codec,
                        limits.budget.max_bytes.saturating_mul(2),
                        EvictionRank::Pinned,
                    )
                })
                .transpose()?
        } else {
            None
        };
        let mut bytes = Vec::new();
        wire::encode(&message, &mut bytes, limits.budget, limits.codec)?;
        peer.transport.send(bytes)
    }
    #[allow(clippy::too_many_arguments)]
    fn handle<J: CommitJournal<ExecutionModel>, S: HostCheckpointStore<ExecutionModel>>(
        peer: &mut Peer<T>,
        sessions: &mut SessionRegistry,
        server: &mut Server<ExecutionHost, J, S>,
        message: ClientMessage,
        limits: NetworkLimits,
        following: bool,
        peer_id: u128,
        control: &mut Option<control::ControlPlane>,
    ) -> Result<(), Fault> {
        match message {
            ClientMessage::Hello {
                version,
                compatibility,
            } => {
                if peer.viewer.is_some()
                    || version != PROTOCOL_VERSION
                    || compatibility != server.binding().compatibility
                {
                    return Err(Fault::incompatible(
                        "protocol or authority content mismatch",
                    ));
                }
                let viewer = sessions.open()?;
                if let Err(e) = server.register_session(
                    viewer,
                    peer.policy.actor.clone(),
                    peer.policy.scope.clone(),
                ) {
                    sessions.close(viewer.session);
                    return Err(e);
                }
                peer.viewer = Some(viewer);
                if let Some(control) = &mut peer.player {
                    let child = sessions.open()?;
                    server.authority()?;
                    if let Err(error) =
                        server
                            .authority
                            .bind_player(viewer.session, child, control.entity)
                    {
                        sessions.close(child.session);
                        return Err(error);
                    }
                    control.viewer = Some(child);
                }
                Self::send(
                    peer,
                    ServerMessage::Welcome {
                        binding: server.binding(),
                        session: viewer.session,
                    },
                    limits,
                )?;
                if peer.player.is_some() {
                    Self::send_player(peer, server, false, limits)?;
                }
                Ok(())
            }
            ClientMessage::Request { seq, call } => {
                let viewer = peer
                    .viewer
                    .ok_or_else(|| Fault::invalid_data("handshake required"))?;
                let tick = server.authority()?.resume_point().tick;
                let admitted = match sessions.check(viewer.session, seq, tick) {
                    Err(reason) => Admission::Rejected(reason),
                    Ok(())
                        if peer.waiting.len() >= limits.outstanding
                            && !peer.waiting.contains(&seq.0) =>
                    {
                        Admission::Rejected(RejectCode::QuotaExceeded)
                    }
                    Ok(()) if call.action == crate::execution::movement_action() => {
                        Admission::Rejected(RejectCode::PermissionDenied)
                    }
                    Ok(()) => server.submit(WorldRequest {
                        id: RequestId {
                            world: server.binding().world,
                            epoch: server.binding().epoch,
                            session: viewer.session,
                            seq,
                        },
                        actor: viewer.actor,
                        input: call,
                    }),
                };
                if matches!(admitted, Admission::Queued { .. } | Admission::Duplicate) {
                    sessions.admitted(viewer.session, seq);
                    peer.waiting.insert(seq.0);
                }
                Self::send(
                    peer,
                    ServerMessage::Admission {
                        seq,
                        admission: admitted,
                    },
                    limits,
                )
            }
            ClientMessage::Subscribe { sections } => {
                let viewer = peer
                    .viewer
                    .ok_or_else(|| Fault::invalid_data("handshake required"))?;
                if !following && peer.subscriptions.len() >= limits.subscriptions {
                    return Err(Fault::resource_limit("peer subscription capacity"));
                }
                let scope = ReplicaScope {
                    sections: sections.iter().copied().collect(),
                };
                if scope.sections.len() != sections.len()
                    || !scope.sections.is_subset(&peer.policy.scope.sections)
                {
                    return Self::send(
                        peer,
                        ServerMessage::Rejected(RejectCode::PermissionDenied),
                        limits,
                    );
                }
                if following {
                    return Ok(());
                }
                match server.open_replica(viewer.session, scope.clone(), limits.budget) {
                    Ok(seed) => {
                        peer.subscriptions.insert(seed.subscription.0);
                        Self::send(peer, ServerMessage::Seed(seed), limits)
                    }
                    Err(error)
                        if error.code == FaultCode::ResourceLimit && peer.memory.is_some() =>
                    {
                        if peer.deferred.is_some() {
                            return Err(Fault::resource_limit("pending subscription capacity"));
                        }
                        peer.deferred = Some(scope);
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
            ClientMessage::Unsubscribe { subscription } => {
                if !peer.subscriptions.remove(&subscription.0) {
                    return Self::send(
                        peer,
                        ServerMessage::Rejected(RejectCode::PermissionDenied),
                        limits,
                    );
                }
                peer.follow_ready = false;
                server.close_replica(subscription)
            }
            ClientMessage::Ping => {
                peer.viewer
                    .ok_or_else(|| Fault::invalid_data("handshake required"))?;
                Ok(())
            }
            ClientMessage::Move(input) => Self::receive_movement(peer, server, input, limits),
            ClientMessage::ControlOffer { api } => {
                Self::offer_control(peer_id, peer, server, control, api, limits)
            }
            ClientMessage::Control { seq, payload } => {
                Self::receive_control(peer, server, control, seq, payload, limits)
            }
            ClientMessage::Close => {
                peer.closed = true;
                peer.transport.close();
                Ok(())
            }
        }
    }
    pub fn poll<J: CommitJournal<ExecutionModel>, S: HostCheckpointStore<ExecutionModel>>(
        &mut self,
        server: &mut Server<ExecutionHost, J, S>,
    ) -> Result<(), Fault> {
        self.poll_at(server, Instant::now())
    }
    /// An explicit monotonic clock permits deterministic connection-deadline tests.
    pub fn poll_at<J: CommitJournal<ExecutionModel>, S: HostCheckpointStore<ExecutionModel>>(
        &mut self,
        server: &mut Server<ExecutionHost, J, S>,
        now: Instant,
    ) -> Result<(), Fault> {
        if server.binding() != self.binding {
            return Err(Fault::invalid_data("hub binding mismatch"));
        }
        if self.follow.is_some() {
            // Give residency one bounded opportunity before enqueuing the next
            // tick's continuous movement requests, otherwise unload can starve.
            server.poll_residency(self)?;
        }
        let tick = server.authority()?.resume_point().tick;
        let control_bytes = self.control_reserve();
        let mut control = self.control.take();
        let mut users: BTreeSet<_> = self
            .peers
            .values()
            .filter(|peer| !peer.closed)
            .filter_map(|peer| peer.user.clone())
            .collect();
        let mut players: BTreeSet<_> = self
            .peers
            .values()
            .filter(|peer| !peer.closed)
            .filter_map(|peer| peer.player.as_ref().map(|player| player.entity))
            .collect();
        for (peer_id, peer) in self.peers.iter_mut() {
            if peer.closed {
                continue;
            }
            // Keep one control allowance for the hub's lifetime. Full resident
            // budgets must still allow an unsubscribe or close to be decoded.
            peer.codec = self.codec.take();
            let result = (|| -> Result<(), Fault> {
                if tick.0.saturating_sub(peer.last_tick.0) > self.limits.idle_ticks
                    || (if peer.viewer.is_none() && self.authentication.is_some() {
                        Some(Duration::from_secs(15))
                    } else {
                        self.connection_timeout
                    })
                    .is_some_and(|timeout| {
                        now.saturating_duration_since(peer.last_activity) > timeout
                    })
                {
                    return Err(Fault::invalid_data("idle peer"));
                }
                peer.transport.poll()?;
                for _ in 0..self.limits.frames_per_poll {
                    let Some(bytes) = peer.transport.receive()? else {
                        break;
                    };
                    let message: ClientMessage =
                        wire::decode(&bytes, self.limits.budget, self.limits.codec)?;
                    if matches!(&message, ClientMessage::Hello { .. })
                        && let Some(verifier) = &self.authentication
                    {
                        if peer.viewer.is_some() {
                            return Err(Fault::invalid_data("duplicate authenticated handshake"));
                        }
                        if !matches!(
                            peer.transport.security(),
                            torse_net::tls::TransportSecurity::MutualTls(_)
                        ) {
                            return Err(Fault::incompatible("TLS identity is not verified"));
                        }
                        let grant = verifier.verify(peer.transport.security())?;
                        grant.validate(server.authority()?.replica_section_limit())?;
                        if users.contains(&grant.user)
                            || grant.player.is_some_and(|entity| players.contains(&entity))
                        {
                            return Err(Fault::incompatible(
                                "authenticated user or player is already connected",
                            ));
                        }
                        let policy_bytes = grant
                            .actor
                            .accounted_bytes()
                            .saturating_mul(2)
                            .saturating_add(grant.scope.sections.len() as u64 * 8192)
                            .saturating_add(self.limits.movement_bytes)
                            .saturating_add(self.limits.outstanding as u64 * 512)
                            .saturating_add(control_bytes);
                        if let Some(memory) = &mut peer.memory {
                            memory.resize(policy_bytes)?;
                        }
                        users.insert(grant.user.clone());
                        peer.user = Some(grant.user);
                        peer.policy = PeerPolicy {
                            actor: grant.actor,
                            scope: grant.scope,
                        };
                        peer.player = grant.player.map(|entity| {
                            players.insert(entity);
                            player::Control::new(entity, self.limits.movement_inputs)
                        });
                    }
                    peer.last_tick = tick;
                    peer.last_activity = now;
                    let following = self.follow.is_some() && peer.player.is_some();
                    Self::handle(
                        peer,
                        &mut self.sessions,
                        server,
                        message,
                        self.limits,
                        following,
                        *peer_id,
                        &mut control,
                    )?;
                    if peer.closed {
                        return Ok(());
                    }
                }
                if let Some(plane) = control.as_mut() {
                    Self::deliver_control(*peer_id, peer, plane, self.limits)?;
                }
                if let Some(viewer) = peer.viewer {
                    if let Some(config) = self.follow
                        && peer.player.is_some()
                    {
                        Self::update_follow(peer, server, config, self.limits)?;
                    }
                    if let Some(scope) = peer.deferred.as_ref() {
                        if !scope.sections.is_subset(&peer.policy.scope.sections) {
                            return Err(Fault::invalid_data("deferred scope lost authorization"));
                        }
                        match server.open_replica(viewer.session, scope.clone(), self.limits.budget)
                        {
                            Ok(seed) => {
                                peer.deferred = None;
                                peer.subscriptions.insert(seed.subscription.0);
                                Self::send(peer, ServerMessage::Seed(seed), self.limits)?;
                            }
                            Err(error) if error.code == FaultCode::ResourceLimit => {}
                            Err(error) => return Err(error),
                        }
                    }
                    let waiting: Vec<_> = peer.waiting.iter().copied().collect();
                    for seq in waiting {
                        let id = RequestId {
                            world: self.binding.world,
                            epoch: self.binding.epoch,
                            session: viewer.session,
                            seq: RequestSeq(seq),
                        };
                        if let Some(receipt) = server.receipt(id)? {
                            let reply = receipt.reply.clone();
                            Self::send(
                                peer,
                                ServerMessage::Result {
                                    seq: RequestSeq(seq),
                                    reply,
                                },
                                self.limits,
                            )?;
                            peer.waiting.remove(&seq);
                        } else if server.authority()?.expired(id) {
                            Self::send(
                                peer,
                                ServerMessage::Admission {
                                    seq: RequestSeq(seq),
                                    admission: Admission::Rejected(RejectCode::SequenceExpired),
                                },
                                self.limits,
                            )?;
                            peer.waiting.remove(&seq);
                        }
                    }
                    Self::poll_player(peer, server, self.limits)?;
                    for id in peer.subscriptions.iter().copied().collect::<Vec<_>>() {
                        for batch in server.take_replica(SubscriptionId(id), self.limits.budget)? {
                            Self::send(peer, ServerMessage::Batch(batch), self.limits)?;
                        }
                    }
                }
                peer.transport.poll()
            })();
            self.codec = peer.codec.take();
            if result.is_err() {
                peer.closed = true;
                peer.transport.close();
            }
        }
        self.control = control;
        self.cleanup(server)?;
        self.update_follow_residency(server)
    }
    fn cleanup<J: CommitJournal<ExecutionModel>, S: HostCheckpointStore<ExecutionModel>>(
        &mut self,
        server: &mut Server<ExecutionHost, J, S>,
    ) -> Result<(), Fault> {
        let dead: Vec<_> = self
            .peers
            .iter()
            .filter(|(_, p)| p.closed)
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            let mut peer = self.peers.remove(&id).expect("peer exists");
            if let Some(plane) = &mut self.control {
                Self::release_control(plane, id, &peer);
            }
            peer.transport.close();
            if let Some(viewer) = peer.viewer {
                if let Some(child) = peer.player.as_ref().and_then(|p| p.viewer) {
                    self.sessions.close(child.session);
                }
                server.disconnect(viewer.session)?;
                self.sessions.close(viewer.session);
                for subscription in peer.subscriptions {
                    server.close_replica(SubscriptionId(subscription))?;
                }
            }
        }
        Ok(())
    }
}
impl<T: Transport> InvalidationSink for NetworkHub<T> {
    fn invalidate(
        &mut self,
        binding: Binding,
        subscriptions: &[SubscriptionId],
    ) -> Result<(), Fault> {
        if binding != self.binding {
            return Err(Fault::invalid_data("invalidation binding"));
        }
        for peer in self.peers.values_mut() {
            if subscriptions
                .iter()
                .any(|id| peer.subscriptions.contains(&id.0))
            {
                if self.follow.is_some() && peer.player.is_some() {
                    peer.follow_ready = false;
                    continue;
                }
                peer.closed = true;
                peer.transport.close();
                peer.waiting.clear();
            }
        }
        Ok(())
    }
}
