//! Snapshot body: the complete authoritative world at one commit position.
//!
//! The layout follows `torse_world::Snapshot` field by field. Canonical order is part of the
//! format and is enforced in both directions: states ascend by runtime identifier, sections
//! by coordinate (x, then y, then z), entities by persistent identity, and components and
//! fields by content key. Equal snapshots therefore produce equal bytes, and a reordered or
//! duplicated entry is reported instead of being silently merged.
//!
//! The codec checks structure only. It does not resolve state identifiers against the state
//! table, content keys against a registry, components against schemas, or positions against
//! world limits. `Snapshot::restore` performs those checks against the running content, and
//! unknown content fails there instead of being substituted.

use torse_types::{EntityId, SectionPos};
use torse_world::{
    BlockStateId, MAX_SECTION_SIDE, Snapshot, SnapshotCells, SnapshotComponent, SnapshotEntity,
    SnapshotSection, StateEntry, WorldBounds,
};

use crate::metadata::{SNAPSHOT_META_BYTES, read_snapshot_meta, write_snapshot_meta};
use crate::value::{
    POSITION_BYTES, ascending, charge_vec, invalid, read_entity, read_items, read_key, read_len,
    read_position, read_snapshot_entries, write_entity, write_items, write_key, write_len,
    write_position, write_snapshot_entries,
};
use crate::{Decoder, EncodeResult, Encoder};

// Section cell tags.
const CELLS_UNIFORM: u8 = 0;
const CELLS_DENSE: u8 = 1;
const CELLS_PACKED: u8 = 2;

fn symbols_per_byte(states: &[StateEntry]) -> Option<usize> {
    match states.len() {
        1 => Some(8),
        2..=3 => Some(4),
        4..=15 => Some(2),
        _ => None,
    }
}

pub(crate) fn has_packable_cells(value: &Snapshot) -> bool {
    symbols_per_byte(&value.states).is_some()
        && value
            .sections
            .iter()
            .any(|s| matches!(&s.cells, SnapshotCells::Dense(cells) if cells.len() > 1))
}

/// Minimum widths used to reject impossible counts before allocation.
const MIN_STATE_BYTES: usize = 2 + 1 + 4; // id, key length, value count
const MIN_SECTION_BYTES: usize = 24 + 1 + 2; // position, tag, uniform state
const MIN_ENTITY_BYTES: usize = 16 + POSITION_BYTES + 4; // id, position, component count
const MIN_COMPONENT_BYTES: usize = 1 + 4; // key length, field count

/// Fixed part of the body that follows the metadata: side, min_y, max_y.
pub(crate) const SNAPSHOT_HEADER_BYTES: usize = SNAPSHOT_META_BYTES + 4 + 8 + 8;

/// Cells in one dense section. The side is bounded by the world's own maximum, so the
/// product fits comfortably and a hostile side cannot request a giant allocation.
fn cells_per_section(side: u32) -> EncodeResult<usize> {
    if side == 0 || side > MAX_SECTION_SIDE {
        return Err(invalid(format!(
            "section side {side} is outside 1..={MAX_SECTION_SIDE}"
        )));
    }
    let side = side as usize;
    Ok(side * side * side)
}

/// Append one snapshot body: metadata, extent, state table, sections, then entities.
pub(crate) fn write_snapshot(out: &mut Encoder<'_>, value: &Snapshot) -> EncodeResult<()> {
    value
        .validate_entity_allocator()
        .map_err(|error| invalid(error.to_string()))?;
    let cells = cells_per_section(value.side)?;
    // `WorldBounds` is constructible with public fields, so its invariant is checked here
    // rather than trusted; the reader rebuilds the bounds through `WorldBounds::new`.
    if value.bounds.min_y >= value.bounds.max_y {
        return Err(invalid("snapshot bounds are empty"));
    }
    out.ensure(SNAPSHOT_HEADER_BYTES as u64)?;
    write_snapshot_meta(out, value.meta)?;
    out.write_u32(value.side)?;
    out.write_i64(value.bounds.min_y)?;
    out.write_i64(value.bounds.max_y)?;
    write_states(out, &value.states)?;
    write_sections(out, &value.sections, cells, &value.states)?;
    write_entities(out, &value.entities)?;
    out.write_u128(value.next_entity_id)?;
    // Preserve the codec's earlier, more specific ordering and resource errors.
    // The encoder remains private until this domain validation also succeeds.
    value
        .validate_structure()
        .map_err(|error| invalid(error.to_string()))
}

