//! Bounded, validated native terrain shared by lighting consumers.
use super::{LightInputVersion, LightSource};
use sha2::{Digest as _, Sha256};
use std::{collections::BTreeMap, sync::Arc};
use torse_types::{Binding, BlockPos, Digest, Fault, SectionPos};
use torse_world::{
    BlockStateId, BlockStateRegistry, Section, SectionStore, SnapshotCells, SnapshotSection,
    StateEntry, TerrainQuery, WorldBounds,
};

/// Input storage is independent of the authority's residency state. Owners must
/// remove an entry when its backing content can no longer be verified.
pub struct LightTerrain {
    binding: Binding,
    registry_digest: Digest,
    side: u32,
    bounds: WorldBounds,
    pages: BTreeMap<SectionPos, Page>,
    max_sections: usize,
    max_bytes: usize,
    bytes: usize,
}
struct Page {
    digest: Digest,
    section: Arc<Section>,
    bytes: usize,
    verified: bool,
}

impl LightTerrain {
    pub fn new(
        binding: Binding,
        side: u32,
        bounds: WorldBounds,
        registry: &BlockStateRegistry,
        max_sections: usize,
        max_bytes: usize,
    ) -> Result<Self, Fault> {
        SectionStore::new(side, bounds).map_err(invalid)?;
        if max_sections == 0 || max_sections > 32768 || max_bytes == 0 {
            return Err(Fault::resource_limit("light terrain limits"));
        }
        Ok(Self {
            binding,
            registry_digest: registry.authority_digest(),
            side,
            bounds,
            pages: BTreeMap::new(),
            max_sections,
            max_bytes,
            bytes: 0,
        })
    }
    pub fn binding(&self) -> Binding {
        self.binding
    }
    pub fn allocated_bytes(&self) -> usize {
        self.bytes
    }
    pub fn len(&self) -> usize {
        self.pages.len()
    }
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// Accept trusted generated or resident cells. Validation and the content
    /// digest do not use allocation layout, storage revision or source type.
    /// Returns whether the logical input changed. Failure preserves the old page.
    pub fn insert(
        &mut self,
        pos: SectionPos,
        section: Section,
        registry: &BlockStateRegistry,
    ) -> Result<bool, Fault> {
        if registry.authority_digest() != self.registry_digest
            || section.side() != self.side
            || !self.intersects(pos)
        {
            return Err(Fault::invalid_data("incompatible light terrain"));
        }
        super::section_origin(pos, i64::from(self.side))?;
        let bytes = size_of::<Page>() + 128 + section.allocated_bytes();
        let previous = self.pages.get(&pos);
        let total = self
            .bytes
            .checked_sub(previous.map_or(0, |p| p.bytes))
            .and_then(|n| n.checked_add(bytes))
            .ok_or_else(|| Fault::resource_limit("light terrain bytes"))?;
        if total > self.max_bytes || (previous.is_none() && self.pages.len() == self.max_sections) {
            return Err(Fault::resource_limit("light terrain capacity"));
        }
        let mut hash = Sha256::new();
        hash.update(b"torse/light-terrain/v1\0");
        hash.update(self.registry_digest.0);
        hash.update(self.side.to_le_bytes());
        hash.update(self.bounds.min_y.to_le_bytes());
        hash.update(self.bounds.max_y.to_le_bytes());
        for id in section.values() {
            if !id.is_empty() && registry.light_transport(id).is_none() {
                return Err(Fault::invalid_data("unknown light terrain state"));
            }
            hash.update(id.0.to_le_bytes());
        }
        let digest = Digest(hash.finalize().into());
        if previous.is_some_and(|p| p.digest == digest) {
            self.pages
                .get_mut(&pos)
                .expect("existing light input")
                .verified = true;
            return Ok(false);
        }
        self.pages.insert(
            pos,
            Page {
                digest,
                section: Arc::new(section),
                bytes,
                verified: true,
            },
        );
        self.bytes = total;
        Ok(true)
    }

