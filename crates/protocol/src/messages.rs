use torse_codec::{Decoder, EncodeResult, Encoder, wire::Wire};
use torse_types::*;
pub const PROTOCOL_VERSION: u16 = 3;
/// Negotiated separately from [`PROTOCOL_VERSION`] and from world payloads.
pub const CONTROL_API_VERSION: u16 = 1;
/// Movement sequencing is independent of ordinary action requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MoveCommand {
    pub session: SessionId,
    pub seq: u64,
    pub tick_hint: Tick,
    pub wish: [i8; 2],
    pub jump: bool,
}
/// Durable authoritative pose for the player bound to this connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlayerUpdate {
    pub session: SessionId,
    pub entity: EntityId,
    pub input_seq: u64,
    pub tick: Tick,
    pub position_bits: [u64; 3],
    pub vertical_micros: i64,
    pub grounded: bool,
    pub reset: bool,
}
impl Wire for MoveCommand {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        self.session.write(e)?;
        self.seq.write(e)?;
        self.tick_hint.write(e)?;
        (self.wish[0] as u8).write(e)?;
        (self.wish[1] as u8).write(e)?;
        self.jump.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok(Self {
            session: Wire::read(d)?,
            seq: Wire::read(d)?,
            tick_hint: Wire::read(d)?,
            wish: [u8::read(d)? as i8, u8::read(d)? as i8],
            jump: Wire::read(d)?,
        })
    }
}
impl Wire for PlayerUpdate {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        self.session.write(e)?;
        self.entity.write(e)?;
        self.input_seq.write(e)?;
        self.tick.write(e)?;
        for bits in self.position_bits {
            bits.write(e)?;
        }
        self.vertical_micros.write(e)?;
        self.grounded.write(e)?;
        self.reset.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok(Self {
            session: Wire::read(d)?,
            entity: Wire::read(d)?,
            input_seq: Wire::read(d)?,
            tick: Wire::read(d)?,
            position_bits: [Wire::read(d)?, Wire::read(d)?, Wire::read(d)?],
            vertical_micros: Wire::read(d)?,
            grounded: Wire::read(d)?,
            reset: Wire::read(d)?,
        })
    }
}
/// Wall-clock interval for authenticated client liveness, independent of rendering.
pub const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
#[derive(Debug)]
pub enum ClientMessage<I> {
    Hello {
        version: u16,
        compatibility: Compatibility,
    },
    Request {
        seq: RequestSeq,
        call: I,
    },
    Subscribe {
        sections: Vec<SectionPos>,
    },
    Unsubscribe {
        subscription: SubscriptionId,
    },
    Close,
    Ping,
    Move(MoveCommand),
    /// Ask to enable the control extension. Ignored by hosts that have no dispatcher.
    ControlOffer {
        api: u16,
    },
    /// One bounded sample. The host viewer is not read from this payload.
    Control {
        seq: u64,
        payload: Vec<u8>,
    },
}
#[derive(Debug)]
pub enum ServerMessage<O, S, C> {
    Welcome {
        binding: Binding,
        session: SessionId,
    },
    Admission {
        seq: RequestSeq,
        admission: Admission,
    },
    Result {
        seq: RequestSeq,
        reply: O,
    },
    Seed(ReplicaSeed<S>),
    Batch(ReplicaBatch<C>),
    Rejected(RejectCode),
    Player(PlayerUpdate),
    /// The offered control API was accepted for this connection.
    ControlReady {
        api: u16,
    },
    /// Transport admission for one control sample. This is not a world receipt.
    ControlAck {
        seq: u64,
        accepted: bool,
    },
}
fn invalid() -> Fault {
    Fault::invalid_data("unknown protocol message/tag")
}
impl<I: Wire> Wire for ClientMessage<I> {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        match self {
            Self::Hello {
                version,
                compatibility,
            } => {
                0u8.write(e)?;
                version.write(e)?;
                compatibility.write(e)
            }
            Self::Request { seq, call } => {
                1u8.write(e)?;
                seq.write(e)?;
                call.write(e)
            }
            Self::Subscribe { sections } => {
                2u8.write(e)?;
                sections.write(e)
            }
            Self::Unsubscribe { subscription } => {
                3u8.write(e)?;
                subscription.write(e)
            }
            Self::Close => 4u8.write(e),
            Self::Ping => 5u8.write(e),
            Self::Move(input) => {
                6u8.write(e)?;
                input.write(e)
            }
            Self::ControlOffer { api } => {
                7u8.write(e)?;
                api.write(e)
            }
            Self::Control { seq, payload } => {
                8u8.write(e)?;
                seq.write(e)?;
                payload.write(e)
            }
        }
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        match u8::read(d)? {
            0 => Ok(Self::Hello {
                version: Wire::read(d)?,
                compatibility: Wire::read(d)?,
            }),
            1 => Ok(Self::Request {
                seq: Wire::read(d)?,
                call: Wire::read(d)?,
            }),
            2 => Ok(Self::Subscribe {
                sections: Wire::read(d)?,
            }),
            3 => Ok(Self::Unsubscribe {
                subscription: Wire::read(d)?,
            }),
            4 => Ok(Self::Close),
            5 => Ok(Self::Ping),
            6 => Ok(Self::Move(Wire::read(d)?)),
            7 => Ok(Self::ControlOffer {
                api: Wire::read(d)?,
            }),
            8 => Ok(Self::Control {
                seq: Wire::read(d)?,
                payload: Wire::read(d)?,
            }),
            _ => Err(invalid()),
        }
    }
}
// Admission belongs to the authority vocabulary; use a local encoding helper
// rather than an orphan trait implementation in this protocol crate.
fn write_admission(value: &Admission, e: &mut Encoder<'_>) -> EncodeResult<()> {
    match value {
        Admission::Queued { target_tick } => {
            0u8.write(e)?;
            target_tick.write(e)
        }
        Admission::Duplicate => 1u8.write(e),
        Admission::Rejected(r) => {
            2u8.write(e)?;
            r.write(e)
        }
    }
}
fn read_admission(d: &mut Decoder<'_>) -> EncodeResult<Admission> {
    match u8::read(d)? {
        0 => Ok(Admission::Queued {
            target_tick: Wire::read(d)?,
        }),
        1 => Ok(Admission::Duplicate),
        2 => Ok(Admission::Rejected(Wire::read(d)?)),
        _ => Err(invalid()),
    }
}
impl<O: Wire, S: Wire, C: Wire> Wire for ServerMessage<O, S, C> {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        match self {
            Self::Welcome { binding, session } => {
                0u8.write(e)?;
                binding.write(e)?;
                session.write(e)
            }
            Self::Admission { seq, admission } => {
                1u8.write(e)?;
                seq.write(e)?;
                write_admission(admission, e)
            }
            Self::Result { seq, reply } => {
                2u8.write(e)?;
                seq.write(e)?;
                reply.write(e)
            }
            Self::Seed(seed) => {
                3u8.write(e)?;
                seed.write(e)
            }
            Self::Batch(batch) => {
                4u8.write(e)?;
                batch.write(e)
            }
            Self::Rejected(reason) => {
                5u8.write(e)?;
                reason.write(e)
            }
            Self::Player(update) => {
                6u8.write(e)?;
                update.write(e)
            }
            Self::ControlReady { api } => {
                7u8.write(e)?;
                api.write(e)
            }
            Self::ControlAck { seq, accepted } => {
                8u8.write(e)?;
                seq.write(e)?;
                accepted.write(e)
            }
        }
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        match u8::read(d)? {
            0 => Ok(Self::Welcome {
                binding: Wire::read(d)?,
                session: Wire::read(d)?,
            }),
            1 => Ok(Self::Admission {
                seq: Wire::read(d)?,
                admission: read_admission(d)?,
            }),
            2 => Ok(Self::Result {
                seq: Wire::read(d)?,
                reply: Wire::read(d)?,
            }),
            3 => Ok(Self::Seed(Wire::read(d)?)),
            4 => Ok(Self::Batch(Wire::read(d)?)),
            5 => Ok(Self::Rejected(Wire::read(d)?)),
            6 => Ok(Self::Player(Wire::read(d)?)),
            7 => Ok(Self::ControlReady {
                api: Wire::read(d)?,
            }),
            8 => Ok(Self::ControlAck {
                seq: Wire::read(d)?,
                accepted: Wire::read(d)?,
            }),
            _ => Err(invalid()),
        }
    }
}