pub(crate) fn read_snapshot(input: &mut Decoder<'_>) -> EncodeResult<Snapshot> {
    let meta = read_snapshot_meta(input)?;
    let side = input.read_u32()?;
    let cells = cells_per_section(side)?;
    let min_y = input.read_i64()?;
    let max_y = input.read_i64()?;
    let bounds = WorldBounds::new(min_y, max_y).map_err(|error| invalid(error.to_string()))?;
    let states = read_states(input)?;
    let sections = read_sections(input, cells, &states)?;
    let entities = read_entities(input)?;
    let next_entity_id = input.read_u128()?;
    let snapshot = Snapshot {
        meta,
        side,
        bounds,
        states,
        sections,
        entities,
        next_entity_id,
    };
    snapshot
        .validate_structure()
        .map_err(|error| invalid(error.to_string()))?;
    Ok(snapshot)
}

// State table -------------------------------------------------------------------------------
//
// Identifier zero is the empty-occupancy sentinel and is never listed: a reader that saw it
// could not tell whether the table redefines "empty".

fn write_states(out: &mut Encoder<'_>, states: &[StateEntry]) -> EncodeResult<()> {
    write_items(out, states.len(), "state")?;
    let mut previous = None;
    for entry in states {
        if entry.id.is_empty() {
            return Err(invalid("state table lists the empty sentinel"));
        }
        ascending(previous, &entry.id, "state")?;
        previous = Some(&entry.id);
        out.write_u16(entry.id.0)?;
        write_key(out, &entry.block)?;
        write_len(out, entry.values.len(), "state value")?;
        for value in &entry.values {
            out.write_u16(*value)?;
        }
    }
    Ok(())
}

fn read_states(input: &mut Decoder<'_>) -> EncodeResult<Vec<StateEntry>> {
    let count = read_items(input, MIN_STATE_BYTES, "state")?;
    charge_vec::<StateEntry>(input, count)?;
    let mut states: Vec<StateEntry> = Vec::with_capacity(count);
    for _ in 0..count {
        let id = BlockStateId(input.read_u16()?);
        if id.is_empty() {
            return Err(invalid("state table lists the empty sentinel"));
        }
        ascending(states.last().map(|entry| &entry.id), &id, "state")?;
        let block = read_key(input)?;
        let len = read_len(input, 2, "state value")?;
        charge_vec::<u16>(input, len)?;
        let mut values = Vec::with_capacity(len);
        for _ in 0..len {
            values.push(input.read_u16()?);
        }
        states.push(StateEntry { id, block, values });
    }
    Ok(states)
}

// Sections ----------------------------------------------------------------------------------
//
// A dense section has no length prefix: its length is exactly `side^3`, already fixed by the
// header. Storing it again would only create a second value that could disagree.

fn write_sections(
    out: &mut Encoder<'_>,
    sections: &[SnapshotSection],
    cells: usize,
    states: &[StateEntry],
) -> EncodeResult<()> {
    write_items(out, sections.len(), "section")?;
    let mut previous = None;
    for section in sections {
        ascending(previous, &section.pos, "section")?;
        previous = Some(&section.pos);
        out.write_i64(section.pos.x)?;
        out.write_i64(section.pos.y)?;
        out.write_i64(section.pos.z)?;
        match &section.cells {
            SnapshotCells::Uniform(state) => {
                out.write_u8(CELLS_UNIFORM)?;
                out.write_u16(state.0)?;
            }
            SnapshotCells::Dense(list) => {
                if list.len() != cells {
                    return Err(invalid(format!(
                        "dense section {:?} holds {} cells, expected {cells}",
                        section.pos,
                        list.len()
                    )));
                }
                out.ensure(1 + 2 * cells as u64)?;
                if out.packed_snapshot_cells
                    && cells > 1
                    && let Some(per_byte) = symbols_per_byte(states)
                {
                    out.write_u8(CELLS_PACKED)?;
                    write_packed(out, list, states, per_byte)?;
                } else {
                    out.write_u8(CELLS_DENSE)?;
                    write_dense(out, list)?;
                }
            }
        }
    }
    Ok(())
}

/// Copy dense cells through a small stack buffer instead of one call per cell.
fn write_dense(out: &mut Encoder<'_>, cells: &[BlockStateId]) -> EncodeResult<()> {
    let mut buffer = [0u8; 512];
    for chunk in cells.chunks(buffer.len() / 2) {
        for (slot, state) in buffer.chunks_exact_mut(2).zip(chunk) {
            slot.copy_from_slice(&state.0.to_le_bytes());
        }
        out.write(&buffer[..chunk.len() * 2])?;
    }
    Ok(())
}

