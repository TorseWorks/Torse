//! Section lifecycle and authoritative terrain queries.
//!
//! The store owns every section that is present in memory and the state machine
//! that produced it. Queries never guess: a caller either receives an
//! authoritative state, is told that the data is not loaded, or is told that
//! the coordinate is outside the supported world. An opened storage face is
//! known empty space. Returning air for an unloaded region is the error this
//! API exists to prevent.
//!
//! Lifecycle transitions are deliberately explicit rather than automatic. A
//! caller that requests a section must also install data, finish loading, and
//! release its tickets, which keeps async work and eviction visible in the
//! call graph instead of hiding behind a background thread.

use std::collections::BTreeMap;

use sha2::{Digest as _, Sha256};
use torse_types::{BlockPos, Compatibility, Digest, Revision, SectionPos};

use crate::registry::BlockStateId;
use crate::section::{Section, SectionError};

/// Vertical extent of a world instance.
///
/// `min_y` is inclusive and `max_y` is exclusive, matching the range convention
/// used by Rust slices. Horizontal coordinates are unbounded for M1 because the
/// generator and the collision service derive their own limits from it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorldBounds {
    /// Lowest supported block layer.
    pub min_y: i64,
    /// First unsupported block layer above the world.
    pub max_y: i64,
}

impl WorldBounds {
    /// Build a vertical range and reject empty or absurd extents.
    pub fn new(min_y: i64, max_y: i64) -> Result<Self, StoreError> {
        if min_y >= max_y {
            return Err(StoreError::InvalidBounds { min_y, max_y });
        }
        Ok(Self { min_y, max_y })
    }

    /// Return whether a block layer belongs to this world.
    pub const fn contains_y(&self, y: i64) -> bool {
        y >= self.min_y && y < self.max_y
    }
}

/// Which storage faces are known empty instead of a solid boundary.
///
/// The default preserves movement above the build ceiling and blocks the lower
/// face. Opening a face does not make an unloaded section inside the store empty.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExteriorPolicy {
    /// Space below [`WorldBounds::min_y`] is empty.
    pub below: bool,
    /// Space at and above [`WorldBounds::max_y`] is empty.
    pub above: bool,
}

impl Default for ExteriorPolicy {
    fn default() -> Self {
        Self {
            below: false,
            above: true,
        }
    }
}

/// Mix a non-default exterior policy into the authority compatibility identity.
///
/// The default policy leaves `compatibility` unchanged, so existing worlds stay
/// compatible. A different policy produces a different rules digest.
pub fn bind_exterior(mut compatibility: Compatibility, policy: ExteriorPolicy) -> Compatibility {
    if policy == ExteriorPolicy::default() {
        return compatibility;
    }
    let mut hash = Sha256::new();
    hash.update(b"torse/exterior-policy/v1\0");
    hash.update(compatibility.rules_digest.0);
    hash.update([u8::from(policy.below), u8::from(policy.above)]);
    compatibility.rules_digest = Digest(hash.finalize().into());
    compatibility
}

/// Result of a terrain query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerrainQuery {
    /// The authoritative state of a loaded cell.
    Ready(BlockStateId),
    /// The section exists conceptually but has no authoritative data in memory.
    NotLoaded,
    /// The coordinate is outside the supported world extent.
    OutOfBounds,
    /// Known empty space outside a storage face the host opened.
    ///
    /// This is not stored terrain and not an unloaded section.
    Exterior,
}

impl TerrainQuery {
    /// Return the state if the cell is loaded.
    pub const fn ready(self) -> Option<BlockStateId> {
        match self {
            Self::Ready(state) => Some(state),
            Self::NotLoaded | Self::OutOfBounds | Self::Exterior => None,
        }
    }
}

/// Lifecycle phase of one section.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SectionPhase {
    /// A load has been requested but no data is present.
    Requested,
    /// Data is being installed or generated.
    Loading,
    /// Authoritative data is readable but the section holds no simulation ticket.
    Ready,
    /// Authoritative data is readable and at least one ticket is held.
    Active,
}

/// Why a caller holds a section in memory.
///
/// Ticket classes are separate because their quotas and eviction rules differ:
/// a player ticket should outrank a background prefetch, and a transaction ticket
/// must keep a cross-section commit from losing its target mid-flight.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TicketKind {
    /// A player or camera keeps the section visible.
    Player,
    /// Generation, lighting, or meshing work.
    Background,
    /// A commit that reads or writes the section.
    Transaction,
    /// Replication to a connected client.
    Network,
}

impl TicketKind {
    /// All ticket classes in a stable order.
    pub const ALL: [Self; 4] = [
        Self::Player,
        Self::Background,
        Self::Transaction,
        Self::Network,
    ];

    /// Position of this class in the per-section ticket counters.
    pub const fn index(self) -> usize {
        match self {
            Self::Player => 0,
            Self::Background => 1,
            Self::Transaction => 2,
            Self::Network => 3,
        }
    }
}

/// Maximum tickets of one class per section.
pub const TICKET_QUOTA_PER_KIND: u16 = 8;

