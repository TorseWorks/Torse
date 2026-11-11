//! Section storage for authoritative block cells.
//!
//! Uniform sections own no cell buffer. Low-diversity sections pack local
//! palette indices; high-diversity sections retain direct state identifiers.
//! A dense section counts its states and shrinks on the write that makes a
//! palette or a single value smaller. Layout changes do not change cell
//! queries or exchange formats.
//!
//! Linear indexing follows the architecture note: `x + side * (z + side * y)`.
//! The encoding is internal, but fixing it here keeps tests and future
//! serialization from inventing a second order.

use torse_types::{LocalPos, Revision, SpatialError};

use crate::registry::BlockStateId;

/// Section side used by the M1 prototype.
///
/// The public API takes the side explicitly so the 16-versus-32 experiment can
/// still change the default without changing types.
pub const DEFAULT_SECTION_SIDE: u32 = 32;

/// Largest supported side that keeps `side^3` inside `u32` and under a million
/// cells, which bounds any single allocation made by storage.
pub const MAX_SECTION_SIDE: u32 = 64;

/// Direct identifiers for a high-diversity section, plus private state counts.
///
/// The counts are not part of the cell query. They exist so a write that
/// removes the last cell of a state can shrink without scanning the section.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DenseCells {
    cells: Vec<BlockStateId>,
    states: Vec<BlockStateId>,
    counts: Vec<u32>,
}

impl DenseCells {
    fn from_cells(cells: Vec<BlockStateId>) -> Self {
        let mut order = cells.clone();
        order.sort_unstable();
        let mut states = Vec::new();
        let mut counts = Vec::new();
        for cell in order {
            if states.last() == Some(&cell) {
                *counts
                    .last_mut()
                    .expect("a state is recorded with its count") += 1;
            } else {
                states.push(cell);
                counts.push(1);
            }
        }
        states.shrink_to_fit();
        counts.shrink_to_fit();
        Self {
            cells,
            states,
            counts,
        }
    }

    fn allocated_bytes(&self) -> usize {
        self.cells.capacity() * size_of::<BlockStateId>()
            + self.states.capacity() * size_of::<BlockStateId>()
            + self.counts.capacity() * size_of::<u32>()
    }

    /// Update one cell. `Some` replaces the whole section layout.
    fn write(&mut self, index: usize, state: BlockStateId) -> Option<SectionStorage> {
        let previous = self.cells[index];
        let before = self.states.len();
        let old = self
            .states
            .binary_search(&previous)
            .expect("dense census contains the previous state");
        self.counts[old] -= 1;
        if self.counts[old] == 0 {
            self.states.remove(old);
            self.counts.remove(old);
        }
        match self.states.binary_search(&state) {
            Ok(found) => self.counts[found] += 1,
            Err(insert_at) => {
                self.states.insert(insert_at, state);
                self.counts.insert(insert_at, 1);
            }
        }
        self.cells[index] = state;
        if self.states.len() >= before {
            return None;
        }
        if self.states.len() == 1 {
            return Some(SectionStorage::Uniform(self.states[0]));
        }
        if self.states.len() <= crate::MAX_PALETTE_STATES {
            let packed = crate::PaletteCells::build(self.cells.iter().copied(), self.cells.len())?;
            return Some(SectionStorage::Palette(Box::new(packed)));
        }
        None
    }
}

impl std::ops::Deref for DenseCells {
    type Target = [BlockStateId];

    fn deref(&self) -> &Self::Target {
        &self.cells
    }
}

impl<'a> IntoIterator for &'a DenseCells {
    type Item = &'a BlockStateId;
    type IntoIter = std::slice::Iter<'a, BlockStateId>;

    fn into_iter(self) -> Self::IntoIter {
        self.cells.iter()
    }
}

impl From<Vec<BlockStateId>> for DenseCells {
    fn from(cells: Vec<BlockStateId>) -> Self {
        Self::from_cells(cells)
    }
}

/// Internal cell layout of a section.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SectionStorage {
    /// Every cell holds the same state.
    Uniform(BlockStateId),
    /// One state identifier per cell, in linear index order.
    Dense(DenseCells),
    /// Local symbols encoded in the minimum number of bits per cell.
    Palette(Box<crate::PaletteCells>),
}

/// A fixed-size cube of authoritative block cells.
///
/// The revision advances whenever a cell changes value. Derived work such as
/// meshing captures the revision and refuses to publish a result computed from
/// an older one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Section {
    side: u32,
    revision: Revision,
    storage: SectionStorage,
    dense_edits: u32,
}

