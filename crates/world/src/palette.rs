//! Bounded local state palettes with densely packed, cross-word indices.
use crate::BlockStateId;

/// Maximum local symbols before falling back to direct state identifiers.
pub const MAX_PALETTE_STATES: usize = 256;

/// Validated packed cells. Indices and reference counts stay private.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaletteCells {
    states: Vec<BlockStateId>,
    counts: Vec<u32>,
    words: Vec<u64>,
    bits: u8,
    len: usize,
}

impl PaletteCells {
    pub fn states(&self) -> &[BlockStateId] {
        &self.states
    }
    pub const fn bits(&self) -> u8 {
        self.bits
    }
    pub const fn len(&self) -> usize {
        self.len
    }
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn allocated_bytes(&self) -> usize {
        self.states.capacity() * size_of::<BlockStateId>()
            + self.counts.capacity() * size_of::<u32>()
            + self.words.capacity() * size_of::<u64>()
    }
    pub fn get(&self, index: usize) -> Option<BlockStateId> {
        (index < self.len).then(|| self.states[self.symbol(index)])
    }
    /// Pack a slice whose census has found exactly two states in this order.
    pub(crate) fn build_binary(
        cells: &[BlockStateId],
        first: BlockStateId,
        second: BlockStateId,
    ) -> Option<Self> {
        let len = cells.len();
        if !smaller(len, 2, 1) {
            return None;
        }
        let mut second_count = 0;
        let mut words = Vec::with_capacity(word_count(len, 1));
        for chunk in cells.chunks(64) {
            let mut word = 0u64;
            for (bit, &state) in chunk.iter().enumerate() {
                debug_assert!(state == first || state == second);
                word |= u64::from(state == second) << bit;
            }
            second_count += word.count_ones();
            words.push(word);
        }
        Some(Self {
            states: vec![first, second],
            counts: vec![len as u32 - second_count, second_count],
            words,
            bits: 1,
            len,
        })
    }
    pub(crate) fn build(
        values: impl Iterator<Item = BlockStateId> + Clone,
        len: usize,
    ) -> Option<Self> {
        let mut states = Vec::new();
        let mut counts = Vec::new();
        for value in values.clone() {
            if let Some(index) = states.iter().position(|s| *s == value) {
                counts[index] += 1;
            } else {
                if states.len() == MAX_PALETTE_STATES {
                    return None;
                }
                states.push(value);
                counts.push(1u32);
            }
        }
        if states.len() < 2 {
            return None;
        }
        let bits = width(states.len());
        if !smaller(len, states.len(), bits) {
            return None;
        }
        let mut packed = Self {
            states,
            counts,
            words: vec![0; word_count(len, bits)],
            bits,
            len,
        };
        for (index, value) in values.enumerate() {
            let symbol = packed
                .states
                .iter()
                .position(|s| *s == value)
                .expect("counted state");
            packed.put(index, symbol);
        }
        packed.states.shrink_to_fit();
        packed.counts.shrink_to_fit();
        Some(packed)
    }
    /// False requests a dense fallback; the previous packed value is unchanged.
    pub(crate) fn set(&mut self, index: usize, state: BlockStateId) -> bool {
        let old = self.symbol(index);
        let symbol = if let Some(symbol) = self.states.iter().position(|s| *s == state) {
            symbol
        } else if self.counts[old] == 1 {
            // Reuse the vacated symbol without growing the dictionary.
            self.states[old] = state;
            return true;
        } else {
            let n = self.states.len() + 1;
            let bits = width(n);
            if n > MAX_PALETTE_STATES || !smaller(self.len, n, bits) {
                return false;
            }
            if bits != self.bits {
                self.repack(bits, None);
            }
            self.states.push(state);
            self.counts.push(0);
            self.states.shrink_to_fit();
            self.counts.shrink_to_fit();
            n - 1
        };
        if symbol == old {
            return true;
        }
        self.put(index, symbol);
        self.counts[old] -= 1;
        self.counts[symbol] += 1;
        if self.counts[old] == 0 {
            self.states.remove(old);
            self.counts.remove(old);
            self.repack(width(self.states.len()), Some(old));
            self.states.shrink_to_fit();
            self.counts.shrink_to_fit();
        }
        true
    }
    fn repack(&mut self, bits: u8, removed: Option<usize>) {
        let previous = std::mem::replace(&mut self.words, vec![0; word_count(self.len, bits)]);
        let old_bits = std::mem::replace(&mut self.bits, bits);
        for index in 0..self.len {
            let mut symbol = read(&previous, old_bits, index);
            if removed.is_some_and(|removed| symbol > removed) {
                symbol -= 1;
            }
            self.put(index, symbol);
        }
    }
    fn symbol(&self, index: usize) -> usize {
        read(&self.words, self.bits, index)
    }
    fn put(&mut self, index: usize, value: usize) {
        let offset = index * usize::from(self.bits);
        let word = offset / 64;
        let shift = offset % 64;
        let mask = (1u64 << self.bits) - 1;
        self.words[word] = (self.words[word] & !(mask << shift)) | ((value as u64) << shift);
        if shift + usize::from(self.bits) > 64 {
            let upper_bits = shift + usize::from(self.bits) - 64;
            let upper_mask = (1u64 << upper_bits) - 1;
            self.words[word + 1] =
                (self.words[word + 1] & !upper_mask) | ((value as u64) >> (64 - shift));
        }
    }
}
fn read(words: &[u64], bits: u8, index: usize) -> usize {
    let offset = index * usize::from(bits);
    let word = offset / 64;
    let shift = offset % 64;
    let mut value = words[word] >> shift;
    if shift + usize::from(bits) > 64 {
        value |= words[word + 1] << (64 - shift);
    }
    (value & ((1u64 << bits) - 1)) as usize
}
fn width(states: usize) -> u8 {
    ((usize::BITS - states.saturating_sub(1).leading_zeros()) as u8).max(1)
}
fn word_count(len: usize, bits: u8) -> usize {
    (len * usize::from(bits)).div_ceil(64)
}
fn smaller(len: usize, states: usize, bits: u8) -> bool {
    size_of::<PaletteCells>() + word_count(len, bits) * 8 + states * 6 < len * 2
}