/// Proof that a section is held for a stated purpose.
///
/// The handle is a value, not a borrow, so callers can hold tickets across
/// function boundaries without borrowing the whole store. Releasing is explicit
/// and checked, which makes double release a reported error instead of an
/// underflow.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TicketHandle {
    pos: SectionPos,
    kind: TicketKind,
}

impl TicketHandle {
    /// Section kept alive by this ticket.
    pub const fn section(self) -> SectionPos {
        self.pos
    }

    /// Purpose recorded for this ticket.
    pub const fn kind(self) -> TicketKind {
        self.kind
    }
}

#[derive(Debug)]
struct Entry {
    section: Section,
    phase: SectionPhase,
    tickets: [u16; 4],
}

impl Entry {
    const fn ticket_total(&self) -> u32 {
        let mut total = 0u32;
        let mut index = 0usize;
        while index < self.tickets.len() {
            total += self.tickets[index] as u32;
            index += 1;
        }
        total
    }
}

/// In-memory terrain keyed by section coordinate.
#[derive(Debug)]
pub struct SectionStore {
    side: u32,
    bounds: WorldBounds,
    exterior: ExteriorPolicy,
    sections: BTreeMap<SectionPos, Entry>,
    retired: BTreeMap<SectionPos, Revision>,
    retired_floor: Option<Revision>,
}

const RETIRED_REVISION_LIMIT: usize = 1024;

impl SectionStore {
    /// Create an empty store.
    pub fn new(side: u32, bounds: WorldBounds) -> Result<Self, SectionError> {
        let _ = Section::empty(side)?;
        Ok(Self {
            side,
            bounds,
            exterior: ExteriorPolicy::default(),
            sections: BTreeMap::new(),
            retired: BTreeMap::new(),
            retired_floor: None,
        })
    }

    /// Section side length in cells.
    pub const fn side(&self) -> u32 {
        self.side
    }

    /// Supported vertical extent.
    pub const fn bounds(&self) -> WorldBounds {
        self.bounds
    }

    /// How space outside the stored height is reported.
    pub const fn exterior(&self) -> ExteriorPolicy {
        self.exterior
    }

    /// Replace the exterior policy. Hosts should do this through the authority
    /// so the compatibility identity changes with it.
    pub fn set_exterior(&mut self, policy: ExteriorPolicy) {
        self.exterior = policy;
    }

    /// Current phase of a section, or `None` when it is absent.
    pub fn phase(&self, pos: SectionPos) -> Option<SectionPhase> {
        self.sections.get(&pos).map(|entry| entry.phase)
    }

    /// Record a load request without providing data yet.
    pub fn request(&mut self, pos: SectionPos) -> Result<SectionPhase, StoreError> {
        match self.sections.get_mut(&pos) {
            None => {
                let section = Section::empty(self.side)?;
                self.sections.insert(
                    pos,
                    Entry {
                        section,
                        phase: SectionPhase::Requested,
                        tickets: [0; 4],
                    },
                );
                Ok(SectionPhase::Requested)
            }
            Some(entry) => match entry.phase {
                SectionPhase::Requested
                | SectionPhase::Loading
                | SectionPhase::Ready
                | SectionPhase::Active => Ok(entry.phase),
            },
        }
    }