impl Section {
    pub(crate) fn resume_revision(&mut self, after: Revision) -> Result<(), SectionError> {
        self.revision = Revision(
            self.revision
                .0
                .max(after.0)
                .checked_add(1)
                .ok_or(SectionError::RevisionExhausted)?,
        );
        Ok(())
    }
    /// Create a section filled with one state.
    pub fn new(side: u32, fill: BlockStateId) -> Result<Self, SectionError> {
        let _ = checked_cell_count(side)?;
        Ok(Self {
            side,
            revision: Revision::INITIAL,
            storage: SectionStorage::Uniform(fill),
            dense_edits: 0,
        })
    }

    /// Create an empty section whose cells contain no block.
    pub fn empty(side: u32) -> Result<Self, SectionError> {
        Self::new(side, BlockStateId::EMPTY)
    }

    /// Create a section from an explicit cell layout.
    ///
    /// Used by restore, which receives storage that was already validated by the
    /// snapshot but still must match the side length of this run.
    pub fn from_storage(side: u32, storage: SectionStorage) -> Result<Self, SectionError> {
        let expected = checked_cell_count(side)?;
        if let SectionStorage::Dense(cells) = &storage
            && cells.len() != expected
        {
            return Err(SectionError::CellCountMismatch {
                expected,
                actual: cells.len(),
            });
        }
        if let SectionStorage::Palette(cells) = &storage
            && cells.len() != expected
        {
            return Err(SectionError::CellCountMismatch {
                expected,
                actual: cells.len(),
            });
        }
        let mut section = Self {
            side,
            revision: Revision::INITIAL,
            storage,
            dense_edits: 0,
        };
        section.compact();
        Ok(section)
    }

    /// Build imported cells directly into their compact layout, avoiding a
    /// temporary dense census when the final section uses a small palette.
    pub fn from_cells(side: u32, cells: Vec<BlockStateId>) -> Result<Self, SectionError> {
        let expected = checked_cell_count(side)?;
        if cells.len() != expected {
            return Err(SectionError::CellCountMismatch {
                expected,
                actual: cells.len(),
            });
        }
        let mut seen = [0u64; 1024];
        let mut distinct = 0;
        let first = cells[0];
        let mut second = first;
        for id in &cells {
            let word = &mut seen[id.0 as usize / 64];
            let bit = 1u64 << (id.0 % 64);
            if *word & bit == 0 {
                *word |= bit;
                distinct += 1;
                if distinct == 2 {
                    second = *id;
                }
            }
            if distinct > crate::MAX_PALETTE_STATES {
                break;
            }
        }
        let packed = match distinct {
            2 => crate::PaletteCells::build_binary(&cells, first, second),
            3..=crate::MAX_PALETTE_STATES => {
                crate::PaletteCells::build(cells.iter().copied(), cells.len())
            }
            _ => None,
        };
        let storage = if distinct == 1 {
            SectionStorage::Uniform(first)
        } else if let Some(packed) = packed {
            SectionStorage::Palette(Box::new(packed))
        } else {
            SectionStorage::Dense(cells.into())
        };
        Ok(Self {
            side,
            revision: Revision::INITIAL,
            storage,
            dense_edits: 0,
        })
    }

    /// Section side length in cells.
    pub const fn side(&self) -> u32 {
        self.side
    }

    /// Current terrain revision of this section.
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Current internal representation.
    pub const fn storage(&self) -> &SectionStorage {
        &self.storage
    }

    /// Logical non-uniform cells. Use `allocated_bytes` for heap accounting.
    pub fn allocated_cells(&self) -> usize {
        match &self.storage {
            SectionStorage::Uniform(_) => 0,
            SectionStorage::Dense(cells) => cells.len(),
            SectionStorage::Palette(cells) => cells.len(),
        }
    }

    pub fn cell_count(&self) -> usize {
        (self.side as usize).pow(3)
    }

    /// Heap capacity owned by the cell representation, excluding this struct.
    pub fn allocated_bytes(&self) -> usize {
        match &self.storage {
            SectionStorage::Uniform(_) => 0,
            SectionStorage::Dense(cells) => cells.allocated_bytes(),
            SectionStorage::Palette(cells) => {
                size_of::<crate::PaletteCells>() + cells.allocated_bytes()
            }
        }
    }