fn write_packed(
    out: &mut Encoder<'_>,
    cells: &[BlockStateId],
    states: &[StateEntry],
    per_byte: usize,
) -> EncodeResult<()> {
    out.charge_saved_bytes(cells.len() * 2 - cells.len().div_ceil(per_byte))?;
    if let [state] = states {
        // The enclosing snapshot validates the original cells before finishing,
        // including IDs that are neither empty nor the declared state.
        return write_symbols(out, cells, per_byte, |cell| Ok(u8::from(cell == state.id)));
    }
    write_symbols(out, cells, per_byte, |cell| {
        if cell.is_empty() {
            Ok(0)
        } else {
            states
                .binary_search_by_key(&cell, |entry| entry.id)
                .map(|index| index as u8 + 1)
                .map_err(|_| invalid("packed cell references an undefined state"))
        }
    })
}

fn write_symbols(
    out: &mut Encoder<'_>,
    cells: &[BlockStateId],
    per_byte: usize,
    symbol: impl Fn(BlockStateId) -> EncodeResult<u8>,
) -> EncodeResult<()> {
    let bits = 8 / per_byte;
    let mut buffer = [0u8; 256];
    for chunk in cells.chunks(buffer.len() * per_byte) {
        for (byte, group) in buffer.iter_mut().zip(chunk.chunks(per_byte)) {
            *byte = 0;
            for (index, &state) in group.iter().enumerate() {
                *byte |= symbol(state)? << (index * bits);
            }
        }
        out.write(&buffer[..chunk.len().div_ceil(per_byte)])?;
    }
    Ok(())
}

fn read_packed(
    input: &mut Decoder<'_>,
    cells: usize,
    states: &[StateEntry],
) -> EncodeResult<SnapshotCells> {
    let per_byte = symbols_per_byte(states)
        .ok_or_else(|| invalid("packed section state table exceeds symbol range"))?;
    let bits = 8 / per_byte;
    let bytes = input.read(cells.div_ceil(per_byte))?;
    let tail = cells % per_byte;
    if tail != 0 && bytes.last().is_some_and(|byte| byte >> (tail * bits) != 0) {
        return Err(invalid("packed section has nonzero padding"));
    }
    charge_vec::<BlockStateId>(input, cells)?;
    let mut dictionary = [BlockStateId::EMPTY; 16];
    for (slot, state) in dictionary[1..].iter_mut().zip(states) {
        *slot = state.id;
    }
    let dictionary = &dictionary[..=states.len()];
    let mask = (1 << bits) - 1;
    let symbols = bytes
        .iter()
        .flat_map(|byte| (0..per_byte).map(move |index| (byte >> (index * bits)) & mask))
        .take(cells);
    let mut values = Vec::with_capacity(cells);
    for symbol in symbols {
        let state = dictionary
            .get(usize::from(symbol))
            .copied()
            .ok_or_else(|| invalid("packed section contains an undefined symbol"))?;
        values.push(state);
    }
    Ok(SnapshotCells::Dense(values))
}

fn read_sections(
    input: &mut Decoder<'_>,
    cells: usize,
    states: &[StateEntry],
) -> EncodeResult<Vec<SnapshotSection>> {
    let count = read_items(input, MIN_SECTION_BYTES, "section")?;
    charge_vec::<SnapshotSection>(input, count)?;
    let mut sections: Vec<SnapshotSection> = Vec::with_capacity(count);
    for _ in 0..count {
        let pos = SectionPos::new(input.read_i64()?, input.read_i64()?, input.read_i64()?);
        ascending(sections.last().map(|section| &section.pos), &pos, "section")?;
        let cells = match input.read_u8()? {
            CELLS_UNIFORM => SnapshotCells::Uniform(BlockStateId(input.read_u16()?)),
            CELLS_DENSE => {
                // The whole section is borrowed first, so a truncated section fails before
                // the cell vector is allocated or charged.
                let bytes = input.read(cells * 2)?;
                charge_vec::<BlockStateId>(input, cells)?;
                let list = bytes
                    .chunks_exact(2)
                    .map(|pair| BlockStateId(u16::from_le_bytes([pair[0], pair[1]])))
                    .collect();
                SnapshotCells::Dense(list)
            }
            CELLS_PACKED if input.packed_snapshot_cells && cells > 1 => {
                read_packed(input, cells, states)?
            }
            tag => return Err(invalid(format!("unknown section cell tag {tag}"))),
        };
        sections.push(SnapshotSection { pos, cells });
    }
    Ok(sections)
}

// Entities ----------------------------------------------------------------------------------

