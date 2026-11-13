//! Retained solver state for bounded increase/decrease relaxation after edits.
use super::*;

pub(super) struct Incremental {
    pub(super) seeds: Vec<u8>,
    pub(super) direct: Vec<u8>,
    queued: Vec<u8>,
    dirty: Vec<SectionPos>,
    pub(super) scan: Option<usize>,
    visits: u64,
}
impl LightJob {
    /// Opt in to retaining a solver. Additional buffers and dependency metadata
    /// must fit the same job byte budget; ordinary one-shot jobs allocate none.
    pub fn reusable(mut self, max_bytes: u64) -> Result<Self, Fault> {
        if self.seeded != 0 || self.incremental.is_some() {
            return Err(Fault::invalid_data(
                "lighting reuse must be enabled before polling",
            ));
        }
        let cells = self.levels.len();
        let extra = cells
            .checked_mul(3)
            .and_then(|n| n.checked_add(self.observed.dependencies.len() * size_of::<SectionPos>()))
            .and_then(|n| n.checked_add(size_of::<Incremental>()))
            .ok_or_else(|| Fault::resource_limit("incremental lighting buffers"))?;
        let peak = self.allocated_bytes().checked_add(extra).and_then(|n| {
            n.checked_add(
                (self.observed.side as usize).pow(3)
                    + size_of::<LightSection>()
                    + self.observed.bytes(),
            )
        });
        if peak.is_none_or(|n| n as u64 > max_bytes) {
            return Err(Fault::resource_limit("incremental lighting byte budget"));
        }
        self.incremental = Some(Incremental {
            seeds: vec![0; cells],
            direct: vec![15; cells],
            queued: vec![0; cells],
            dirty: Vec::with_capacity(self.observed.dependencies.len()),
            scan: None,
            visits: 0,
        });
        Ok(self)
    }
    pub fn allocated_bytes(&self) -> usize {
        size_of::<Self>()
            + self.levels.capacity()
            + self.absorption.capacity()
            + self.sky_columns.capacity()
            + self.queue.capacity() * size_of::<(u8, u32)>()
            + self.observed.bytes()
            + self.incremental.as_ref().map_or(0, |i| {
                size_of::<Incremental>()
                    + i.seeds.capacity()
                    + i.direct.capacity()
                    + i.queued.capacity()
                    + i.dirty.capacity() * size_of::<SectionPos>()
            })
    }
    /// Rebind completed state to a new content observation. Changed cells are
    /// scanned in budgeted polls; only changed seeds and their affected neighbors
    /// enter the relaxation frontier. Rules/shape changes require a cold job.
    pub fn update(
        &mut self,
        binding: Binding,
        source: &(impl LightSource + ?Sized),
        registry: &BlockStateRegistry,
    ) -> Result<(), Fault> {
        if self.failed
            || self.seeded != self.levels.len()
            || !self.queue.is_empty()
            || self.observed.binding != binding
            || source.binding().is_some_and(|b| b != binding)
            || self.observed.content != registry.authority_digest()
            || self.observed.rules != source.rules_version()
            || self.observed.side != source.side()
            || self.observed.bounds != source.bounds()
        {
            return Err(Fault::invalid_data("lighting state cannot be updated"));
        }
        let state = self
            .incremental
            .as_mut()
            .ok_or_else(|| Fault::invalid_data("lighting state was not retained"))?;
        if state.scan.is_some() {
            return Err(Fault::invalid_data("lighting update already pending"));
        }
        state.dirty.clear();
        for dependency in &mut self.observed.dependencies {
            let version = source.version(dependency.pos);
            if version != dependency.version {
                state.dirty.push(dependency.pos);
                dependency.version = version;
            }
        }
        state.dirty.sort_unstable();
        state.visits = 0;
        if !state.dirty.is_empty() {
            state.scan = Some(0);
            self.sky_columns.fill(15);
            self.complete = self.observed.dependencies.iter().all(|d| {
                matches!(
                    d.version,
                    LightInputVersion::Content(_)
                        | LightInputVersion::Boundary
                        | LightInputVersion::Resident { readable: true, .. }
                )
            });
        }
        Ok(())
    }
    pub fn update_frontier_visits(&self) -> u64 {
        self.incremental.as_ref().map_or(0, |i| i.visits)
    }
    pub(super) fn advance_update(
        &mut self,
        source: &(impl LightSource + ?Sized),
        registry: &BlockStateRegistry,
        work: usize,
    ) -> Result<bool, Fault> {
        for _ in 0..work {
            let scan = self
                .incremental
                .as_ref()
                .unwrap()
                .scan
                .expect("active update scan");
            if scan < self.levels.len() {
                let layer = self.dims[0] * self.dims[2];
                let index = if self.sky {
                    (self.dims[1] - 1 - scan / layer) * layer + scan % layer
                } else {
                    scan
                };
                let pos = self.position(index);
                let section = pos.to_section(self.observed.side).map_err(invalid)?.0;
                let changed = self
                    .incremental
                    .as_ref()
                    .unwrap()
                    .dirty
                    .binary_search(&section)
                    .is_ok();
                if changed {
                    let (absorption, direct, emission) = match source.query(pos) {
                        TerrainQuery::Ready(id) if id.is_empty() => (1, 0, 0),
                        TerrainQuery::Ready(id) => {
                            let transport = registry.light_transport(id).ok_or_else(|| {
                                Fault::invalid_data("unknown updated light state")
                            })?;
                            (
                                (transport.absorption * 15.0).ceil().clamp(1.0, 15.0) as u8,
                                (transport.absorption * 15.0).ceil() as u8,
                                (transport.emission * 15.0).round() as u8,
                            )
                        }
                        _ => (15, 15, 0),
                    };
                    let state = self.incremental.as_mut().unwrap();
                    state.direct[index] = direct;
                    if !self.sky && state.seeds[index] != emission {
                        state.seeds[index] = emission;
                        self.enqueue_update(index)?;
                    }
                    if self.absorption[index] != absorption {
                        self.absorption[index] = absorption;
                        self.enqueue_update(index)?;
                    }
                }
                if self.sky {
                    let column = index % layer;
                    let state = self.incremental.as_mut().unwrap();
                    self.sky_columns[column] =
                        self.sky_columns[column].saturating_sub(state.direct[index]);
                    let seed = self.sky_columns[column];
                    if seed != state.seeds[index] {
                        state.seeds[index] = seed;
                        self.enqueue_update(index)?;
                    }
                }
                self.incremental.as_mut().unwrap().scan = Some(scan + 1);
            } else if let Some((_, index)) = self.queue.pop() {
                let index = index as usize;
                let state = self.incremental.as_mut().unwrap();
                state.queued[index] = 0;
                state.visits = state.visits.saturating_add(1);
                let mut level = state.seeds[index];
                let neighbors = self.neighbors(index);
                for neighbor in neighbors.into_iter().flatten() {
                    level = level.max(self.levels[neighbor].saturating_sub(self.absorption[index]));
                }
                if level != self.levels[index] {
                    self.levels[index] = level;
                    for neighbor in neighbors.into_iter().flatten() {
                        self.enqueue_update(neighbor)?;
                    }
                }
            } else {
                self.incremental.as_mut().unwrap().scan = None;
                return Ok(true);
            }
        }
        // Leave scan present until the final frontier has been observed empty.
        Ok(false)
    }
    fn enqueue_update(&mut self, index: usize) -> Result<(), Fault> {
        if self.incremental.as_ref().unwrap().queued[index] == 0 {
            self.push(0, index)?;
            self.incremental.as_mut().unwrap().queued[index] = 1;
        }
        Ok(())
    }
    fn neighbors(&self, index: usize) -> [Option<usize>; 6] {
        let [x, y, z] = self.coordinates(index);
        let layer = self.dims[0] * self.dims[2];
        [
            (x > 0).then(|| index - 1),
            (x + 1 < self.dims[0]).then(|| index + 1),
            (z > 0).then(|| index - self.dims[0]),
            (z + 1 < self.dims[2]).then(|| index + self.dims[0]),
            (y > 0).then(|| index - layer),
            (y + 1 < self.dims[1]).then(|| index + layer),
        ]
    }
}
