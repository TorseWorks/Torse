//! Optional control samples for an established peer.
//! The dispatcher sees the host viewer and may enqueue input. It has no
//! authority handle, so it cannot commit world state or emit a world receipt.
use super::*;
use std::collections::VecDeque;
use torse_protocol::CONTROL_API_VERSION;

/// What to do when a peer's control queue or byte budget is already full.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlPolicy {
    /// Keep the newest sample and drop the previous queued sample.
    Coalesce,
    /// Leave the queue unchanged and refuse the new sample.
    Reject,
}

/// Separate from action-request and movement limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlBudget {
    pub messages_per_tick: u32,
    pub queue_messages: usize,
    pub queue_bytes: u64,
    pub max_message_bytes: u64,
    pub policy: ControlPolicy,
}

impl ControlBudget {
    pub(super) fn validate(self) -> Result<(), Fault> {
        if self.messages_per_tick == 0
            || self.queue_messages == 0
            || self.queue_bytes == 0
            || self.max_message_bytes == 0
            || self.max_message_bytes > self.queue_bytes
        {
            return Err(Fault::invalid_data("control budget"));
        }
        Ok(())
    }
}

/// Host callback for one negotiated control extension.
pub trait ControlDispatch: Send {
    fn on_established(&mut self, peer: u128, viewer: Viewer);
    fn on_disconnected(&mut self, peer: u128, viewer: Viewer);
    fn on_control(
        &mut self,
        peer: u128,
        viewer: Viewer,
        seq: u64,
        payload: &[u8],
    ) -> Result<(), Fault>;
}

pub(super) struct ControlPlane {
    pub budget: ControlBudget,
    pub dispatch: Box<dyn ControlDispatch>,
}

pub(super) struct PeerLink {
    established: bool,
    queue: VecDeque<QueuedControl>,
    bytes: u64,
    tick: Tick,
    received: u32,
    last_seq: u64,
}

struct QueuedControl {
    seq: u64,
    payload: Vec<u8>,
}

enum Admit {
    Queued,
    Coalesced(Vec<u64>),
    Rejected,
}

impl PeerLink {
    fn new(tick: Tick) -> Self {
        Self {
            // This becomes true only after ControlReady is accepted by the
            // transport and the paired established callback has run.
            established: false,
            queue: VecDeque::new(),
            bytes: 0,
            tick,
            received: 0,
            last_seq: 0,
        }
    }

    fn admit(&mut self, budget: ControlBudget, tick: Tick, seq: u64, payload: Vec<u8>) -> Admit {
        if self.tick != tick {
            self.tick = tick;
            self.received = 0;
        }
        let size = payload.len() as u64;
        if self.received >= budget.messages_per_tick
            || size > budget.max_message_bytes
            || seq <= self.last_seq
        {
            return Admit::Rejected;
        }
        let fits = self.queue.len() < budget.queue_messages
            && self.bytes.saturating_add(size) <= budget.queue_bytes;
        if fits {
            self.push(seq, payload);
            return Admit::Queued;
        }
        if budget.policy == ControlPolicy::Reject {
            return Admit::Rejected;
        }
        // Control samples describe the latest client intent. Once the queue is
        // full, keeping any older sample would make the dispatcher act on stale
        // input before the newest one. Coalescing therefore replaces the whole
        // queued prefix with the incoming sample.
        let mut dropped = Vec::new();
        while let Some(old) = self.queue.pop_front() {
            self.bytes = self.bytes.saturating_sub(old.payload.len() as u64);
            dropped.push(old.seq);
        }
        if self.queue.len() >= budget.queue_messages
            || self.bytes.saturating_add(size) > budget.queue_bytes
        {
            return Admit::Rejected;
        }
        self.push(seq, payload);
        Admit::Coalesced(dropped)
    }

    fn push(&mut self, seq: u64, payload: Vec<u8>) {
        self.bytes = self.bytes.saturating_add(payload.len() as u64);
        self.received += 1;
        self.last_seq = seq;
        self.queue.push_back(QueuedControl { seq, payload });
    }
}

impl<T: Transport> NetworkHub<T> {
    /// Install before any peer. The dispatcher outlives those peers.
    pub fn set_control(
        &mut self,
        budget: ControlBudget,
        dispatch: Box<dyn ControlDispatch>,
    ) -> Result<(), Fault> {
        if self.control.is_some() || !self.peers.is_empty() {
            return Err(Fault::invalid_data("control dispatch requires a fresh hub"));
        }
        budget.validate()?;
        self.control = Some(ControlPlane { budget, dispatch });
        Ok(())
    }