    /// Conservative revision watermark for a nonresident section.
    /// Compaction may return a watermark from another retired coordinate.
    pub fn retired_revision(&self, pos: SectionPos) -> Option<Revision> {
        match (self.retired.get(&pos).copied(), self.retired_floor) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    fn retire(&mut self, pos: SectionPos, revision: Revision) {
        self.retired.insert(pos, revision);
        if self.retired.len() >= RETIRED_REVISION_LIMIT {
            // A store-wide watermark conservatively invalidates cold-load
            // observations without retaining every coordinate ever visited.
            // New installations resume above it, so compaction cannot reuse
            // a revision previously observed at a forgotten coordinate.
            self.retired_floor = self
                .retired
                .values()
                .copied()
                .chain(self.retired_floor)
                .max();
            self.retired.clear();
        }
    }

    pub fn validate_install_revision(
        &self,
        pos: SectionPos,
        section: &Section,
    ) -> Result<(), StoreError> {
        if let Some(old) = self.retired_revision(pos) {
            section
                .revision()
                .0
                .max(old.0)
                .checked_add(1)
                .ok_or(SectionError::RevisionExhausted)?;
        }
        Ok(())
    }

    pub fn install(&mut self, pos: SectionPos, mut section: Section) -> Result<(), StoreError> {
        if section.side() != self.side {
            return Err(StoreError::SectionSideMismatch {
                expected: self.side,
                actual: section.side(),
            });
        }
        if let Some(revision) = self.retired_revision(pos) {
            section.resume_revision(revision)?;
        }
        let entry = self
            .sections
            .get_mut(&pos)
            .ok_or(StoreError::UnknownSection { pos })?;
        match entry.phase {
            SectionPhase::Requested | SectionPhase::Loading => {
                entry.section = section;
                entry.phase = SectionPhase::Loading;
                Ok(())
            }
            phase => Err(StoreError::WrongPhase {
                pos,
                phase,
                expected: "requested or loading",
            }),
        }
    }

    /// Mark installed data as readable.
    pub fn finish_load(&mut self, pos: SectionPos) -> Result<(), StoreError> {
        let entry = self
            .sections
            .get_mut(&pos)
            .ok_or(StoreError::UnknownSection { pos })?;
        match entry.phase {
            SectionPhase::Loading => {
                entry.phase = SectionPhase::Ready;
                Ok(())
            }
            phase => Err(StoreError::WrongPhase {
                pos,
                phase,
                expected: "loading",
            }),
        }
    }

    /// Take a ticket that prevents eviction.
    ///
    /// A ready section becomes active while at least one ticket is held.
    pub fn acquire_ticket(
        &mut self,
        pos: SectionPos,
        kind: TicketKind,
    ) -> Result<TicketHandle, StoreError> {
        let entry = self
            .sections
            .get_mut(&pos)
            .ok_or(StoreError::UnknownSection { pos })?;
        match entry.phase {
            SectionPhase::Ready | SectionPhase::Active => {}
            phase => {
                return Err(StoreError::WrongPhase {
                    pos,
                    phase,
                    expected: "ready or active",
                });
            }
        }
        let counter = &mut entry.tickets[kind.index()];
        if *counter >= TICKET_QUOTA_PER_KIND {
            return Err(StoreError::TicketQuota {
                pos,
                kind,
                quota: TICKET_QUOTA_PER_KIND,
            });
        }
        *counter += 1;
        entry.phase = SectionPhase::Active;
        Ok(TicketHandle { pos, kind })
    }

    /// Release a ticket obtained from [`Self::acquire_ticket`].
    pub fn release_ticket(&mut self, handle: TicketHandle) -> Result<(), StoreError> {
        let entry = self
            .sections
            .get_mut(&handle.pos)
            .ok_or(StoreError::TicketNotHeld { handle })?;
        let counter = &mut entry.tickets[handle.kind.index()];
        if *counter == 0 {
            return Err(StoreError::TicketNotHeld { handle });
        }
        *counter -= 1;
        if entry.ticket_total() == 0 && entry.phase == SectionPhase::Active {
            entry.phase = SectionPhase::Ready;
        }
        Ok(())
    }

    /// Read an authoritative cell.
    pub fn query(&self, pos: BlockPos) -> TerrainQuery {
        if !self.bounds.contains_y(pos.y) {
            let opened = (pos.y < self.bounds.min_y && self.exterior.below)
                || (pos.y >= self.bounds.max_y && self.exterior.above);
            return if opened {
                TerrainQuery::Exterior
            } else {
                TerrainQuery::OutOfBounds
            };
        }
        let (section_pos, local) = match pos.to_section(self.side) {
            Ok(converted) => converted,
            Err(_) => return TerrainQuery::OutOfBounds,
        };
        let entry = match self.sections.get(&section_pos) {
            Some(entry) => entry,
            None => return TerrainQuery::NotLoaded,
        };
        match entry.phase {
            SectionPhase::Ready | SectionPhase::Active => match entry.section.get(local) {
                Ok(state) => TerrainQuery::Ready(state),
                Err(_) => TerrainQuery::OutOfBounds,
            },
            SectionPhase::Requested | SectionPhase::Loading => TerrainQuery::NotLoaded,
        }
    }

    /// Write one authoritative cell.
    ///
    /// Only readable sections accept edits. Loading or absent data returns an
    /// error so a caller cannot turn a missing section into air by writing.
    pub fn edit(&mut self, pos: BlockPos, state: BlockStateId) -> Result<Revision, StoreError> {
        if !self.bounds.contains_y(pos.y) {
            return Err(StoreError::OutOfBounds { pos });
        }
        let (section_pos, local) = pos
            .to_section(self.side)
            .map_err(|_| StoreError::OutOfBounds { pos })?;
        let entry = match self.sections.get_mut(&section_pos) {
            Some(entry) => entry,
            None => return Err(StoreError::UnknownSection { pos: section_pos }),
        };
        match entry.phase {
            SectionPhase::Ready | SectionPhase::Active => {}
            phase => {
                return Err(StoreError::NotEditable {
                    pos: section_pos,
                    phase,
                });
            }
        }
        entry.section.set(local, state)?;
        Ok(entry.section.revision())
    }

    /// Check a group of edits without writing anything.
    ///
    /// Returns the touched sections in coordinate order. The coordinator uses
    /// this at prepare time so a published change cannot fail on a missing
    /// section: the same checks have already passed.
    pub fn validate_edits(&self, edits: &[BlockEdit]) -> Result<Vec<SectionPos>, StoreError> {
        let mut touched: Vec<SectionPos> = Vec::new();
        for edit in edits {
            if !self.bounds.contains_y(edit.pos.y) {
                return Err(StoreError::OutOfBounds { pos: edit.pos });
            }
            let (section_pos, _) = edit
                .pos
                .to_section(self.side)
                .map_err(|_| StoreError::OutOfBounds { pos: edit.pos })?;
            let entry = self
                .sections
                .get(&section_pos)
                .ok_or(StoreError::NotLoaded { pos: section_pos })?;
            match entry.phase {
                SectionPhase::Ready | SectionPhase::Active => {}
                phase => {
                    return Err(StoreError::NotEditable {
                        pos: section_pos,
                        phase,
                    });
                }
            }
            if !touched.contains(&section_pos) {
                touched.push(section_pos);
            }
        }
        touched.sort();
        Ok(touched)
    }

    /// Apply a group of cell edits as one all-or-nothing group.
    ///
    /// Every target is validated before the first write, so a missing section or
    /// an out-of-range coordinate cannot leave half a transaction applied. The
    /// call is not a transaction in the simulation sense: it neither checks
    /// preconditions nor assigns a commit sequence. The coordinator does that
    /// before calling here.
    pub fn apply_edits(
        &mut self,
        edits: &[BlockEdit],
    ) -> Result<Vec<(SectionPos, Revision)>, StoreError> {
        let touched = self.validate_edits(edits)?;

        for edit in edits {
            let (section_pos, local) = edit
                .pos
                .to_section(self.side)
                .map_err(|_| StoreError::OutOfBounds { pos: edit.pos })?;
            let entry = self
                .sections
                .get_mut(&section_pos)
                .ok_or(StoreError::NotLoaded { pos: section_pos })?;
            entry.section.set(local, edit.state)?;
        }

        let mut revisions = Vec::with_capacity(touched.len());
        for section_pos in touched {
            let entry = self
                .sections
                .get(&section_pos)
                .ok_or(StoreError::NotLoaded { pos: section_pos })?;
            revisions.push((section_pos, entry.section.revision()));
        }
        Ok(revisions)
    }

    /// Place a block only when the target cell is currently empty.
    pub fn place(&mut self, pos: BlockPos, state: BlockStateId) -> Result<Revision, StoreError> {
        if state.is_empty() {
            return Err(StoreError::InvalidPlacement { pos });
        }
        match self.query(pos) {
            TerrainQuery::Ready(current) if current.is_empty() => self.edit(pos, state),
            TerrainQuery::Ready(_) => Err(StoreError::Occupied { pos }),
            TerrainQuery::NotLoaded => Err(StoreError::NotLoaded {
                pos: self.section_of(pos)?,
            }),
            TerrainQuery::OutOfBounds | TerrainQuery::Exterior => {
                Err(StoreError::OutOfBounds { pos })
            }
        }
    }

    /// Remove the block in a cell only when the cell is currently occupied.
    pub fn break_block(&mut self, pos: BlockPos) -> Result<(BlockStateId, Revision), StoreError> {
        match self.query(pos) {
            TerrainQuery::Ready(current) if !current.is_empty() => {
                let revision = self.edit(pos, BlockStateId::EMPTY)?;
                Ok((current, revision))
            }
            TerrainQuery::Ready(_) => Err(StoreError::EmptyCell { pos }),
            TerrainQuery::NotLoaded => Err(StoreError::NotLoaded {
                pos: self.section_of(pos)?,
            }),
            TerrainQuery::OutOfBounds | TerrainQuery::Exterior => {
                Err(StoreError::OutOfBounds { pos })
            }
        }
    }

    fn section_of(&self, pos: BlockPos) -> Result<SectionPos, StoreError> {
        pos.to_section(self.side)
            .map(|(section, _)| section)
            .map_err(|_| StoreError::OutOfBounds { pos })
    }

    /// Terrain revision of a section, if it has data.
    pub fn revision(&self, pos: SectionPos) -> Option<Revision> {
        self.sections
            .get(&pos)
            .map(|entry| entry.section.revision())
    }

    /// Borrow a section's data.
    pub fn section(&self, pos: SectionPos) -> Option<&Section> {
        self.sections.get(&pos).map(|entry| &entry.section)
    }

    /// Every tracked section coordinate, ascending.
    ///
    /// The iterator includes sections that are still loading; callers that need
    /// readable data must check the phase.
    pub fn positions(&self) -> impl Iterator<Item = SectionPos> + '_ {
        self.sections.keys().copied()
    }

