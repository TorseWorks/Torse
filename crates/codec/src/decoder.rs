//! Bounded decoder over a borrowed input slice.

use torse_types::{Fault, OutputBudget};

use crate::{BudgetGuard, CodecLimits, EncodeResult};

/// Reads fixed-width values and length-prefixed data from a borrowed slice.
///
/// Reads borrow from the input and do not charge the budget. Codec implementations charge
/// items and bytes through [`Decoder::charge_items`] and [`Decoder::charge_bytes`] before
/// allocating owned payloads or containers.
pub struct Decoder<'a> {
    input: &'a [u8],
    position: usize,
    depth: u16,
    limits: CodecLimits,
    guard: BudgetGuard,
    pub(crate) packed_snapshot_cells: bool,
}

impl<'a> Decoder<'a> {
    pub fn new(input: &'a [u8], budget: OutputBudget, limits: CodecLimits) -> Self {
        Self {
            input,
            position: 0,
            depth: 0,
            limits,
            guard: BudgetGuard::new(budget),
            packed_snapshot_cells: false,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.position == self.input.len()
    }

    pub fn remaining(&self) -> usize {
        self.input.len() - self.position
    }

    /// Borrow exactly `len` bytes. On failure the position is unchanged.
    pub fn read(&mut self, len: usize) -> EncodeResult<&'a [u8]> {
        if len > self.remaining() {
            return Err(Fault::invalid_data(format!(
                "unexpected end of input: need {} bytes, have {}",
                len,
                self.remaining()
            )));
        }
        let start = self.position;
        self.position += len;
        Ok(&self.input[start..self.position])
    }

    fn read_array<const N: usize>(&mut self) -> EncodeResult<[u8; N]> {
        let mut array = [0; N];
        array.copy_from_slice(self.read(N)?);
        Ok(array)
    }

    pub fn read_u8(&mut self) -> EncodeResult<u8> {
        Ok(u8::from_le_bytes(self.read_array()?))
    }

    pub fn read_u16(&mut self) -> EncodeResult<u16> {
        Ok(u16::from_le_bytes(self.read_array()?))
    }

    pub fn read_u32(&mut self) -> EncodeResult<u32> {
        Ok(u32::from_le_bytes(self.read_array()?))
    }

    pub fn read_u64(&mut self) -> EncodeResult<u64> {
        Ok(u64::from_le_bytes(self.read_array()?))
    }

    pub fn read_u128(&mut self) -> EncodeResult<u128> {
        Ok(u128::from_le_bytes(self.read_array()?))
    }

    /// Read a two's-complement signed value in little-endian order.
    pub fn read_i64(&mut self) -> EncodeResult<i64> {
        Ok(i64::from_le_bytes(self.read_array()?))
    }

    /// Read a u32 length prefix and the data, bounded by the configured field limit.
    pub fn read_field(&mut self) -> EncodeResult<&'a [u8]> {
        self.read_prefixed(self.limits.max_field_bytes, "field")
    }

    /// Check the declared length against `max` before reading the data.
    pub(crate) fn read_prefixed(&mut self, max: u32, kind: &str) -> EncodeResult<&'a [u8]> {
        let len = self.read_u32()?;
        if len > max {
            return Err(Fault::invalid_data(format!(
                "{kind} length {len} exceeds limit {max}"
            )));
        }
        self.read(len as usize)
    }

    /// Enter one level of nesting, bounded by the configured depth limit.
    pub fn enter(&mut self) -> EncodeResult<()> {
        if self.depth >= self.limits.max_depth {
            return Err(Fault::invalid_data(format!(
                "nesting depth exceeds limit {}",
                self.limits.max_depth
            )));
        }
        self.depth += 1;
        Ok(())
    }

    pub fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    pub fn charge_items(&mut self, items: u32) -> EncodeResult<()> {
        self.guard.charge_items(items)
    }

    pub fn charge_bytes(&mut self, bytes: u64) -> EncodeResult<()> {
        self.guard.charge_bytes(bytes)
    }

    pub fn limits(&self) -> CodecLimits {
        self.limits
    }

    pub fn guard(&self) -> &BudgetGuard {
        &self.guard
    }
}
