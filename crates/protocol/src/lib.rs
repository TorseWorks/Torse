//! Versioned application protocol; identities are assigned by the host.
#![forbid(unsafe_code)]
mod messages;
pub use messages::{
    CONTROL_API_VERSION, ClientMessage, HEARTBEAT_INTERVAL, MoveCommand, PROTOCOL_VERSION,
    PlayerUpdate, ServerMessage,
};