    /// Remove a section that no caller is using.
    pub fn unload(&mut self, pos: SectionPos) -> Result<(), StoreError> {
        let entry = self
            .sections
            .get(&pos)
            .ok_or(StoreError::UnknownSection { pos })?;
        if entry.ticket_total() > 0 {
            return Err(StoreError::TicketsHeld {
                pos,
                tickets: entry.ticket_total(),
            });
        }
        match entry.phase {
            SectionPhase::Ready => {
                self.retire(pos, entry.section.revision());
                self.sections.remove(&pos);
                Ok(())
            }
            phase => Err(StoreError::WrongPhase {
                pos,
                phase,
                expected: "ready",
            }),
        }
    }

    /// Immediately forget a section regardless of phase, for shutdown or tests.
    ///
    /// Out-of-order teardown is explicit: normal operation goes through
    /// `release_ticket` and `unload`, which enforce the lifecycle rules.
    pub fn discard(&mut self, pos: SectionPos) -> Option<Section> {
        self.sections.remove(&pos).map(|entry| {
            self.retire(pos, entry.section.revision());
            entry.section
        })
    }

    /// Number of sections tracked by the store, in any phase.
    pub fn len(&self) -> usize {
        self.sections.len()
    }

    /// Return whether no section is tracked.
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// Memory accounting used by the acceptance checks.
    pub fn stats(&self) -> StoreStats {
        let mut stats = StoreStats::default();
        for entry in self.sections.values() {
            match entry.phase {
                SectionPhase::Ready | SectionPhase::Active => {
                    stats.loaded_sections += 1;
                    stats.cell_bytes += entry.section.allocated_bytes();
                    match entry.section.storage() {
                        crate::SectionStorage::Uniform(_) => stats.uniform_sections += 1,
                        crate::SectionStorage::Dense(cells) => stats.dense_cells += cells.len(),
                        crate::SectionStorage::Palette(_) => stats.palette_sections += 1,
                    }
                }
                SectionPhase::Requested | SectionPhase::Loading => stats.pending_sections += 1,
            }
            stats.tickets += entry.ticket_total();
        }
        stats
    }