    /// Resolve the state dictionary of an already authenticated page. Page
    /// envelope/digest/binding verification is the caller's I/O responsibility;
    /// this boundary also validates every cell and rejects malformed dictionaries.
    pub fn insert_snapshot(
        &mut self,
        page: &SnapshotSection,
        states: &[StateEntry],
        registry: &BlockStateRegistry,
    ) -> Result<bool, Fault> {
        if states.len() > registry.state_count() as usize {
            return Err(Fault::resource_limit("light state dictionary"));
        }
        let mut mapping = BTreeMap::new();
        let mut previous = None;
        for entry in states {
            if entry.id.is_empty() || previous.is_some_and(|p| p >= entry.id) {
                return Err(Fault::invalid_data("invalid light state dictionary"));
            }
            let id = registry
                .state_of(&entry.block, &entry.values)
                .ok_or_else(|| Fault::invalid_data("unknown light page state"))?;
            mapping.insert(entry.id, id);
            previous = Some(entry.id);
        }
        let resolve = |id: BlockStateId| -> Result<BlockStateId, Fault> {
            if id.is_empty() {
                Ok(id)
            } else {
                mapping
                    .get(&id)
                    .copied()
                    .ok_or_else(|| Fault::invalid_data("undefined light page state"))
            }
        };
        let section = match &page.cells {
            SnapshotCells::Uniform(id) => {
                Section::new(self.side, resolve(*id)?).map_err(invalid)?
            }
            SnapshotCells::Dense(ids) => {
                if ids.len() != (self.side as usize).pow(3) {
                    return Err(Fault::invalid_data("light page cell count"));
                }
                Section::from_cells(
                    self.side,
                    ids.iter()
                        .map(|id| resolve(*id))
                        .collect::<Result<_, _>>()?,
                )
                .map_err(invalid)?
            }
        };
        self.insert(page.pos, section, registry)
    }
    /// Missing/corrupt input becomes Unknown, never an empty section.
    pub fn remove(&mut self, pos: SectionPos) -> bool {
        if let Some(page) = self.pages.remove(&pos) {
            self.bytes -= page.bytes;
            true
        } else {
            false
        }
    }
    /// Hide cached cells until a replacement source has been verified.
    pub fn suspend(&mut self, pos: SectionPos) {
        if let Some(page) = self.pages.get_mut(&pos) {
            page.verified = false;
        }
    }
    pub fn positions(&self) -> impl Iterator<Item = SectionPos> + '_ {
        self.pages.keys().copied()
    }
    fn intersects(&self, pos: SectionPos) -> bool {
        let base = i128::from(pos.y) * i128::from(self.side);
        base < i128::from(self.bounds.max_y)
            && base + i128::from(self.side) > i128::from(self.bounds.min_y)
    }
}
impl LightSource for LightTerrain {
    fn binding(&self) -> Option<Binding> {
        Some(self.binding)
    }
    fn side(&self) -> u32 {
        self.side
    }
    fn bounds(&self) -> WorldBounds {
        self.bounds
    }
    fn version(&self, pos: SectionPos) -> LightInputVersion {
        if !self.intersects(pos) {
            LightInputVersion::Boundary
        } else {
            self.pages
                .get(&pos)
                .filter(|p| p.verified)
                .map_or(LightInputVersion::Unknown, |p| {
                    LightInputVersion::Content(p.digest)
                })
        }
    }
    fn query(&self, pos: BlockPos) -> TerrainQuery {
        if !self.bounds.contains_y(pos.y) {
            return TerrainQuery::OutOfBounds;
        }
        let Ok((section, local)) = pos.to_section(self.side) else {
            return TerrainQuery::OutOfBounds;
        };
        self.pages
            .get(&section)
            .filter(|p| p.verified)
            .map_or(TerrainQuery::NotLoaded, |p| {
                p.section
                    .get(local)
                    .map_or(TerrainQuery::OutOfBounds, TerrainQuery::Ready)
            })
    }
}
fn invalid(e: impl std::fmt::Display) -> Fault {
    Fault::invalid_data(e.to_string())
}