    /// Iterate logical values without allocating an expanded cell array.
    pub fn values(&self) -> impl Iterator<Item = BlockStateId> + Clone + '_ {
        (0..self.cell_count()).map(|index| self.value(index))
    }

    fn value(&self, index: usize) -> BlockStateId {
        match &self.storage {
            SectionStorage::Uniform(value) => *value,
            SectionStorage::Dense(cells) => cells[index],
            SectionStorage::Palette(cells) => cells.get(index).expect("validated packed index"),
        }
    }

    /// Reconsider dense storage after bulk changes without advancing revision.
    /// Imports and every full section's worth of dense edits also do this.
    /// A write that drops a state shrinks immediately when the census already
    /// shows that a palette or a single value fits.
    pub fn compact(&mut self) {
        let next = match &self.storage {
            SectionStorage::Dense(cells) if cells.states.len() == 1 => {
                Some(SectionStorage::Uniform(cells.states[0]))
            }
            SectionStorage::Dense(cells) if cells.states.len() <= crate::MAX_PALETTE_STATES => {
                crate::PaletteCells::build(cells.iter().copied(), cells.len())
                    .map(|packed| SectionStorage::Palette(Box::new(packed)))
            }
            _ => None,
        };
        if let Some(next) = next {
            self.storage = next;
        }
        self.dense_edits = 0;
    }

    /// Read one cell.
    pub fn get(&self, local: LocalPos) -> Result<BlockStateId, SectionError> {
        let index = self.index(local)?;
        Ok(self.value(index))
    }

    /// Write one cell.
    ///
    /// Returns `true` when the value changed and the revision advanced. Writing
    /// the same value is free and does not invalidate derived work.
    pub fn set(&mut self, local: LocalPos, state: BlockStateId) -> Result<bool, SectionError> {
        let index = self.index(local)?;
        let previous = self.value(index);
        if previous == state {
            return Ok(false);
        }
        // Exhaustion is checked before any allocation or cell mutation.
        let revision = self
            .revision
            .next()
            .ok_or(SectionError::RevisionExhausted)?;
        if let SectionStorage::Uniform(fill) = self.storage {
            let len = self.cell_count();
            let values = (0..len).map(|i| if i == index { state } else { fill });
            self.storage = crate::PaletteCells::build(values.clone(), len)
                .map(|packed| SectionStorage::Palette(Box::new(packed)))
                .unwrap_or_else(|| SectionStorage::Dense(values.collect::<Vec<_>>().into()));
            self.compact();
        } else if let SectionStorage::Palette(packed) = &mut self.storage {
            if !packed.set(index, state) {
                let mut cells: Vec<_> = self.values().collect();
                cells[index] = state;
                self.storage = SectionStorage::Dense(cells.into());
                self.compact();
            } else if packed.states().len() == 1 {
                self.storage = SectionStorage::Uniform(packed.states()[0]);
            }
        } else if let SectionStorage::Dense(cells) = &mut self.storage {
            if let Some(next) = cells.write(index, state) {
                self.storage = next;
                self.dense_edits = 0;
            } else {
                self.dense_edits += 1;
                if self.dense_edits as usize >= self.cell_count() {
                    self.compact();
                }
            }
        }
        self.revision = revision;
        Ok(true)
    }

    /// Visit every cell in linear index order.
    pub fn for_each(&self, mut visit: impl FnMut(LocalPos, BlockStateId)) {
        for (index, value) in self.values().enumerate() {
            visit(self.local_of(index), value);
        }
    }

    fn index(&self, local: LocalPos) -> Result<usize, SectionError> {
        let side = self.side;
        if local.x >= side || local.y >= side || local.z >= side {
            return Err(SectionError::LocalOutOfRange {
                side,
                local: [local.x, local.y, local.z],
            });
        }
        Ok(linear_index(side, local))
    }

    fn local_of(&self, index: usize) -> LocalPos {
        let side = self.side as usize;
        let x = index % side;
        let rest = index / side;
        let z = rest % side;
        let y = rest / side;
        LocalPos {
            x: x as u32,
            y: y as u32,
            z: z as u32,
        }
    }
}

/// Linear cell index inside an untranslated section.
///
/// The order is `x + side * (z + side * y)`, matching the architecture note and
/// the `for_each` traversal above.
pub fn linear_index(side: u32, local: LocalPos) -> usize {
    let side = side as usize;
    local.x as usize + side * (local.z as usize + side * local.y as usize)
}

fn checked_cell_count(side: u32) -> Result<usize, SectionError> {
    if side == 0 || side > MAX_SECTION_SIDE {
        return Err(SectionError::SideOutOfRange {
            side,
            max: MAX_SECTION_SIDE,
        });
    }
    Ok((side as usize).pow(3))
}

