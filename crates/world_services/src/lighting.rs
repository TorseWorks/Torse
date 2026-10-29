//! Bounded monochromatic emitted light and skylight, as disposable derived data.
//!
//! Levels are 0..=15. Every edge loses at least one level, so a 15-cell halo
//! contains every source that can affect the requested section. Missing input
//! is opaque and makes the result incomplete; it is never interpreted as air.
//! Skylight additionally observes bounded columns up to the world ceiling.
use std::collections::{BTreeMap, BinaryHeap};
use std::sync::atomic::{AtomicU64, Ordering};
use torse_types::{Binding, BlockPos, Digest, Fault, Revision, SectionPos};
use torse_world::{BlockStateRegistry, SectionPhase, SectionStore, TerrainQuery, WorldBounds};

pub const MAX_LIGHT: u8 = 15;
static NEXT_CACHE: AtomicU64 = AtomicU64::new(1);

fn next_cache_identity(counter: &AtomicU64) -> Result<u64, Fault> {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let next = current
            .checked_add(1)
            .ok_or_else(|| Fault::resource_limit("light cache identities exhausted"))?;
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(current),
            Err(observed) => current = observed,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LightLimits {
    pub cells: usize,
    pub dependencies: usize,
    pub frontier: usize,
    /// Peak owned buffers, output and native structs; allocator bookkeeping is excluded.
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Dependency {
    pos: SectionPos,
    revision: Option<Revision>,
    readable: bool,
}

#[derive(Clone, Debug)]
struct Observation {
    binding: Binding,
    content: Digest,
    side: u32,
    bounds: WorldBounds,
    dependencies: Vec<Dependency>,
}
impl Observation {
    fn valid(&self, binding: Binding, store: &SectionStore, registry: &BlockStateRegistry) -> bool {
        self.binding == binding
            && self.content == registry.authority_digest()
            && self.side == store.side()
            && self.bounds == store.bounds()
            && self.dependencies.iter().all(|d| {
                store.revision(d.pos) == d.revision && readable(store, d.pos) == d.readable
            })
    }
    fn bytes(&self) -> usize {
        self.dependencies.capacity() * size_of::<Dependency>()
    }
}

/// An opaque generation for work which consumed a published lighting result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LightStamp {
    binding: Binding,
    target: SectionPos,
    generation: u64,
    cache: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightQuery {
    Ready { level: u8, complete: bool },
    Pending,
    OutOfBounds,
}

/// Fully resolved target cells, with the source revisions needed to use them.
#[derive(Debug)]
pub struct LightSection {
    target: SectionPos,
    observed: Observation,
    levels: Vec<u8>,
    complete: bool,
}
impl LightSection {
    /// Borrow resolved cells in the store's linear order after validating this observation.
    pub fn levels(&self) -> &[u8] {
        &self.levels
    }
    pub fn target(&self) -> SectionPos {
        self.target
    }
    pub fn is_complete(&self) -> bool {
        self.complete
    }
    pub fn allocated_bytes(&self) -> usize {
        size_of::<Self>() + self.levels.capacity() + self.observed.bytes()
    }
    pub fn validate(
        &self,
        binding: Binding,
        store: &SectionStore,
        registry: &BlockStateRegistry,
    ) -> bool {
        self.observed.valid(binding, store, registry)
    }
}

/// Incremental read-only calculation. Publication never exposes partial output.
pub struct LightJob {
    target: SectionPos,
    observed: Observation,
    origin: BlockPos,
    dims: [usize; 3],
    absorption: Vec<u8>,
    levels: Vec<u8>,
    queue: BinaryHeap<(u8, u32)>,
    frontier_limit: usize,
    seeded: usize,
    complete: bool,
    failed: bool,
    sky: bool,
    sky_columns: Vec<u8>,
}
impl LightJob {
    pub fn prepare(
        binding: Binding,
        store: &SectionStore,
        registry: &BlockStateRegistry,
        target: SectionPos,
        limits: LightLimits,
    ) -> Result<Self, Fault> {
        Self::prepare_kind(binding, store, registry, target, limits, false)
    }

    /// Skylight seeds vertical columns from the world ceiling. Missing cells
    /// block the column. Lateral propagation still loses at least one level.
    pub fn prepare_sky(
        binding: Binding,
        store: &SectionStore,
        registry: &BlockStateRegistry,
        target: SectionPos,
        limits: LightLimits,
    ) -> Result<Self, Fault> {
        Self::prepare_kind(binding, store, registry, target, limits, true)
    }

    fn prepare_kind(
        binding: Binding,
        store: &SectionStore,
        registry: &BlockStateRegistry,
        target: SectionPos,
        limits: LightLimits,
        sky: bool,
    ) -> Result<Self, Fault> {
        if !readable(store, target) {
            return Err(Fault::invalid_data("lighting target is not loaded"));
        }
        if limits.cells == 0
            || limits.cells > 1_000_000
            || limits.dependencies == 0
            || limits.dependencies > 32768
            || limits.frontier == 0
            || limits.frontier > 2_000_000
        {
            return Err(Fault::invalid_data("invalid lighting limits"));
        }
        let side = i64::from(store.side());
        let base = section_origin(target, side)?;
        let min = base.checked_add(-15, -15, -15).map_err(invalid)?;
        let max = base
            .checked_add(side + 14, side + 14, side + 14)
            .map_err(invalid)?;
        let bounds = store.bounds();
        let origin = BlockPos::new(
            min.x,
            if sky {
                bounds.min_y
            } else {
                min.y.max(bounds.min_y)
            },
            min.z,
        );
        let last = BlockPos::new(
            max.x,
            if sky {
                bounds.max_y - 1
            } else {
                max.y.min(bounds.max_y - 1)
            },
            max.z,
        );
        if origin.y > last.y {
            return Err(Fault::invalid_data(
                "lighting target is outside world bounds",
            ));
        }
        let extent = |last: i64, first: i64| -> Result<usize, Fault> {
            usize::try_from(i128::from(last) - i128::from(first) + 1)
                .map_err(|_| Fault::resource_limit("lighting extent overflow"))
        };
        let dims = [
            extent(last.x, origin.x)?,
            extent(last.y, origin.y)?,
            extent(last.z, origin.z)?,
        ];
        let cells = dims
            .iter()
            .try_fold(1usize, |n, dim| n.checked_mul(*dim))
            .filter(|n| *n <= limits.cells)
            .ok_or_else(|| Fault::resource_limit("lighting cell limit exceeded"))?;
        let first_section = origin.to_section(store.side()).map_err(invalid)?.0;
        let last_section = last.to_section(store.side()).map_err(invalid)?.0;
        let deps = [
            extent(last_section.x, first_section.x)?,
            extent(last_section.y, first_section.y)?,
            extent(last_section.z, first_section.z)?,
        ]
        .iter()
        .try_fold(1usize, |n, dim| n.checked_mul(*dim))
        .filter(|n| *n <= limits.dependencies)
        .ok_or_else(|| Fault::resource_limit("lighting dependency limit exceeded"))?;
        let frontier = limits.frontier.min(cells * 2);
        let bytes = size_of::<Self>()
            + cells * 2
            + frontier * size_of::<(u8, u32)>()
            + deps * size_of::<Dependency>()
            + (store.side() as usize).pow(3)
            + size_of::<LightSection>()
            + if sky { dims[0] * dims[2] } else { 0 };
        if cells > limits.cells || deps > limits.dependencies || bytes as u64 > limits.bytes {
            return Err(Fault::resource_limit(
                "lighting region exceeds its preparation budget",
            ));
        }
        let mut dependencies = Vec::with_capacity(deps);
        for y in first_section.y..=last_section.y {
            for z in first_section.z..=last_section.z {
                for x in first_section.x..=last_section.x {
                    let pos = SectionPos::new(x, y, z);
                    dependencies.push(Dependency {
                        pos,
                        revision: store.revision(pos),
                        readable: readable(store, pos),
                    });
                }
            }
        }
        Ok(Self {
            target,
            observed: Observation {
                binding,
                content: registry.authority_digest(),
                side: store.side(),
                bounds,
                dependencies,
            },
            origin,
            dims,
            absorption: vec![15; cells],
            levels: vec![0; cells],
            queue: BinaryHeap::with_capacity(frontier),
            frontier_limit: frontier,
            seeded: 0,
            complete: true,
            failed: false,
            sky,
            sky_columns: if sky {
                vec![15; dims[0] * dims[2]]
            } else {
                Vec::new()
            },
        })
    }

    /// At most `work` seed cells or frontier pops, plus the bounded dependency check.
    /// Return true only after every seed and every propagation entry is resolved.
    pub fn poll(
        &mut self,
        binding: Binding,
        store: &SectionStore,
        registry: &BlockStateRegistry,
        work: usize,
    ) -> Result<bool, Fault> {
        if self.failed || !self.observed.valid(binding, store, registry) {
            self.failed = true;
            return Err(Fault::invalid_data("lighting observation changed"));
        }
        let result = self.advance(store, registry, work);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn advance(
        &mut self,
        store: &SectionStore,
        registry: &BlockStateRegistry,
        work: usize,
    ) -> Result<bool, Fault> {
        for _ in 0..work {
            if self.seeded < self.levels.len() {
                let index = if self.sky {
                    let layer = self.dims[0] * self.dims[2];
                    (self.dims[1] - 1 - self.seeded / layer) * layer + self.seeded % layer
                } else {
                    self.seeded
                };
                let pos = self.position(index);
                let column = index % (self.dims[0] * self.dims[2]);
                let mut direct_absorption = 15;
                match store.query(pos) {
                    TerrainQuery::NotLoaded => self.complete = false,
                    TerrainQuery::OutOfBounds | TerrainQuery::Exterior => {}
                    TerrainQuery::Ready(state) if state.is_empty() => {
                        self.absorption[index] = 1;
                        direct_absorption = 0;
                    }
                    TerrainQuery::Ready(state) => {
                        let definition = registry
                            .definition(state)
                            .ok_or_else(|| Fault::invalid_data("unregistered lighting state"))?;
                        self.absorption[index] = (definition.light_transport.absorption * 15.0)
                            .ceil()
                            .clamp(1.0, 15.0)
                            as u8;
                        let level = (definition.light_transport.emission * 15.0).round() as u8;
                        direct_absorption =
                            (definition.light_transport.absorption * 15.0).ceil() as u8;
                        self.levels[index] = if self.sky { 0 } else { level };
                        if level > 0 && !self.sky {
                            self.push(level, index)?;
                        }
                    }
                }
                if self.sky {
                    self.sky_columns[column] =
                        self.sky_columns[column].saturating_sub(direct_absorption);
                    let level = self.sky_columns[column];
                    self.levels[index] = level;
                    if level > 0 {
                        self.push(level, index)?;
                    }
                }
                self.seeded += 1;
            } else if let Some((level, index)) = self.queue.pop() {
                let index = index as usize;
                if self.levels[index] != level {
                    continue;
                }
                let [x, y, z] = self.coordinates(index);
                for (axis, positive) in [
                    (0, false),
                    (0, true),
                    (2, false),
                    (2, true),
                    (1, false),
                    (1, true),
                ] {
                    let mut p = [x, y, z];
                    if positive {
                        if p[axis] + 1 == self.dims[axis] {
                            continue;
                        }
                        p[axis] += 1;
                    } else {
                        if p[axis] == 0 {
                            continue;
                        }
                        p[axis] -= 1;
                    }
                    let other = p[0] + self.dims[0] * (p[2] + self.dims[2] * p[1]);
                    let next = level.saturating_sub(self.absorption[other]);
                    if next > self.levels[other] {
                        self.levels[other] = next;
                        self.push(next, other)?;
                    }
                }
            } else {
                return Ok(true);
            }
        }
        Ok(self.seeded == self.levels.len() && self.queue.is_empty())
    }
    fn push(&mut self, level: u8, index: usize) -> Result<(), Fault> {
        if self.queue.len() == self.frontier_limit {
            return Err(Fault::resource_limit("lighting frontier exhausted"));
        }
        self.queue.push((level, index as u32));
        Ok(())
    }
    fn coordinates(&self, index: usize) -> [usize; 3] {
        [
            index % self.dims[0],
            index / (self.dims[0] * self.dims[2]),
            (index / self.dims[0]) % self.dims[2],
        ]
    }
    fn position(&self, index: usize) -> BlockPos {
        let [x, y, z] = self.coordinates(index);
        BlockPos::new(
            self.origin.x + x as i64,
            self.origin.y + y as i64,
            self.origin.z + z as i64,
        )
    }
    pub fn finish(
        self,
        binding: Binding,
        store: &SectionStore,
        registry: &BlockStateRegistry,
    ) -> Result<LightSection, Fault> {
        if self.failed
            || self.seeded != self.levels.len()
            || !self.queue.is_empty()
            || !self.observed.valid(binding, store, registry)
        {
            return Err(Fault::invalid_data("lighting job is incomplete or stale"));
        }
        let side = self.observed.side;
        let base = section_origin(self.target, i64::from(side))?;
        let mut levels = Vec::with_capacity((side as usize).pow(3));
        for y in 0..side {
            for z in 0..side {
                for x in 0..side {
                    let pos = BlockPos::new(
                        base.x + i64::from(x),
                        base.y + i64::from(y),
                        base.z + i64::from(z),
                    );
                    let value = if self.observed.bounds.contains_y(pos.y) {
                        let p = [
                            (pos.x - self.origin.x) as usize,
                            (pos.y - self.origin.y) as usize,
                            (pos.z - self.origin.z) as usize,
                        ];
                        self.levels[p[0] + self.dims[0] * (p[2] + self.dims[2] * p[1])]
                    } else {
                        0
                    };
                    levels.push(value);
                }
            }
        }
        Ok(LightSection {
            target: self.target,
            observed: self.observed,
            levels,
            complete: self.complete,
        })
    }
}

/// Atomic publication of fully calculated sections, with stale-read rejection.
pub struct LightCache {
    binding: Binding,
    entries: BTreeMap<SectionPos, (u64, LightSection)>,
    max_sections: usize,
    max_bytes: usize,
    bytes: usize,
    generation: u64,
    identity: u64,
}
impl LightCache {
    /// Validate once before a bulk mesh-input capture, then borrow the resolved cells.
    pub fn section(
        &self,
        binding: Binding,
        store: &SectionStore,
        registry: &BlockStateRegistry,
        target: SectionPos,
    ) -> Option<&LightSection> {
        let (_, data) = self.entries.get(&target)?;
        (binding == self.binding && data.validate(binding, store, registry)).then_some(data)
    }
    pub fn new(binding: Binding, max_sections: usize, max_bytes: usize) -> Result<Self, Fault> {
        if max_sections == 0 || max_sections > 32768 || max_bytes == 0 {
            return Err(Fault::invalid_data("invalid light cache limits"));
        }
        let identity = next_cache_identity(&NEXT_CACHE)?;
        Ok(Self {
            binding,
            entries: BTreeMap::new(),
            max_sections,
            max_bytes,
            bytes: 0,
            generation: 0,
            identity,
        })
    }
    pub fn allocated_bytes(&self) -> usize {
        self.bytes
    }
    pub fn publish(
        &mut self,
        section: LightSection,
        store: &SectionStore,
        registry: &BlockStateRegistry,
    ) -> Result<LightStamp, Fault> {
        if !section.validate(self.binding, store, registry) {
            return Err(Fault::invalid_data("stale light publication"));
        }
        let previous = self
            .entries
            .get(&section.target)
            .map_or(0, |(_, s)| s.allocated_bytes());
        let bytes = self
            .bytes
            .checked_sub(previous)
            .and_then(|b| b.checked_add(section.allocated_bytes()))
            .ok_or_else(|| Fault::resource_limit("light cache byte overflow"))?;
        if bytes > self.max_bytes || (previous == 0 && self.entries.len() == self.max_sections) {
            return Err(Fault::resource_limit("light cache capacity exhausted"));
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| Fault::resource_limit("light generations exhausted"))?;
        let stamp = LightStamp {
            binding: self.binding,
            target: section.target,
            generation,
            cache: self.identity,
        };
        self.entries.insert(section.target, (generation, section));
        self.bytes = bytes;
        self.generation = generation;
        Ok(stamp)
    }
    pub fn validate_stamp(
        &self,
        stamp: LightStamp,
        binding: Binding,
        store: &SectionStore,
        registry: &BlockStateRegistry,
    ) -> bool {
        self.binding == binding
            && stamp.binding == binding
            && stamp.cache == self.identity
            && self.entries.get(&stamp.target).is_some_and(|(g, s)| {
                *g == stamp.generation && s.validate(binding, store, registry)
            })
    }
    pub fn sample(
        &self,
        binding: Binding,
        store: &SectionStore,
        registry: &BlockStateRegistry,
        pos: BlockPos,
    ) -> LightQuery {
        if !store.bounds().contains_y(pos.y) {
            return LightQuery::OutOfBounds;
        }
        let Ok((section, local)) = pos.to_section(store.side()) else {
            return LightQuery::Pending;
        };
        let Some((_, data)) = self.entries.get(&section) else {
            return LightQuery::Pending;
        };
        if binding != self.binding || !data.validate(binding, store, registry) {
            return LightQuery::Pending;
        }
        let index = torse_world::linear_index(store.side(), local);
        LightQuery::Ready {
            level: data.levels[index],
            complete: data.complete,
        }
    }
    pub fn remove(&mut self, target: SectionPos) -> bool {
        if let Some((_, section)) = self.entries.remove(&target) {
            self.bytes -= section.allocated_bytes();
            true
        } else {
            false
        }
    }
    /// The owner declares whether this cache still has active consumers.
    pub fn memory_resident(&self, subscribed: bool) -> Option<torse_types::MemoryResident> {
        (self.bytes > 0).then_some(torse_types::MemoryResident {
            class: torse_types::MemoryClass::Mesh,
            bytes: self.bytes as u64,
            rank: if subscribed {
                torse_types::EvictionRank::Pinned
            } else {
                torse_types::EvictionRank::UnsubscribedDerived
            },
        })
    }
}
fn section_origin(pos: SectionPos, side: i64) -> Result<BlockPos, Fault> {
    Ok(BlockPos::new(
        pos.x
            .checked_mul(side)
            .ok_or_else(|| invalid("lighting coordinate overflow"))?,
        pos.y
            .checked_mul(side)
            .ok_or_else(|| invalid("lighting coordinate overflow"))?,
        pos.z
            .checked_mul(side)
            .ok_or_else(|| invalid("lighting coordinate overflow"))?,
    ))
}
fn readable(store: &SectionStore, pos: SectionPos) -> bool {
    matches!(
        store.phase(pos),
        Some(SectionPhase::Ready | SectionPhase::Active)
    )
}
fn invalid(error: impl std::fmt::Display) -> Fault {
    Fault::invalid_data(error.to_string())
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    #[test]
    fn identity_exhaustion_does_not_wrap_or_change_the_counter() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(next_cache_identity(&counter).unwrap(), u64::MAX - 1);
        assert!(next_cache_identity(&counter).is_err());
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }
    #[test]
    fn concurrent_identity_allocation_is_unique_and_consecutive() {
        let counter = AtomicU64::new(1);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        (0..100)
                            .map(|_| next_cache_identity(&counter).unwrap())
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            let mut identities: Vec<_> = workers
                .into_iter()
                .flat_map(|worker| worker.join().unwrap())
                .collect();
            identities.sort_unstable();
            assert_eq!(identities, (1..=800).collect::<Vec<_>>());
        });
        assert_eq!(counter.load(Ordering::Relaxed), 801);
    }
}
