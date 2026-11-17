use crate::network::ServerMessage;
use torse_codec::{CodecLimits, wire};
use torse_types::{Fault, OutputBudget};

/// Include the root envelope and decoder-owned storage, not only body bytes.
pub(crate) fn check_frame(
    message: &ServerMessage,
    budget: OutputBudget,
    codec: CodecLimits,
) -> Result<(), Fault> {
    let mut bytes = Vec::new();
    wire::encode(message, &mut bytes, budget, codec)?;
    let _: ServerMessage = wire::decode(&bytes, budget, codec)?;
    Ok(())
}