    pub(super) fn control_reserve(&self) -> u64 {
        self.control
            .as_ref()
            .map(|control| control.budget.queue_bytes)
            .unwrap_or(0)
    }

    pub(super) fn offer_control<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        peer_id: u128,
        peer: &mut Peer<T>,
        server: &Server<ExecutionHost, J, S>,
        plane: &mut Option<ControlPlane>,
        api: u16,
        limits: NetworkLimits,
    ) -> Result<(), Fault> {
        let viewer = peer
            .viewer
            .ok_or_else(|| Fault::invalid_data("handshake required"))?;
        let Some(plane) = plane.as_mut() else {
            return Self::send(
                peer,
                ServerMessage::Rejected(RejectCode::PermissionDenied),
                limits,
            );
        };
        if api != CONTROL_API_VERSION || peer.control_link.is_some() {
            return Self::send(
                peer,
                ServerMessage::Rejected(RejectCode::PermissionDenied),
                limits,
            );
        }
        let tick = server.authority()?.resume_point().tick;
        peer.control_link = Some(PeerLink::new(tick));
        Self::send(peer, ServerMessage::ControlReady { api }, limits)?;
        peer.control_link
            .as_mut()
            .expect("control link was just installed")
            .established = true;
        plane.dispatch.on_established(peer_id, viewer);
        Ok(())
    }

    pub(super) fn receive_control<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        peer: &mut Peer<T>,
        server: &Server<ExecutionHost, J, S>,
        plane: &mut Option<ControlPlane>,
        seq: u64,
        payload: Vec<u8>,
        limits: NetworkLimits,
    ) -> Result<(), Fault> {
        let _viewer = peer
            .viewer
            .ok_or_else(|| Fault::invalid_data("handshake required"))?;
        if seq == 0 || payload.is_empty() {
            return Err(Fault::invalid_data(
                "control sample requires a sequence and bytes",
            ));
        }
        let Some(plane) = plane.as_ref() else {
            return Err(Fault::invalid_data("control extension is not negotiated"));
        };
        if peer.control_link.is_none() {
            return Err(Fault::invalid_data("control extension is not negotiated"));
        }
        let tick = server.authority()?.resume_point().tick;
        let budget = plane.budget;
        let outcome = peer
            .control_link
            .as_mut()
            .expect("control link was checked")
            .admit(budget, tick, seq, payload);
        match outcome {
            Admit::Queued => Ok(()),
            Admit::Coalesced(dropped) => {
                for seq in dropped {
                    Self::send(
                        peer,
                        ServerMessage::ControlAck {
                            seq,
                            accepted: false,
                        },
                        limits,
                    )?;
                }
                Ok(())
            }
            Admit::Rejected => Self::send(
                peer,
                ServerMessage::ControlAck {
                    seq,
                    accepted: false,
                },
                limits,
            ),
        }
    }

    pub(super) fn deliver_control(
        peer_id: u128,
        peer: &mut Peer<T>,
        plane: &mut ControlPlane,
        limits: NetworkLimits,
    ) -> Result<(), Fault> {
        let Some(viewer) = peer.viewer else {
            return Ok(());
        };
        let mut delivered = 0u32;
        loop {
            if delivered >= plane.budget.messages_per_tick {
                break;
            }
            let Some(message) = peer.control_link.as_mut().and_then(|link| {
                let message = link.queue.pop_front()?;
                link.bytes = link.bytes.saturating_sub(message.payload.len() as u64);
                Some(message)
            }) else {
                break;
            };
            let accepted = plane
                .dispatch
                .on_control(peer_id, viewer, message.seq, &message.payload)
                .is_ok();
            Self::send(
                peer,
                ServerMessage::ControlAck {
                    seq: message.seq,
                    accepted,
                },
                limits,
            )?;
            delivered += 1;
        }
        Ok(())
    }

    pub(super) fn release_control(plane: &mut ControlPlane, peer_id: u128, peer: &Peer<T>) {
        if peer
            .control_link
            .as_ref()
            .is_some_and(|link| link.established)
            && let Some(viewer) = peer.viewer
        {
            plane.dispatch.on_disconnected(peer_id, viewer);
        }
    }
}