    /// Report resident section structs and cell buffers, including loading data.
    /// Map nodes, tickets and retired-revision metadata are owned separately.
    pub fn memory_resident(&self) -> Option<torse_types::MemoryResident> {
        let bytes = self.sections.values().fold(0u64, |total, entry| {
            total.saturating_add((size_of::<Section>() + entry.section.allocated_bytes()) as u64)
        });
        (bytes > 0).then_some(torse_types::MemoryResident {
            class: torse_types::MemoryClass::World,
            bytes,
            rank: torse_types::EvictionRank::Pinned,
        })
    }
    /// Cell buffers plus conservative map-entry reservations, including retired revisions.
    pub fn accounted_bytes(&self) -> u64 {
        self.memory_resident()
            .map_or(0, |resident| resident.bytes)
            .saturating_add((self.sections.len() + self.retired.len()) as u64 * 4096)
            .saturating_add(size_of::<Self>() as u64)
    }
}

/// Snapshot of store occupancy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoreStats {
    /// Sections with readable data.
    pub loaded_sections: usize,
    /// Sections waiting for data.
    pub pending_sections: usize,
    /// Loaded sections that still use the uniform representation.
    pub uniform_sections: usize,
    /// Allocated cell identifiers across dense sections.
    pub dense_cells: usize,
    /// Loaded sections using packed local palette indices.
    pub palette_sections: usize,
    /// Heap capacity of readable sections, including palette metadata.
    pub cell_bytes: usize,
    /// Tickets currently held by callers.
    pub tickets: u32,
}

/// Failure produced by the section store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreError {
    /// The vertical range is empty or inverted.
    InvalidBounds {
        /// Requested minimum layer.
        min_y: i64,
        /// Requested maximum layer.
        max_y: i64,
    },
    /// The store has no record of the section.
    UnknownSection {
        /// Missing section.
        pos: SectionPos,
    },
    /// A transition was attempted from an incompatible phase.
    WrongPhase {
        /// Section being transitioned.
        pos: SectionPos,
        /// Observed phase.
        phase: SectionPhase,
        /// Required phase description.
        expected: &'static str,
    },
    /// Installed data used a different side length than the store.
    SectionSideMismatch {
        /// Store side length.
        expected: u32,
        /// Provided section side length.
        actual: u32,
    },
    /// The coordinate is outside the supported vertical extent.
    OutOfBounds {
        /// Rejected coordinate.
        pos: BlockPos,
    },
    /// The section has no readable data, so it cannot accept edits.
    NotEditable {
        /// Section that rejected the edit.
        pos: SectionPos,
        /// Observed phase.
        phase: SectionPhase,
    },
    /// One ticket class reached its per-section quota.
    TicketQuota {
        /// Section at the quota.
        pos: SectionPos,
        /// Exhausted class.
        kind: TicketKind,
        /// Configured quota.
        quota: u16,
    },
    /// A ticket was released that the store did not issue, or was released twice.
    TicketNotHeld {
        /// Rejected handle.
        handle: TicketHandle,
    },
    /// Unloading was attempted while tickets are still held.
    TicketsHeld {
        /// Section that is still in use.
        pos: SectionPos,
        /// Number of outstanding tickets.
        tickets: u32,
    },
    /// The section has no authoritative data.
    NotLoaded {
        /// Section that is absent or still loading.
        pos: SectionPos,
    },
    /// A placement target already contains a block.
    Occupied {
        /// Target coordinate.
        pos: BlockPos,
    },
    /// A break target contains no block.
    EmptyCell {
        /// Target coordinate.
        pos: BlockPos,
    },
    /// A placement requested the empty sentinel as the new state.
    InvalidPlacement {
        /// Target coordinate.
        pos: BlockPos,
    },
    /// Section storage rejected the operation.
    Section(SectionError),
}