/// Failure produced by section storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SectionError {
    /// The requested side length is zero or larger than the supported maximum.
    SideOutOfRange {
        /// Requested side.
        side: u32,
        /// Largest supported side.
        max: u32,
    },
    /// A local coordinate did not fit inside the section.
    LocalOutOfRange {
        /// Section side.
        side: u32,
        /// Offending coordinate components.
        local: [u32; 3],
    },
    /// The revision counter cannot advance without wrapping.
    RevisionExhausted,
    /// A dense layout did not contain exactly `side^3` cells.
    CellCountMismatch {
        /// Expected number of cells.
        expected: usize,
        /// Observed number of cells.
        actual: usize,
    },
}

impl std::fmt::Display for SectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SideOutOfRange { side, max } => {
                write!(f, "section side {side} must be in 1..={max}")
            }
            Self::LocalOutOfRange { side, local } => write!(
                f,
                "local coordinate [{}, {}, {}] is outside a section of side {side}",
                local[0], local[1], local[2]
            ),
            Self::RevisionExhausted => write!(f, "section revision counter exhausted"),
            Self::CellCountMismatch { expected, actual } => {
                write!(f, "dense section has {actual} cells, expected {expected}")
            }
        }
    }
}

impl std::error::Error for SectionError {}

impl From<SpatialError> for SectionError {
    fn from(error: SpatialError) -> Self {
        match error {
            SpatialError::ZeroSectionSide => Self::SideOutOfRange {
                side: 0,
                max: MAX_SECTION_SIDE,
            },
            SpatialError::LocalCoordinateOutOfRange { side } => Self::LocalOutOfRange {
                side,
                local: [u32::MAX, u32::MAX, u32::MAX],
            },
            SpatialError::CoordinateOverflow => Self::LocalOutOfRange {
                side: 0,
                local: [u32::MAX, u32::MAX, u32::MAX],
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(x: u32, y: u32, z: u32) -> LocalPos {
        LocalPos { x, y, z }
    }

    #[test]
    fn uniform_section_allocates_nothing_until_first_edit() {
        let mut section = Section::empty(32).expect("valid side");
        assert_eq!(section.allocated_cells(), 0);
        assert_eq!(section.get(local(1, 2, 3)), Ok(BlockStateId::EMPTY));
        assert!(
            !section
                .set(local(1, 2, 3), BlockStateId::EMPTY)
                .expect("in range")
        );
        assert_eq!(section.allocated_cells(), 0);
        assert_eq!(section.revision(), Revision::INITIAL);
        assert!(
            section
                .set(local(1, 2, 3), BlockStateId(7))
                .expect("in range")
        );
        assert_eq!(section.allocated_cells(), 32usize.pow(3));
        assert_eq!(section.get(local(1, 2, 3)), Ok(BlockStateId(7)));
        assert_eq!(section.revision(), Revision(1));
    }

    #[test]
    fn repeated_write_of_same_value_keeps_revision() {
        let mut section = Section::new(8, BlockStateId(3)).expect("valid side");
        assert!(
            !section
                .set(local(7, 7, 7), BlockStateId(3))
                .expect("in range")
        );
        assert_eq!(section.revision(), Revision::INITIAL);
    }

    #[test]
    fn out_of_range_local_coordinate_is_rejected() {
        let section = Section::empty(16).expect("valid side");
        assert!(matches!(
            section.get(local(16, 0, 0)),
            Err(SectionError::LocalOutOfRange { .. })
        ));
        assert!(matches!(
            Section::empty(0),
            Err(SectionError::SideOutOfRange { .. })
        ));
        assert!(matches!(
            Section::empty(MAX_SECTION_SIDE + 1),
            Err(SectionError::SideOutOfRange { .. })
        ));
    }

    #[test]
    fn linear_index_matches_documented_order() {
        // x varies fastest, then z, then y.
        let side = 4;
        assert_eq!(linear_index(side, local(1, 0, 0)), 1);
        assert_eq!(linear_index(side, local(0, 0, 1)), 4);
        assert_eq!(linear_index(side, local(0, 1, 0)), 16);
        assert_eq!(linear_index(side, local(3, 3, 3)), 63);
    }

    #[test]
    fn traversal_covers_each_cell_once() {
        let side = 5;
        let mut section = Section::empty(side).expect("valid side");
        section
            .set(local(4, 3, 2), BlockStateId(9))
            .expect("in range");
        let mut seen = 0usize;
        let mut hits = 0usize;
        section.for_each(|_, value| {
            seen += 1;
            if value == BlockStateId(9) {
                hits += 1;
            }
        });
        assert_eq!(seen, (side as usize).pow(3));
        assert_eq!(hits, 1);
    }

    #[test]
    fn dense_roundtrip_preserves_every_cell() {
        let side = 16;
        let mut section = Section::empty(side).expect("valid side");
        for y in 0..side {
            for z in 0..side {
                for x in 0..side {
                    let value = BlockStateId(((x + y * 3 + z * 7) % 5) as u16);
                    section.set(local(x, y, z), value).expect("in range");
                }
            }
        }
        for y in 0..side {
            for z in 0..side {
                for x in 0..side {
                    let value = BlockStateId(((x + y * 3 + z * 7) % 5) as u16);
                    assert_eq!(section.get(local(x, y, z)), Ok(value));
                }
            }
        }
    }

    #[test]
    fn imported_cells_match_dense_compaction_for_all_storage_classes() {
        for (side, states) in [
            (16u32, 1),
            (2, 2),
            (5, 2),
            (16, 2),
            (16, 17),
            (16, 257),
            (16, 300),
        ] {
            let cells: Vec<_> = (0..(side as usize).pow(3))
                .map(|i| BlockStateId(u16::MAX - (i % states) as u16))
                .collect();
            let expected =
                Section::from_storage(side, SectionStorage::Dense(cells.clone().into())).unwrap();
            let actual = Section::from_cells(side, cells).unwrap();
            assert_eq!(actual.storage(), expected.storage());
            assert_eq!(actual.revision(), expected.revision());
        }
        let mut late: Vec<_> = (0..4096).map(|i| BlockStateId((i % 256) as u16)).collect();
        late[4095] = BlockStateId(300);
        let expected =
            Section::from_storage(16, SectionStorage::Dense(late.clone().into())).unwrap();
        assert_eq!(
            Section::from_cells(16, late).unwrap().storage(),
            expected.storage()
        );
        assert!(Section::from_cells(16, vec![BlockStateId::EMPTY; 4095]).is_err());
        assert!(Section::from_cells(0, vec![]).is_err());
    }

    #[test]
    fn packed_word_boundaries_and_all_widths_preserve_cells() {
        for symbols in [2, 3, 5, 9, 17, 33, 65, 129, 256] {
            let cells: Vec<_> = (0..32usize.pow(3))
                .map(|i| BlockStateId((i % symbols) as u16))
                .collect();
            let section =
                Section::from_storage(32, SectionStorage::Dense(cells.clone().into())).unwrap();
            let SectionStorage::Palette(packed) = section.storage() else {
                panic!("expected packed cells");
            };
            assert_eq!(
                usize::from(packed.bits()),
                (usize::BITS - (symbols - 1).leading_zeros()) as usize
            );
            assert_eq!(packed.states().len(), symbols);
            assert!(section.values().eq(cells));
            assert!(section.allocated_bytes() < 32usize.pow(3) * 2);
            assert_eq!(packed.get(packed.len()), None);
        }
    }

    #[test]
    fn edits_grow_shrink_and_release_the_complete_palette() {
        let mut section = Section::empty(32).unwrap();
        for i in 0..128 {
            section
                .set(section.local_of(i), BlockStateId(i as u16 + 1))
                .unwrap();
        }
        let SectionStorage::Palette(packed) = section.storage() else {
            panic!()
        };
        assert_eq!(packed.bits(), 8);
        for i in (0..128).rev() {
            section
                .set(section.local_of(i), BlockStateId::EMPTY)
                .unwrap();
            for j in 0..128 {
                assert_eq!(
                    section.value(j),
                    if j < i {
                        BlockStateId(j as u16 + 1)
                    } else {
                        BlockStateId::EMPTY
                    }
                );
            }
        }
        assert_eq!(
            section.storage(),
            &SectionStorage::Uniform(BlockStateId::EMPTY)
        );
        assert_eq!(section.allocated_bytes(), 0);
        assert_eq!(section.revision(), Revision(256));
    }

    #[test]
    fn dense_fallback_recompresses_and_uniform_reclaims_without_an_explicit_scan() {
        let mut section = Section::empty(16).unwrap();
        for i in 0..256 {
            section
                .set(section.local_of(i), BlockStateId(i as u16 + 1))
                .unwrap();
        }
        assert!(matches!(section.storage(), SectionStorage::Dense(_)));
        for i in 0..256 {
            section.set(section.local_of(i), BlockStateId(1)).unwrap();
        }
        let revision = section.revision();
        section.compact();
        assert!(matches!(section.storage(), SectionStorage::Palette(_)));
        assert_eq!(section.revision(), revision);
        for i in 0..256 {
            section
                .set(section.local_of(i), BlockStateId(i as u16 + 1))
                .unwrap();
        }
        assert!(matches!(section.storage(), SectionStorage::Dense(_)));
        for i in 0..section.cell_count() {
            section
                .set(section.local_of(i), BlockStateId(60000))
                .unwrap();
        }
        assert_eq!(
            section.storage(),
            &SectionStorage::Uniform(BlockStateId(60000))
        );
        assert_eq!(section.allocated_bytes(), 0);
    }

    #[test]
    fn a_unique_symbol_can_be_replaced_at_palette_capacity() {
        let cells: Vec<_> = (0..32usize.pow(3))
            .map(|i| BlockStateId(if i < 255 { i as u16 + 1 } else { 0 }))
            .collect();
        let mut section = Section::from_storage(32, SectionStorage::Dense(cells.into())).unwrap();
        section.set(local(0, 0, 0), BlockStateId(u16::MAX)).unwrap();
        let SectionStorage::Palette(packed) = section.storage() else {
            panic!()
        };
        assert_eq!(packed.states().len(), 256);
        assert!(!packed.states().contains(&BlockStateId(1)));
        assert_eq!(section.get(local(0, 0, 0)), Ok(BlockStateId(u16::MAX)));
    }

    #[test]
    fn exhausted_revision_and_invalid_import_leave_cells_unchanged() {
        let mut section = Section::empty(16).unwrap();
        section.set(local(1, 1, 1), BlockStateId(1)).unwrap();
        section.revision = Revision(u64::MAX);
        let before = section.clone();
        assert_eq!(
            section.set(local(2, 1, 1), BlockStateId(2)),
            Err(SectionError::RevisionExhausted)
        );
        assert_eq!(section, before);
        assert_eq!(section.set(local(1, 1, 1), BlockStateId(1)), Ok(false));
        assert!(Section::from_storage(32, section.storage().clone()).is_err());
        assert!(Section::from_storage(16, SectionStorage::Dense(Vec::new().into())).is_err());
    }

    #[test]
    fn dense_shrinks_when_a_state_disappears_without_editing_every_cell() {
        let mut section = Section::empty(16).unwrap();
        for i in 0..257 {
            section
                .set(section.local_of(i), BlockStateId(i as u16 + 1))
                .unwrap();
        }
        assert!(matches!(section.storage(), SectionStorage::Dense(_)));
        for i in 1..257 {
            section
                .set(section.local_of(i), BlockStateId::EMPTY)
                .unwrap();
        }
        assert!(
            matches!(section.storage(), SectionStorage::Palette(_)),
            "two remaining states pack without a full-section edit sweep"
        );
        assert_eq!(section.get(section.local_of(0)), Ok(BlockStateId(1)));
        assert_eq!(section.get(section.local_of(1)), Ok(BlockStateId::EMPTY));
        assert!(section.allocated_bytes() < section.cell_count() * size_of::<BlockStateId>());
        section
            .set(section.local_of(0), BlockStateId::EMPTY)
            .unwrap();
        assert_eq!(
            section.storage(),
            &SectionStorage::Uniform(BlockStateId::EMPTY)
        );
        assert_eq!(section.allocated_bytes(), 0);
        assert!(section.values().all(|value| value == BlockStateId::EMPTY));
    }

    #[test]
    fn mixed_edits_match_a_reference_vector_on_every_supported_side() {
        let mut random = 73u64;
        for side in [1, 2, 3, 16, 32, 64] {
            let mut section = Section::empty(side).unwrap();
            let mut cells = vec![BlockStateId::EMPTY; section.cell_count()];
            for step in 0..6000 {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                let index = (random >> 32) as usize % cells.len();
                let state =
                    BlockStateId(((random >> 8) % if step < 3000 { 511 } else { 3 }) as u16);
                let changed = cells[index] != state;
                assert_eq!(section.set(section.local_of(index), state), Ok(changed));
                cells[index] = state;
                assert_eq!(section.value(index), state);
                if step % 500 == 0 {
                    section.compact();
                    assert!(section.values().eq(cells.iter().copied()));
                }
            }
            assert!(section.values().eq(cells));
        }
    }
}
