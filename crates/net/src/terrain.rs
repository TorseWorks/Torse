//! Independent, bounded display terrain. This state grants no simulation access.
use crate::ReplicaMirror;
use std::collections::{BTreeMap, BTreeSet};
use torse_sim::replica::{ReplicaChanges, ReplicaSnapshot, WorldReplicaSeed};
use torse_types::*;
use torse_world::{SnapshotCells, WorldBounds};

#[derive(Clone, Copy, Debug)]
pub struct TerrainLimits {
    pub max_sections: usize,
    pub max_bytes: u64,
}
#[derive(Debug)]
pub struct TerrainChanges {
    pub changed: Vec<SectionPos>,
    pub removed: Vec<SectionPos>,
}
pub struct TerrainMirror {
    binding: Binding,
    side: u32,
    bounds: WorldBounds,
    limits: TerrainLimits,
    generation: u64,
    seq: u64,
    through: CommitCursor,
    desired: BTreeSet<SectionPos>,
    pages: BTreeMap<SectionPos, TerrainPage<ReplicaSnapshot>>,
    floors: BTreeMap<SectionPos, CommitSeq>,
    bytes: u64,
    _memory: Option<MemoryLease>,
}
fn invalid() -> Fault {
    Fault::new(
        FaultCode::ResetRequired,
        "invalid or stale display terrain stream",
    )
}
fn sorted<T: Ord>(values: impl IntoIterator<Item = T>) -> bool {
    let mut last = None;
    for value in values {
        if last.as_ref().is_some_and(|old| old >= &value) {
            return false;
        }
        last = Some(value);
    }
    true
}
/// Conservative logical payload charge; excludes native allocator capacity/RSS.
pub fn terrain_bytes(snapshot: &ReplicaSnapshot) -> u64 {
    let mut bytes = 4096u64;
    for entry in &snapshot.states {
        bytes = bytes
            .saturating_add(128)
            .saturating_add((entry.block.namespace().len() + entry.block.path().len()) as u64)
            .saturating_add(entry.values.len() as u64 * 2);
    }
    for section in &snapshot.sections {
        bytes = bytes.saturating_add(128);
        if let SnapshotCells::Dense(cells) = &section.cells {
            bytes = bytes.saturating_add(cells.len() as u64 * 2);
        }
    }
    bytes
}
impl TerrainMirror {
    pub fn new(
        binding: Binding,
        side: u32,
        bounds: WorldBounds,
        limits: TerrainLimits,
        memory: Option<MemoryPool>,
    ) -> Result<Self, Fault> {
        if limits.max_sections == 0 || limits.max_sections > 16384 || limits.max_bytes < 4096 {
            return Err(Fault::resource_limit("display terrain limits"));
        }
        ReplicaMirror::from_seed(WorldReplicaSeed {
            binding,
            subscription: SubscriptionId(1),
            baseline: CommitCursor::initial(binding.world),
            state: ReplicaSnapshot {
                side,
                bounds,
                states: Vec::new(),
                sections: Vec::new(),
                entities: Vec::new(),
            },
        })?;
        let lease = memory
            .map(|p| p.reserve(MemoryClass::Replica, limits.max_bytes, EvictionRank::Pinned))
            .transpose()?;
        Ok(Self {
            binding,
            side,
            bounds,
            limits,
            generation: 0,
            seq: 0,
            through: CommitCursor::initial(binding.world),
            desired: BTreeSet::new(),
            pages: BTreeMap::new(),
            floors: BTreeMap::new(),
            bytes: 0,
            _memory: lease,
        })
    }
    pub fn desired(&self) -> &BTreeSet<SectionPos> {
        &self.desired
    }
    pub fn accounted_bytes(&self) -> u64 {
        self.bytes
    }
    pub fn page(&self, pos: SectionPos) -> Option<&ReplicaSnapshot> {
        self.pages
            .get(&pos)
            .filter(|p| self.floors.get(&pos).is_none_or(|floor| p.at.seq >= *floor))
            .map(|p| &p.terrain)
    }
    pub fn pages(&self) -> impl Iterator<Item = &ReplicaSnapshot> {
        self.pages
            .iter()
            .filter(|(pos, p)| self.floors.get(pos).is_none_or(|floor| p.at.seq >= *floor))
            .map(|(_, p)| &p.terrain)
    }
    /// A delayed far response cannot overwrite newer near terrain. Keep its
    /// content hidden until a sufficiently fresh page or retained-content proof.
    pub fn observe_authority(&mut self, changes: &ReplicaChanges, at: CommitCursor) {
        for pos in changes
            .sections
            .iter()
            .map(|p| p.pos)
            .chain(changes.removed_sections.iter().copied())
        {
            if self.desired.contains(&pos) {
                let floor = self.floors.entry(pos).or_insert(at.seq);
                *floor = (*floor).max(at.seq);
            }
        }
    }
    pub fn observe_snapshot(&mut self, snapshot: &ReplicaSnapshot, at: CommitCursor) {
        for section in &snapshot.sections {
            if self.desired.contains(&section.pos) {
                let floor = self.floors.entry(section.pos).or_insert(at.seq);
                *floor = (*floor).max(at.seq);
            }
        }
    }
    pub fn apply(
        &mut self,
        update: TerrainUpdate<ReplicaSnapshot>,
    ) -> Result<TerrainChanges, Fault> {
        if update.binding != self.binding
            || update.through.world != self.binding.world
            || update.through.seq < self.through.seq
            || update.generation == 0
            || match &update.reset {
                Some(_) => update.generation <= self.generation || update.seq != 1,
                None => {
                    update.generation != self.generation
                        || update.seq != self.seq.checked_add(1).ok_or_else(invalid)?
                }
            }
        {
            return Err(invalid());
        }
        let (desired, retained) = if let Some(reset) = &update.reset {
            if reset.sections.len() > self.limits.max_sections
                || !sorted(reset.sections.iter().copied())
                || !sorted(reset.retained.iter().map(|(p, _)| *p))
            {
                return Err(invalid());
            }
            let desired: BTreeSet<_> = reset.sections.iter().copied().collect();
            let retained: BTreeMap<_, _> = reset.retained.iter().copied().collect();
            if retained.iter().any(|(pos, digest)| {
                !desired.contains(pos) || self.pages.get(pos).is_none_or(|p| p.digest != *digest)
            }) {
                return Err(invalid());
            }
            (desired, retained)
        } else {
            (self.desired.clone(), BTreeMap::new())
        };
        if update.pages.len() > self.limits.max_sections
            || update.unavailable.len() > self.limits.max_sections
            || update.pages.iter().any(|p| p.terrain.sections.len() != 1)
            || !sorted(update.pages.iter().map(|p| p.terrain.sections[0].pos))
            || !sorted(update.unavailable.iter().map(|(p, _)| *p))
        {
            return Err(invalid());
        }
        let unavailable: BTreeSet<_> = update.unavailable.iter().map(|(p, _)| *p).collect();
        if unavailable
            .iter()
            .any(|p| !desired.contains(p) || retained.contains_key(p))
        {
            return Err(invalid());
        }
        let mut incoming = BTreeMap::new();
        let mut input_bytes = 0u64;
        for page in update.pages {
            let pos = page.terrain.sections[0].pos;
            input_bytes = input_bytes.saturating_add(terrain_bytes(&page.terrain));
            if input_bytes > self.limits.max_bytes {
                return Err(Fault::resource_limit("display terrain input budget"));
            }
            if !desired.contains(&pos)
                || unavailable.contains(&pos)
                || retained.contains_key(&pos)
                || page.at.world != self.binding.world
                || page.at.seq > update.through.seq
                || page.terrain.side != self.side
                || page.terrain.bounds != self.bounds
                || !page.terrain.entities.is_empty()
            {
                return Err(invalid());
            }
            let checked = ReplicaMirror::from_seed(WorldReplicaSeed {
                binding: self.binding,
                subscription: SubscriptionId(1),
                baseline: page.at,
                state: page.terrain,
            })?;
            let terrain = checked.into_state();
            if self
                .floors
                .get(&pos)
                .is_some_and(|floor| page.at.seq < *floor)
                || (update.reset.is_none()
                    && self
                        .pages
                        .get(&pos)
                        .is_some_and(|old| old.at.seq > page.at.seq))
            {
                continue;
            }
            incoming.insert(pos, TerrainPage { terrain, ..page });
        }
        let keep = |pos: &SectionPos| {
            desired.contains(pos)
                && !unavailable.contains(pos)
                && !incoming.contains_key(pos)
                && (update.reset.is_none() || retained.contains_key(pos))
        };
        let bytes = self
            .pages
            .iter()
            .filter(|(pos, _)| keep(pos))
            .fold(0u64, |n, (_, p)| {
                n.saturating_add(terrain_bytes(&p.terrain))
            })
            .saturating_add(
                incoming
                    .values()
                    .fold(0u64, |n, p| n.saturating_add(terrain_bytes(&p.terrain))),
            )
            .saturating_add(desired.len() as u64 * 128);
        if bytes > self.limits.max_bytes {
            return Err(Fault::resource_limit("display terrain cache budget"));
        }
        let mut changes = TerrainChanges {
            changed: incoming.keys().copied().collect(),
            removed: self
                .pages
                .keys()
                .filter(|p| !keep(p) && !incoming.contains_key(p))
                .copied()
                .collect(),
        };
        self.pages.retain(|pos, _| keep(pos));
        for pos in retained.keys() {
            let page = self.pages.get_mut(pos).expect("checked retained page");
            if self
                .floors
                .get(pos)
                .is_some_and(|floor| page.at.seq < *floor && update.through.seq >= *floor)
            {
                changes.changed.push(*pos);
            }
            page.at = update.through;
        }
        self.pages.extend(incoming);
        self.desired = desired;
        self.floors.retain(|p, _| self.desired.contains(p));
        self.bytes = bytes;
        self.generation = update.generation;
        self.seq = update.seq;
        self.through = update.through;
        changes.changed.sort_unstable();
        changes.changed.dedup();
        changes.removed.sort_unstable();
        Ok(changes)
    }
}