/// One authoritative cell edit, used by batch apply and by commit records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockEdit {
    /// Cell coordinate.
    pub pos: BlockPos,
    /// State to write.
    pub state: BlockStateId,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBounds { min_y, max_y } => {
                write!(f, "world bounds [{min_y}, {max_y}) are empty")
            }
            Self::UnknownSection { pos } => write!(
                f,
                "section [{}, {}, {}] is not tracked",
                pos.x, pos.y, pos.z
            ),
            Self::WrongPhase {
                pos,
                phase,
                expected,
            } => write!(
                f,
                "section [{}, {}, {}] is {phase:?} but {expected} is required",
                pos.x, pos.y, pos.z
            ),
            Self::SectionSideMismatch { expected, actual } => {
                write!(
                    f,
                    "section side {actual} does not match store side {expected}"
                )
            }
            Self::OutOfBounds { pos } => write!(
                f,
                "block [{}, {}, {}] is outside the world",
                pos.x, pos.y, pos.z
            ),
            Self::NotEditable { pos, phase } => write!(
                f,
                "section [{}, {}, {}] is {phase:?} and cannot be edited",
                pos.x, pos.y, pos.z
            ),
            Self::TicketQuota { pos, kind, quota } => write!(
                f,
                "section [{}, {}, {}] already holds {quota} {kind:?} tickets",
                pos.x, pos.y, pos.z
            ),
            Self::TicketNotHeld { handle } => write!(
                f,
                "no {:?} ticket is held for section [{}, {}, {}]",
                handle.kind, handle.pos.x, handle.pos.y, handle.pos.z
            ),
            Self::TicketsHeld { pos, tickets } => write!(
                f,
                "section [{}, {}, {}] still holds {tickets} tickets",
                pos.x, pos.y, pos.z
            ),
            Self::NotLoaded { pos } => write!(
                f,
                "section [{}, {}, {}] has no authoritative data",
                pos.x, pos.y, pos.z
            ),
            Self::Occupied { pos } => write!(
                f,
                "block [{}, {}, {}] is already occupied",
                pos.x, pos.y, pos.z
            ),
            Self::EmptyCell { pos } => {
                write!(
                    f,
                    "block [{}, {}, {}] is already empty",
                    pos.x, pos.y, pos.z
                )
            }
            Self::InvalidPlacement { pos } => write!(
                f,
                "placement at [{}, {}, {}] requested the empty sentinel",
                pos.x, pos.y, pos.z
            ),
            Self::Section(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<SectionError> for StoreError {
    fn from(error: SectionError) -> Self {
        Self::Section(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::section::Section;
    use torse_types::LocalPos;

    fn store() -> SectionStore {
        let bounds = WorldBounds::new(0, 256).expect("bounds are valid");
        SectionStore::new(16, bounds).expect("side is valid")
    }

    fn ready(store: &mut SectionStore, section: SectionPos, fill: BlockStateId) {
        store.request(section).expect("request is accepted");
        store
            .install(section, Section::new(16, fill).expect("side is valid"))
            .expect("install is accepted");
        store.finish_load(section).expect("load finishes");
    }

    #[test]
    fn absent_section_is_not_loaded_not_air() {
        let store = store();
        assert_eq!(
            store.query(BlockPos::new(0, 10, 0)),
            TerrainQuery::NotLoaded
        );
        assert_eq!(
            store.query(BlockPos::new(0, 300, 0)),
            TerrainQuery::Exterior
        );
        assert_eq!(store.phase(SectionPos::ZERO), None);
    }

    #[test]
    fn loading_section_still_reports_not_loaded() {
        let mut store = store();
        store
            .request(SectionPos::ZERO)
            .expect("request is accepted");
        assert_eq!(store.query(BlockPos::ZERO), TerrainQuery::NotLoaded);
        store
            .install(SectionPos::ZERO, Section::empty(16).expect("side is valid"))
            .expect("install is accepted");
        assert_eq!(store.query(BlockPos::ZERO), TerrainQuery::NotLoaded);
        store.finish_load(SectionPos::ZERO).expect("load finishes");
        assert_eq!(
            store.query(BlockPos::ZERO),
            TerrainQuery::Ready(BlockStateId::EMPTY)
        );
    }

    #[test]
    fn negative_coordinates_land_in_their_own_section() {
        let mut store = store();
        let section = SectionPos::new(-1, 0, -1);
        ready(&mut store, section, BlockStateId(4));
        assert_eq!(
            store.query(BlockPos::new(-1, 0, -1)),
            TerrainQuery::Ready(BlockStateId(4))
        );
        assert_eq!(store.query(BlockPos::new(0, 0, 0)), TerrainQuery::NotLoaded);
    }

    #[test]
    fn edit_requires_readable_section_and_bumps_revision() {
        let mut store = store();
        assert!(matches!(
            store.edit(BlockPos::ZERO, BlockStateId(1)),
            Err(StoreError::UnknownSection { .. })
        ));
        ready(&mut store, SectionPos::ZERO, BlockStateId::EMPTY);
        let before = store
            .revision(SectionPos::ZERO)
            .expect("section has a revision");
        let after = store
            .edit(BlockPos::ZERO, BlockStateId(1))
            .expect("edit is accepted");
        assert!(after > before);
        assert_eq!(
            store.query(BlockPos::ZERO),
            TerrainQuery::Ready(BlockStateId(1))
        );
    }

    #[test]
    fn tickets_block_eviction_and_phase_tracks_them() {
        let mut store = store();
        ready(&mut store, SectionPos::ZERO, BlockStateId(1));
        assert_eq!(store.phase(SectionPos::ZERO), Some(SectionPhase::Ready));
        let ticket = store
            .acquire_ticket(SectionPos::ZERO, TicketKind::Player)
            .expect("ticket is granted");
        assert_eq!(store.phase(SectionPos::ZERO), Some(SectionPhase::Active));
        assert!(matches!(
            store.unload(SectionPos::ZERO),
            Err(StoreError::TicketsHeld { .. })
        ));
        store.release_ticket(ticket).expect("ticket is released");
        assert_eq!(store.phase(SectionPos::ZERO), Some(SectionPhase::Ready));
        store.unload(SectionPos::ZERO).expect("unload is accepted");
        assert_eq!(store.phase(SectionPos::ZERO), None);
    }

    #[test]
    fn double_release_and_quota_are_reported() {
        let mut store = store();
        ready(&mut store, SectionPos::ZERO, BlockStateId(1));
        let ticket = store
            .acquire_ticket(SectionPos::ZERO, TicketKind::Background)
            .expect("ticket is granted");
        store.release_ticket(ticket).expect("ticket is released");
        assert!(matches!(
            store.release_ticket(ticket),
            Err(StoreError::TicketNotHeld { .. })
        ));

        let mut tickets = Vec::new();
        for _ in 0..TICKET_QUOTA_PER_KIND {
            tickets.push(
                store
                    .acquire_ticket(SectionPos::ZERO, TicketKind::Network)
                    .expect("quota allows the configured number"),
            );
        }
        assert!(matches!(
            store.acquire_ticket(SectionPos::ZERO, TicketKind::Network),
            Err(StoreError::TicketQuota { .. })
        ));
        for ticket in tickets {
            store.release_ticket(ticket).expect("ticket is released");
        }
    }

    #[test]
    fn stats_separate_uniform_from_dense_sections() {
        let mut store = store();
        ready(&mut store, SectionPos::ZERO, BlockStateId(1));
        ready(&mut store, SectionPos::new(1, 0, 0), BlockStateId::EMPTY);
        let stats = store.stats();
        assert_eq!(stats.loaded_sections, 2);
        assert_eq!(stats.uniform_sections, 2);
        assert_eq!(stats.dense_cells, 0);
        store
            .edit(BlockPos::new(1, 2, 3), BlockStateId(2))
            .expect("edit makes the section non-uniform");
        let stats = store.stats();
        assert_eq!(stats.uniform_sections, 1);
        assert_eq!(stats.dense_cells, 0);
        assert_eq!(stats.palette_sections, 1);
        assert_eq!(stats.cell_bytes, 524 + size_of::<crate::PaletteCells>());
    }

    #[test]
    fn world_memory_report_tracks_packed_buffers_and_loading_sections() {
        let mut store = store();
        ready(&mut store, SectionPos::ZERO, BlockStateId::EMPTY);
        let mut ledger = torse_types::MemoryLedger::new(torse_types::MemoryBudget::new(1_000_000));
        let original = ledger.charge(store.memory_resident().unwrap()).unwrap();
        store.edit(BlockPos::new(1, 1, 1), BlockStateId(1)).unwrap();
        store.request(SectionPos::new(1, 0, 0)).unwrap();
        let report = store.memory_resident().unwrap();
        assert_eq!(
            report.bytes,
            (store.stats().cell_bytes + 2 * size_of::<Section>()) as u64
        );
        let replacement = ledger.exchange(&[original], &[report]).unwrap()[0];
        assert_eq!(ledger.usage(torse_types::MemoryClass::World), report.bytes);
        assert!(ledger.eviction_plan(report.bytes).charges.is_empty());
        ledger.release(replacement).unwrap();
    }

    #[test]
    fn section_side_mismatch_is_rejected() {
        let mut store = store();
        store
            .request(SectionPos::ZERO)
            .expect("request is accepted");
        let wrong = Section::empty(32).expect("side is valid");
        assert!(matches!(
            store.install(SectionPos::ZERO, wrong),
            Err(StoreError::SectionSideMismatch { .. })
        ));
    }

    #[test]
    fn local_query_helper_roundtrips() {
        // Guard against a section-local conversion regression at a negative
        // boundary, which is the classic chunk indexing failure.
        let (section, local) = torse_types::BlockPos::new(-1, 5, -17)
            .to_section(16)
            .expect("conversion succeeds");
        assert_eq!(section, SectionPos::new(-1, 0, -2));
        assert_eq!(local, LocalPos { x: 15, y: 5, z: 15 });
    }

    #[test]
    fn placement_requires_an_empty_target() {
        let mut store = store();
        ready(&mut store, SectionPos::ZERO, BlockStateId::EMPTY);
        store
            .place(BlockPos::new(1, 2, 3), BlockStateId(7))
            .expect("place succeeds");
        assert!(matches!(
            store.place(BlockPos::new(1, 2, 3), BlockStateId(8)),
            Err(StoreError::Occupied { .. })
        ));
        assert!(matches!(
            store.place(BlockPos::new(2, 2, 3), BlockStateId::EMPTY),
            Err(StoreError::InvalidPlacement { .. })
        ));
        assert_eq!(
            store.query(BlockPos::new(1, 2, 3)),
            TerrainQuery::Ready(BlockStateId(7)),
            "a failed placement must not change the cell"
        );
    }

    #[test]
    fn break_requires_an_occupied_cell() {
        let mut store = store();
        ready(&mut store, SectionPos::ZERO, BlockStateId::EMPTY);
        assert!(matches!(
            store.break_block(BlockPos::new(1, 2, 3)),
            Err(StoreError::EmptyCell { .. })
        ));
        store
            .edit(BlockPos::new(1, 2, 3), BlockStateId(4))
            .expect("edit succeeds");
        let (previous, _) = store
            .break_block(BlockPos::new(1, 2, 3))
            .expect("break succeeds");
        assert_eq!(previous, BlockStateId(4));
        assert_eq!(
            store.query(BlockPos::new(1, 2, 3)),
            TerrainQuery::Ready(BlockStateId::EMPTY)
        );
    }

    #[test]
    fn batch_apply_writes_nothing_when_any_target_is_missing() {
        let mut store = store();
        ready(&mut store, SectionPos::ZERO, BlockStateId::EMPTY);
        let edits = [
            BlockEdit {
                pos: BlockPos::new(1, 1, 1),
                state: BlockStateId(3),
            },
            // This one belongs to a section that was never loaded.
            BlockEdit {
                pos: BlockPos::new(40, 1, 1),
                state: BlockStateId(3),
            },
        ];
        assert!(matches!(
            store.apply_edits(&edits),
            Err(StoreError::NotLoaded { .. })
        ));
        assert_eq!(
            store.query(BlockPos::new(1, 1, 1)),
            TerrainQuery::Ready(BlockStateId::EMPTY),
            "validation must run before the first write"
        );
    }

    #[test]
    fn batch_apply_reports_touched_sections_once() {
        let mut store = store();
        ready(&mut store, SectionPos::ZERO, BlockStateId::EMPTY);
        let edits = [
            BlockEdit {
                pos: BlockPos::new(1, 1, 1),
                state: BlockStateId(3),
            },
            BlockEdit {
                pos: BlockPos::new(2, 1, 1),
                state: BlockStateId(4),
            },
            BlockEdit {
                pos: BlockPos::new(1, 1, 1),
                state: BlockStateId(5),
            },
        ];
        let touched = store.apply_edits(&edits).expect("edits are applied");
        assert_eq!(touched.len(), 1);
        assert_eq!(touched[0].0, SectionPos::ZERO);
        assert_eq!(
            store.query(BlockPos::new(1, 1, 1)),
            TerrainQuery::Ready(BlockStateId(5)),
            "later edits in the same batch win"
        );
    }

    #[test]
    fn opened_exterior_is_empty_and_unloaded_sections_stay_unloaded() {
        let mut store = store();
        assert_eq!(
            store.query(BlockPos::new(0, -1, 0)),
            TerrainQuery::OutOfBounds
        );
        store.set_exterior(ExteriorPolicy {
            below: true,
            above: false,
        });
        assert_eq!(store.query(BlockPos::new(0, -1, 0)), TerrainQuery::Exterior);
        assert_eq!(store.query(BlockPos::new(0, 0, 0)), TerrainQuery::NotLoaded);
        assert_eq!(
            store.query(BlockPos::new(0, 256, 0)),
            TerrainQuery::OutOfBounds
        );
        assert!(
            store
                .edit(BlockPos::new(0, -1, 0), BlockStateId(1))
                .is_err()
        );
        let base = torse_types::Compatibility {
            authority_schema: 1,
            rules_digest: torse_types::Digest([9; 32]),
            content_digest: torse_types::Digest([3; 32]),
        };
        assert_eq!(bind_exterior(base, ExteriorPolicy::default()), base);
        assert_ne!(
            bind_exterior(
                base,
                ExteriorPolicy {
                    below: true,
                    above: false,
                }
            ),
            base
        );
    }

    #[test]
    fn long_travel_bounds_retired_metadata_without_reusing_old_revisions() {
        let mut store = store();
        let origin = SectionPos::ZERO;
        store.request(origin).unwrap();
        store.install(origin, Section::empty(16).unwrap()).unwrap();
        store.finish_load(origin).unwrap();
        for n in 0..12 {
            store
                .edit(BlockPos::new(0, 0, 0), BlockStateId(1 + n % 2))
                .unwrap();
        }
        let old = store.revision(origin).unwrap();
        store.unload(origin).unwrap();
        for x in 1..=8192 {
            let pos = SectionPos::new(x, 0, 0);
            store.request(pos).unwrap();
            store.install(pos, Section::empty(16).unwrap()).unwrap();
            store.finish_load(pos).unwrap();
            store.unload(pos).unwrap();
            assert!(
                store.accounted_bytes() < 8 * 1024 * 1024,
                "retired world memory grew with travel at {x}"
            );
        }
        assert!(store.retired_revision(origin).unwrap() >= old);
        store.request(origin).unwrap();
        store.install(origin, Section::empty(16).unwrap()).unwrap();
        store.finish_load(origin).unwrap();
        assert!(store.revision(origin).unwrap() > old);
    }
}
