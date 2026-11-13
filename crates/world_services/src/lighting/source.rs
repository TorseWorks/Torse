//! Read-only lighting inputs. Unknown terrain is never implicit air.
use torse_types::{Binding, BlockPos, Digest, Revision, SectionPos};
use torse_world::{SectionStore, TerrainQuery, WorldBounds};

/// Identity of the cells observed at a section, including negative observations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightInputVersion {
    /// Content identity, independent of residency and storage revisions.
    Content(Digest),
    Unknown,
    Boundary,
    /// Compatibility adapter for callers using the native resident store.
    Resident {
        revision: Option<Revision>,
        readable: bool,
    },
}

/// Callers must keep versions stable exactly while the corresponding input is
/// unchanged. A known empty section has a content version and returns Ready(EMPTY).
/// Validated generated and saved pages use the same contract as resident cells.
pub trait LightSource {
    /// Native stores retain the caller-supplied binding compatibility adapter.
    fn binding(&self) -> Option<Binding> {
        None
    }
    fn rules_version(&self) -> u32 {
        super::LIGHT_RULES_VERSION
    }
    fn side(&self) -> u32;
    fn bounds(&self) -> WorldBounds;
    fn version(&self, pos: SectionPos) -> LightInputVersion;
    fn query(&self, pos: BlockPos) -> TerrainQuery;
}

impl LightSource for SectionStore {
    fn side(&self) -> u32 {
        self.side()
    }
    fn bounds(&self) -> WorldBounds {
        self.bounds()
    }
    fn version(&self, pos: SectionPos) -> LightInputVersion {
        LightInputVersion::Resident {
            revision: self.revision(pos),
            readable: super::readable(self, pos),
        }
    }
    fn query(&self, pos: BlockPos) -> TerrainQuery {
        self.query(pos)
    }
}
