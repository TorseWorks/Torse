//! Ephemeral read-only terrain streams, independent of simulation subscriptions.
use crate::{Binding, CommitCursor, Digest, SectionPos};

/// Safety ceiling; each host and receiver must choose smaller explicit budgets.
pub const MAX_TERRAIN_INTEREST_SECTIONS: usize = 131_072;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerrainReset {
    pub sections: Vec<SectionPos>,
    /// Content already delivered and still valid at this reset's durable cut.
    pub retained: Vec<(SectionPos, Digest)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerrainUnavailable {
    Pending,
    Missing,
    Corrupt,
    Budget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerrainPage<S> {
    pub at: CommitCursor,
    pub digest: Digest,
    /// Exactly one terrain section, with no entities or private runtime state.
    pub terrain: S,
    pub light: Option<TerrainLight>,
}

/// Derived light for exactly one terrain content identity. Generation orders
/// boundary-only changes even while the target's terrain digest is unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerrainLight {
    pub pos: SectionPos,
    pub terrain: Digest,
    pub generation: u64,
    pub rules: u32,
    /// Identity of the immutable optical table used by both channels.
    pub optics: Digest,
    pub complete: bool,
    pub emitted: Vec<u8>,
    pub sky: Vec<u8>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerrainLightUpdate {
    pub binding: Binding,
    pub interest_generation: u64,
    pub seq: u64,
    pub pages: Vec<TerrainLight>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerrainUpdate<S> {
    pub binding: Binding,
    pub generation: u64,
    pub seq: u64,
    pub through: CommitCursor,
    pub reset: Option<TerrainReset>,
    pub pages: Vec<TerrainPage<S>>,
    pub unavailable: Vec<(SectionPos, TerrainUnavailable)>,
}