fn write_entities(out: &mut Encoder<'_>, entities: &[SnapshotEntity]) -> EncodeResult<()> {
    write_items(out, entities.len(), "entity")?;
    let mut previous = None;
    for entity in entities {
        ascending(previous, &entity.id, "entity")?;
        previous = Some(&entity.id);
        write_entity(out, entity.id)?;
        write_position(out, entity.position_bits)?;
        write_items(out, entity.components.len(), "component")?;
        let mut previous_schema = None;
        for component in &entity.components {
            ascending(previous_schema, &component.schema, "component")?;
            previous_schema = Some(&component.schema);
            write_key(out, &component.schema)?;
            // Top-level fields are not nested; depth counts containers inside a value.
            write_snapshot_entries(out, &component.fields, 0)?;
        }
    }
    Ok(())
}

fn read_entities(input: &mut Decoder<'_>) -> EncodeResult<Vec<SnapshotEntity>> {
    let count = read_items(input, MIN_ENTITY_BYTES, "entity")?;
    charge_vec::<SnapshotEntity>(input, count)?;
    let mut entities: Vec<SnapshotEntity> = Vec::with_capacity(count);
    for _ in 0..count {
        let id: EntityId = read_entity(input)?;
        ascending(entities.last().map(|entity| &entity.id), &id, "entity")?;
        let position_bits = read_position(input)?;
        let components = read_components(input)?;
        entities.push(SnapshotEntity {
            id,
            position_bits,
            components,
        });
    }
    Ok(entities)
}

fn read_components(input: &mut Decoder<'_>) -> EncodeResult<Vec<SnapshotComponent>> {
    let count = read_items(input, MIN_COMPONENT_BYTES, "component")?;
    charge_vec::<SnapshotComponent>(input, count)?;
    let mut components: Vec<SnapshotComponent> = Vec::with_capacity(count);
    for _ in 0..count {
        let schema = read_key(input)?;
        ascending(
            components.last().map(|component| &component.schema),
            &schema,
            "component",
        )?;
        let fields = read_snapshot_entries(input)?;
        components.push(SnapshotComponent { schema, fields });
    }
    Ok(components)
}

#[cfg(test)]
mod packed_tests {
    use super::*;
    use crate::CodecLimits;
    use torse_types::{ContentKey, FaultCode, OutputBudget};

    fn states(count: u16) -> Vec<StateEntry> {
        (1..=count)
            .map(|id| StateEntry {
                id: BlockStateId(2 * id),
                block: ContentKey::parse("test:block").unwrap(),
                values: vec![],
            })
            .collect()
    }

    #[test]
    fn packed_widths_preserve_sparse_ids_and_charge_expanded_cells() {
        let limits = CodecLimits {
            max_frame_bytes: 4096,
            max_field_bytes: 4096,
            max_depth: 8,
        };
        let budget = OutputBudget::new(64, 4096);
        for count in [1, 2, 3, 4, 15] {
            let states = states(count);
            let per_byte = symbols_per_byte(&states).unwrap();
            let cells: Vec<_> = (0..125)
                .map(|i| BlockStateId(2 * (i % (count + 1))))
                .collect();
            let mut bytes = Vec::new();
            let mut encoder = Encoder::new(&mut bytes, budget, limits);
            write_packed(&mut encoder, &cells, &states, per_byte).unwrap();
            assert_eq!(encoder.guard().used_bytes(), 250);
            encoder.finish();
            assert_eq!(bytes.len(), cells.len().div_ceil(per_byte));
            let mut decoder = Decoder::new(&bytes, budget, limits);
            assert_eq!(
                read_packed(&mut decoder, cells.len(), &states).unwrap(),
                SnapshotCells::Dense(cells)
            );
            let mut decoder = Decoder::new(&bytes, OutputBudget::new(64, 249), limits);
            assert!(matches!(
                read_packed(&mut decoder, 125, &states),
                Err(e) if e.code == FaultCode::ResourceLimit
            ));
            let mut decoder = Decoder::new(&bytes[..bytes.len() - 1], budget, limits);
            assert!(read_packed(&mut decoder, 125, &states).is_err());
            *bytes.last_mut().unwrap() |= 0x80;
            let mut decoder = Decoder::new(&bytes, budget, limits);
            assert!(read_packed(&mut decoder, 125, &states).is_err());
        }
        let mut decoder = Decoder::new(&[3], budget, limits);
        assert!(read_packed(&mut decoder, 4, &states(2)).is_err());
        assert!(symbols_per_byte(&states(16)).is_none());
    }
}
