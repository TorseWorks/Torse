//! Transactional encoder that appends to a caller-owned buffer.

use torse_types::{Fault, OutputBudget};

use crate::{BudgetGuard, CodecLimits, EncodeResult};

/// Appends encoded bytes to a buffer and rolls them back unless finished.
///
/// Dropping an encoder without calling [`Encoder::finish`] truncates the buffer to the length
/// it had when the encoder was created, so an early return through `?` never leaves a partial
/// record behind. Each write checks the byte budget before the buffer grows.
pub struct Encoder<'a> {
    buffer: &'a mut Vec<u8>,
    start: usize,
    limits: CodecLimits,
    guard: BudgetGuard,
    committed: bool,
    pub(crate) packed_snapshot_cells: bool,
}

impl<'a> Encoder<'a> {
    pub fn new(buffer: &'a mut Vec<u8>, budget: OutputBudget, limits: CodecLimits) -> Self {
        let start = buffer.len();
        Self {
            buffer,
            start,
            limits,
            guard: BudgetGuard::new(budget),
            committed: false,
            packed_snapshot_cells: false,
        }
    }

    /// Fail unless `bytes` more bytes fit in the budget. Charges nothing.
    pub fn ensure(&self, bytes: u64) -> EncodeResult<()> {
        self.guard.check_bytes(bytes)
    }

    pub fn write(&mut self, data: &[u8]) -> EncodeResult<()> {
        self.guard.charge_bytes(data.len() as u64)?;
        self.buffer.extend_from_slice(data);
        Ok(())
    }

    pub fn write_u8(&mut self, value: u8) -> EncodeResult<()> {
        self.write(&[value])
    }

    pub fn write_u16(&mut self, value: u16) -> EncodeResult<()> {
        self.write(&value.to_le_bytes())
    }

    pub fn write_u32(&mut self, value: u32) -> EncodeResult<()> {
        self.write(&value.to_le_bytes())
    }

    pub fn write_u64(&mut self, value: u64) -> EncodeResult<()> {
        self.write(&value.to_le_bytes())
    }

    pub fn write_u128(&mut self, value: u128) -> EncodeResult<()> {
        self.write(&value.to_le_bytes())
    }

    /// Write a two's-complement signed value in little-endian order.
    pub fn write_i64(&mut self, value: i64) -> EncodeResult<()> {
        self.write(&value.to_le_bytes())
    }

    /// Write a u32 length prefix and the data, bounded by the configured field limit.
    pub fn write_field(&mut self, data: &[u8]) -> EncodeResult<()> {
        self.write_prefixed(data, self.limits.max_field_bytes, "field")
    }

    /// Write prefix and data together: either both are appended or neither is.
    pub(crate) fn write_prefixed(&mut self, data: &[u8], max: u32, kind: &str) -> EncodeResult<()> {
        let len = u32::try_from(data.len())
            .ok()
            .filter(|&len| len <= max)
            .ok_or_else(|| {
                Fault::resource_limit(format!("{kind} length {} exceeds limit {max}", data.len()))
            })?;
        self.ensure(4 + u64::from(len))?;
        self.write_u32(len)?;
        self.write(data)
    }

    /// Count logical entries such as objects or collection elements.
    pub fn count_items(&mut self, items: u32) -> EncodeResult<()> {
        self.guard.charge_items(items)
    }

    /// Keep the original byte quota when the durable layout uses fewer bytes.
    pub(crate) fn charge_saved_bytes(&mut self, bytes: usize) -> EncodeResult<()> {
        self.guard.charge_bytes(bytes as u64)
    }

    /// Bytes appended by this encoder so far.
    pub fn written(&self) -> usize {
        self.buffer.len() - self.start
    }

    pub fn limits(&self) -> CodecLimits {
        self.limits
    }

    pub fn guard(&self) -> &BudgetGuard {
        &self.guard
    }

    /// Keep the appended bytes and return their length.
    pub fn finish(mut self) -> usize {
        self.committed = true;
        self.written()
    }
}

impl Drop for Encoder<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.buffer.truncate(self.start);
        }
    }
}
