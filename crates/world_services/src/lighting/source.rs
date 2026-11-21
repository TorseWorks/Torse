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
        if let Some(section) = self.readable_section(pos) {
            return LightInputVersion::Resident {
                revision: Some(section.revision()),
                readable: true,
            };
        }
        LightInputVersion::Resident {
            revision: self.revision(pos),
            readable: false,
        }
    }
    fn query(&self, pos: BlockPos) -> TerrainQuery {
        self.query(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torse_world::{Section, SectionPhase, TicketKind};

    #[test]
    fn resident_versions_preserve_unfinished_data_and_lifecycle_identity() {
        let mut store = SectionStore::new(16, WorldBounds::new(0, 256).unwrap()).unwrap();
        let pos = SectionPos::ZERO;
        let check = |store: &SectionStore| {
            assert_eq!(
                store.version(pos),
                LightInputVersion::Resident {
                    revision: store.revision(pos),
                    readable: matches!(
                        store.phase(pos),
                        Some(SectionPhase::Ready | SectionPhase::Active)
                    ),
                }
            );
        };
        check(&store);
        for _ in 0..2 {
            store.request(pos).unwrap();
            check(&store);
            store.install(pos, Section::empty(16).unwrap()).unwrap();
            check(&store);
            store.finish_load(pos).unwrap();
            check(&store);
            let ticket = store.acquire_ticket(pos, TicketKind::Background).unwrap();
            check(&store);
            store.release_ticket(ticket).unwrap();
            check(&store);
            store.unload(pos).unwrap();
            check(&store);
        }
    }
}
