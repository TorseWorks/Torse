//! Read-only display pages. Disk I/O never installs authoritative sections.
use crate::{HostAuthority, execution::ExecutionHost, section_pages::SectionPages};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::mpsc,
    thread,
};
use torse_codec::wire;
use torse_net::{ReplicaMirror, terrain::terrain_bytes};
use torse_sim::replica::{ReplicaScope, ReplicaSnapshot, WorldReplicaSeed};
use torse_types::*;
use torse_world::{Snapshot, SnapshotCells, SnapshotSection, StateEntry};

mod approval;
pub use approval::VisualTerrainApproval;

const MAX_VISUAL_SECTIONS: usize = 16_384;

#[derive(Clone, Copy, Debug)]
pub struct VisualTerrainLimits {
    /// Maximum pages in the current desired window.
    pub max_sections: usize,
    pub cache_bytes: u64,
    pub io_bytes: u64,
    pub batch_bytes: u64,
    pub pages_per_poll: usize,
}
#[derive(Clone, Copy, Debug)]
pub struct VisualTerrainDiagnostics {
    pub cache_pages: usize,
    pub cache_bytes: u64,
    /// Fixed logical reservation including I/O and output work, not RSS.
    pub reserved_bytes: u64,
    pub io_pending: bool,
    pub generation: u64,
    pub sequence: u64,
}
impl VisualTerrainLimits {
    fn validate(self, pages: &SectionPages) -> Result<u64, Fault> {
        if self.max_sections == 0
            || self.max_sections > MAX_VISUAL_SECTIONS
            || self.pages_per_poll == 0
            || self.pages_per_poll > 64
            || self.cache_bytes < 4096
            || self.batch_bytes < 4096
            || self.io_bytes < pages.budget().max_bytes.saturating_mul(4)
        {
            return Err(Fault::resource_limit("visual terrain limits"));
        }
        self.cache_bytes
            .checked_add(self.io_bytes)
            .and_then(|n| n.checked_add(self.batch_bytes.checked_mul(3)?))
            .and_then(|n| n.checked_add(self.max_sections as u64 * 1024))
            .ok_or_else(|| Fault::resource_limit("visual terrain reservation overflow"))
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stamp {
    Resident(Revision, Option<Revision>),
    Parked(Digest),
    Missing,
}
fn stamp(host: &ExecutionHost, pos: SectionPos) -> Stamp {
    let store = host.world().store();
    if store.section(pos).is_some() {
        Stamp::Resident(
            store.revision(pos).expect("resident revision"),
            store.retired_revision(pos),
        )
    } else if let Some(page) = host.parked_sections().get(&pos) {
        Stamp::Parked(page.digest)
    } else {
        Stamp::Missing
    }
}
struct Cached {
    stamp: Stamp,
    page: ReplicaSnapshot,
    digest: Digest,
    touched: u64,
}
fn distance(pos: SectionPos, center: SectionPos) -> u128 {
    u128::from(pos.x.abs_diff(center.x))
        + u128::from(pos.y.abs_diff(center.y))
        + u128::from(pos.z.abs_diff(center.z))
}
enum Read {
    Saved(Digest),
    Resident(ReplicaSnapshot),
}
struct Item {
    pos: SectionPos,
    stamp: Stamp,
    read: Read,
}
struct Job {
    generation: u64,
    items: Vec<Item>,
}
struct Prepared {
    pos: SectionPos,
    stamp: Stamp,
    result: Result<(ReplicaSnapshot, Digest), TerrainUnavailable>,
}
struct Response {
    generation: u64,
    pages: Vec<Prepared>,
}
struct Worker {
    sender: Option<mpsc::SyncSender<Job>>,
    receiver: mpsc::Receiver<Response>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn clip(snapshot: Snapshot) -> ReplicaSnapshot {
    let mut used = BTreeSet::new();
    for section in &snapshot.sections {
        match &section.cells {
            SnapshotCells::Uniform(id) => {
                used.insert(*id);
            }
            SnapshotCells::Dense(ids) => {
                used.extend(ids.iter().copied());
            }
        }
    }
    ReplicaSnapshot {
        side: snapshot.side,
        bounds: snapshot.bounds,
        states: snapshot
            .states
            .into_iter()
            .filter(|s| used.contains(&s.id))
            .collect(),
        sections: snapshot.sections,
        entities: Vec::new(),
    }
}
fn digest(
    page: &ReplicaSnapshot,
    pages: &SectionPages,
    budget: u64,
) -> Result<Digest, TerrainUnavailable> {
    let mut bytes = Vec::new();
    wire::encode(
        page,
        &mut bytes,
        OutputBudget {
            max_bytes: budget.min(pages.budget().max_bytes),
            ..pages.budget()
        },
        pages.codec_limits(),
    )
    .map_err(|e| {
        if e.code == FaultCode::ResourceLimit {
            TerrainUnavailable::Budget
        } else {
            TerrainUnavailable::Corrupt
        }
    })?;
    Ok(Digest(Sha256::digest(bytes).into()))
}
fn resident(
    host: &ExecutionHost,
    pos: SectionPos,
    budget: u64,
) -> Result<ReplicaSnapshot, TerrainUnavailable> {
    let world = host.world();
    let section = world
        .store()
        .section(pos)
        .ok_or(TerrainUnavailable::Missing)?;
    let mut used = BTreeSet::new();
    match section.storage() {
        torse_world::SectionStorage::Uniform(id) => {
            used.insert(*id);
        }
        torse_world::SectionStorage::Dense(ids) => {
            used.extend(ids.iter().copied());
        }
        torse_world::SectionStorage::Palette(ids) => {
            used.extend(ids.states());
        }
    }
    used.remove(&torse_world::BlockStateId::EMPTY);
    let mut bytes = 4096u64 + (world.store().side() as u64).pow(3) * 2;
    for id in &used {
        let key = world
            .registry()
            .definition_key(*id)
            .ok_or(TerrainUnavailable::Corrupt)?;
        bytes = bytes.saturating_add(
            128 + (key.namespace().len() + key.path().len()) as u64
                + world.registry().state_values(*id).unwrap_or(&[]).len() as u64 * 2,
        );
    }
    if bytes > budget {
        return Err(TerrainUnavailable::Budget);
    }
    let states = used
        .into_iter()
        .map(|id| StateEntry {
            id,
            block: world
                .registry()
                .definition_key(id)
                .expect("preflight")
                .clone(),
            values: world.registry().state_values(id).unwrap_or(&[]).to_vec(),
        })
        .collect();
    Ok(ReplicaSnapshot {
        side: world.store().side(),
        bounds: world.store().bounds(),
        states,
        sections: vec![SnapshotSection {
            pos,
            cells: SnapshotCells::capture(section),
        }],
        entities: Vec::new(),
    })
}

pub(crate) struct VisualSession {
    binding: Binding,
    approved: VisualTerrainApproval,
    desired: ReplicaScope,
    center: SectionPos,
    limits: VisualTerrainLimits,
    worker: Worker,
    busy: bool,
    generation: u64,
    seq: u64,
    reset: bool,
    cache: BTreeMap<SectionPos, Cached>,
    bytes: u64,
    clock: u64,
    dirty: BTreeSet<SectionPos>,
    unavailable: BTreeMap<SectionPos, TerrainUnavailable>,
    observed: BTreeMap<SectionPos, Stamp>,
    sent: BTreeMap<SectionPos, Digest>,
    light_sent: BTreeMap<SectionPos, u64>,
    light_seq: u64,
    reserved_bytes: u64,
    _memory: Option<MemoryLease>,
}
impl VisualSession {
    pub(crate) fn desired_positions(&self) -> Vec<SectionPos> {
        self.desired.sections.iter().copied().collect()
    }
    pub(crate) fn check_request(&self, desired: &ReplicaScope) -> Result<(), Fault> {
        self.approved
            .check_desired(desired, self.limits.max_sections)?;
        self.generation
            .checked_add(1)
            .ok_or_else(|| Fault::resource_limit("visual generation exhausted"))?;
        Ok(())
    }
    fn light_page(
        &self,
        id: u128,
        field: &crate::lighting::HostLightField,
        host: &ExecutionHost,
        pos: SectionPos,
    ) -> Option<TerrainLight> {
        let cached = self.cache.get(&pos)?;
        if cached.stamp != stamp(host, pos) {
            return None;
        }
        let light = field.publication(id, pos, host)?;
        Some(TerrainLight {
            pos,
            terrain: cached.digest,
            generation: light.generation,
            rules: light.rules_version,
            complete: light.emitted.is_complete() && light.sky.is_complete(),
            emitted: light.emitted.levels().to_vec(),
            sky: light.sky.levels().to_vec(),
        })
    }
    pub(crate) fn attach_lights(
        &mut self,
        id: u128,
        field: &crate::lighting::HostLightField,
        host: &ExecutionHost,
        update: &mut TerrainUpdate<ReplicaSnapshot>,
    ) {
        let mut bytes = update
            .pages
            .iter()
            .map(|p| terrain_bytes(&p.terrain))
            .sum::<u64>();
        for page in &mut update.pages {
            let pos = page.terrain.sections[0].pos;
            let Some(publication) = field.publication(id, pos, host) else {
                continue;
            };
            let cost = 128
                + publication.emitted.levels().len() as u64
                + publication.sky.levels().len() as u64;
            if bytes.saturating_add(cost) > self.limits.batch_bytes {
                continue;
            }
            let Some(light) = self.light_page(id, field, host, pos) else {
                continue;
            };
            bytes += cost;
            self.light_sent.insert(pos, light.generation);
            page.light = Some(light);
        }
    }
    pub(crate) fn light_update(
        &mut self,
        id: u128,
        field: &crate::lighting::HostLightField,
        host: &ExecutionHost,
    ) -> Result<Option<TerrainLightUpdate>, Fault> {
        if self.reset {
            return Ok(None);
        }
        let mut pages = Vec::new();
        let mut bytes = 0u64;
        for &pos in &self.desired.sections {
            let Some(cached) = self.cache.get(&pos) else {
                continue;
            };
            if self.sent.get(&pos) != Some(&cached.digest) {
                continue;
            }
            let Some(publication) = field.publication(id, pos, host) else {
                continue;
            };
            if self.light_sent.get(&pos) == Some(&publication.generation) {
                continue;
            }
            let cost = 128
                + publication.emitted.levels().len() as u64
                + publication.sky.levels().len() as u64;
            if pages.len() == self.limits.pages_per_poll
                || bytes.saturating_add(cost) > self.limits.batch_bytes
            {
                break;
            }
            if let Some(light) = self.light_page(id, field, host, pos) {
                bytes += cost;
                pages.push(light);
            }
        }
        if pages.is_empty() {
            return Ok(None);
        }
        let seq = self
            .light_seq
            .checked_add(1)
            .ok_or_else(|| Fault::resource_limit("visual light sequence"))?;
        for light in &pages {
            self.light_sent.insert(light.pos, light.generation);
        }
        self.light_seq = seq;
        Ok(Some(TerrainLightUpdate {
            binding: self.binding,
            interest_generation: self.generation,
            seq,
            pages,
        }))
    }
    pub(crate) fn new(
        host: &ExecutionHost,
        pages: SectionPages,
        approved: VisualTerrainApproval,
        desired: ReplicaScope,
        center: SectionPos,
        limits: VisualTerrainLimits,
    ) -> Result<Self, Fault> {
        let charge = limits.validate(&pages)?;
        approved.check_desired(&desired, limits.max_sections)?;
        let charge = charge
            .checked_add(approved.reservation_bytes()?)
            .ok_or_else(|| Fault::resource_limit("visual approval reservation"))?;
        let memory = host
            .memory_pool()
            .map(|p| p.reserve(MemoryClass::Replica, charge, EvictionRank::Pinned))
            .transpose()?;
        let binding = host.binding();
        let side = host.world().store().side();
        let bounds = host.world().store().bounds();
        let (sender, jobs) = mpsc::sync_channel::<Job>(1);
        let (results, receiver) = mpsc::sync_channel(1);
        let handle = thread::Builder::new()
            .name("torse-visual-pages".into())
            .spawn(move || {
                while let Ok(job) = jobs.recv() {
                    let mut prepared = Vec::new();
                    let mut bytes = 0u64;
                    for item in job.items {
                        let page = match item.read {
                            Read::Resident(page) => Ok(page),
                            Read::Saved(hash) => pages
                                .get(hash)
                                .and_then(|page| {
                                    if page.meta.cursor.world != binding.world
                                        || page.meta.compatibility != binding.compatibility
                                    {
                                        return Err(Fault::invalid_data(
                                            "visual page belongs to another world",
                                        ));
                                    }
                                    Ok(clip(page))
                                })
                                .map_err(|e| {
                                    if e.code == FaultCode::ResourceLimit {
                                        TerrainUnavailable::Budget
                                    } else {
                                        TerrainUnavailable::Corrupt
                                    }
                                }),
                        };
                        let result = page.and_then(|page| {
                            if page.sections.len() != 1
                                || page.sections[0].pos != item.pos
                                || page.side != side
                                || page.bounds != bounds
                            {
                                return Err(TerrainUnavailable::Corrupt);
                            }
                            let page = ReplicaMirror::from_seed(WorldReplicaSeed {
                                binding,
                                subscription: SubscriptionId(1),
                                baseline: CommitCursor::initial(binding.world),
                                state: page,
                            })
                            .map_err(|_| TerrainUnavailable::Corrupt)?
                            .into_state();
                            let hash = digest(&page, &pages, limits.batch_bytes)?;
                            Ok((page, hash))
                        });
                        if let Ok((page, _)) = &result {
                            let cost = terrain_bytes(page);
                            if cost > limits.batch_bytes || cost > limits.cache_bytes {
                                prepared.push(Prepared {
                                    pos: item.pos,
                                    stamp: item.stamp,
                                    result: Err(TerrainUnavailable::Budget),
                                });
                                continue;
                            }
                            if bytes.saturating_add(cost) > limits.batch_bytes {
                                break;
                            }
                            bytes += cost;
                        }
                        prepared.push(Prepared {
                            pos: item.pos,
                            stamp: item.stamp,
                            result,
                        });
                    }
                    if results
                        .send(Response {
                            generation: job.generation,
                            pages: prepared,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|e| {
                Fault::new(
                    FaultCode::StorageUnavailable,
                    format!("visual worker startup: {e}"),
                )
            })?;
        Ok(Self {
            binding,
            approved,
            desired,
            center,
            limits,
            worker: Worker {
                sender: Some(sender),
                receiver,
                handle: Some(handle),
            },
            busy: false,
            generation: 1,
            seq: 0,
            reset: true,
            cache: BTreeMap::new(),
            bytes: 0,
            clock: 0,
            dirty: BTreeSet::new(),
            unavailable: BTreeMap::new(),
            observed: BTreeMap::new(),
            sent: BTreeMap::new(),
            light_sent: BTreeMap::new(),
            light_seq: 0,
            reserved_bytes: charge,
            _memory: memory,
        })
    }
    pub(crate) fn request(
        &mut self,
        desired: ReplicaScope,
        center: SectionPos,
        refresh: bool,
    ) -> Result<(), Fault> {
        self.approved
            .check_desired(&desired, self.limits.max_sections)?;
        self.center = center;
        if !refresh && desired == self.desired {
            return Ok(());
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| Fault::resource_limit("visual generation exhausted"))?;
        self.seq = 0;
        self.light_seq = 0;
        self.light_sent.clear();
        self.reset = true;
        self.desired = desired;
        self.dirty.clear();
        self.unavailable.clear();
        self.observed.clear();
        self.sent.retain(|p, _| self.desired.sections.contains(p));
        Ok(())
    }
    fn remove(&mut self, pos: SectionPos) {
        if let Some(old) = self.cache.remove(&pos) {
            self.bytes = self.bytes.saturating_sub(terrain_bytes(&old.page));
        }
        self.dirty.remove(&pos);
        self.sent.remove(&pos);
        self.light_sent.remove(&pos);
    }
    pub(crate) fn refresh(&mut self) -> Result<(), Fault> {
        self.request(self.desired.clone(), self.center, true)
    }
    pub(crate) fn diagnostics(&self) -> VisualTerrainDiagnostics {
        VisualTerrainDiagnostics {
            cache_pages: self.cache.len(),
            cache_bytes: self.bytes,
            reserved_bytes: self.reserved_bytes,
            io_pending: self.busy,
            generation: self.generation,
            sequence: self.seq,
        }
    }
    pub(crate) fn poll(
        &mut self,
        host: &ExecutionHost,
        can_send: bool,
    ) -> Result<Option<TerrainUpdate<ReplicaSnapshot>>, Fault> {
        if host.binding() != self.binding {
            return Err(Fault::new(
                FaultCode::ResetRequired,
                "visual world binding changed",
            ));
        }
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or_else(|| Fault::resource_limit("visual cache clock exhausted"))?;
        if self.busy {
            match self.worker.receiver.try_recv() {
                Ok(response) => {
                    self.busy = false;
                    if response.generation == self.generation {
                        for page in response.pages {
                            if !self.desired.sections.contains(&page.pos)
                                || stamp(host, page.pos) != page.stamp
                            {
                                continue;
                            }
                            self.observed.insert(page.pos, page.stamp);
                            match page.result {
                                Ok((terrain, digest)) => {
                                    if terrain.states.iter().any(|s| {
                                        host.world()
                                            .registry()
                                            .state_of(&s.block, &s.values)
                                            .is_none()
                                    }) {
                                        self.unavailable
                                            .insert(page.pos, TerrainUnavailable::Corrupt);
                                        continue;
                                    }
                                    self.remove(page.pos);
                                    let cost = terrain_bytes(&terrain);
                                    while self.bytes.saturating_add(cost) > self.limits.cache_bytes
                                        || self.cache.len() >= self.limits.max_sections
                                    {
                                        let victim = self
                                            .cache
                                            .iter()
                                            .filter(|(pos, _)| !self.dirty.contains(pos))
                                            .filter(|(pos, _)| {
                                                !self.desired.sections.contains(pos)
                                                    || (distance(**pos, self.center), **pos)
                                                        > (
                                                            distance(page.pos, self.center),
                                                            page.pos,
                                                        )
                                            })
                                            .max_by_key(|(pos, p)| {
                                                (
                                                    !self.desired.sections.contains(pos),
                                                    distance(**pos, self.center),
                                                    std::cmp::Reverse(p.touched),
                                                    **pos,
                                                )
                                            })
                                            .map(|(pos, _)| *pos);
                                        let Some(victim) = victim else {
                                            break;
                                        };
                                        // An active delivered page must be explicitly invalidated.
                                        if self.desired.sections.contains(&victim) {
                                            self.unavailable
                                                .insert(victim, TerrainUnavailable::Budget);
                                        }
                                        self.remove(victim);
                                        self.observed.insert(victim, stamp(host, victim));
                                    }
                                    if self.bytes.saturating_add(cost) > self.limits.cache_bytes
                                        || self.cache.len() >= self.limits.max_sections
                                    {
                                        self.unavailable
                                            .insert(page.pos, TerrainUnavailable::Budget);
                                        continue;
                                    }
                                    self.bytes += cost;
                                    self.cache.insert(
                                        page.pos,
                                        Cached {
                                            stamp: page.stamp,
                                            page: terrain,
                                            digest,
                                            touched: self.clock,
                                        },
                                    );
                                    self.dirty.insert(page.pos);
                                    self.unavailable.remove(&page.pos);
                                }
                                Err(reason) => {
                                    self.remove(page.pos);
                                    self.unavailable.insert(page.pos, reason);
                                }
                            }
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(Fault::new(
                        FaultCode::StorageUnavailable,
                        "visual worker disconnected",
                    ));
                }
            }
        }
        let desired: Vec<_> = self.desired.sections.iter().copied().collect();
        for pos in desired.iter().copied() {
            let current = stamp(host, pos);
            if self.cache.get(&pos).is_some_and(|p| p.stamp != current) {
                self.remove(pos);
                self.observed.remove(&pos);
                self.unavailable.insert(pos, TerrainUnavailable::Pending);
            }
            if let Some(page) = self.cache.get_mut(&pos) {
                page.touched = self.clock;
                if self.sent.get(&pos) != Some(&page.digest) {
                    self.dirty.insert(pos);
                }
            } else if current == Stamp::Missing && self.observed.get(&pos) != Some(&current) {
                self.observed.insert(pos, current);
                self.unavailable.insert(pos, TerrainUnavailable::Missing);
            }
        }
        if !self.busy {
            let mut order = desired;
            order.sort_by_key(|p| {
                (
                    u128::from(p.x.abs_diff(self.center.x))
                        + u128::from(p.y.abs_diff(self.center.y))
                        + u128::from(p.z.abs_diff(self.center.z)),
                    *p,
                )
            });
            let mut items = Vec::new();
            let mut resident_bytes = 0u64;
            for pos in order {
                if items.len() >= self.limits.pages_per_poll {
                    break;
                }
                let current = stamp(host, pos);
                if self.cache.get(&pos).is_some_and(|p| p.stamp == current)
                    || self.observed.get(&pos) == Some(&current)
                {
                    continue;
                }
                let read = match current {
                    Stamp::Resident(..) => match resident(
                        host,
                        pos,
                        self.limits.batch_bytes.saturating_sub(resident_bytes),
                    ) {
                        Ok(page) => {
                            resident_bytes = resident_bytes.saturating_add(terrain_bytes(&page));
                            Read::Resident(page)
                        }
                        Err(reason) => {
                            if reason == TerrainUnavailable::Budget && !items.is_empty() {
                                break;
                            }
                            self.observed.insert(pos, current);
                            self.unavailable.insert(pos, reason);
                            continue;
                        }
                    },
                    Stamp::Parked(hash) => Read::Saved(hash),
                    Stamp::Missing => continue,
                };
                items.push(Item {
                    pos,
                    stamp: current,
                    read,
                });
            }
            if !items.is_empty() {
                self.worker
                    .sender
                    .as_ref()
                    .expect("live visual worker")
                    .try_send(Job {
                        generation: self.generation,
                        items,
                    })
                    .map_err(|_| {
                        Fault::new(
                            FaultCode::StorageUnavailable,
                            "visual worker queue unavailable",
                        )
                    })?;
                self.busy = true;
            }
        }
        if !can_send {
            return Ok(None);
        }
        let through = host.cursor();
        let reset = if self.reset {
            let retained = self
                .cache
                .iter()
                .filter(|(p, c)| {
                    self.desired.sections.contains(p)
                        && c.stamp == stamp(host, **p)
                        && self.sent.get(p) == Some(&c.digest)
                })
                .map(|(p, c)| (*p, c.digest))
                .collect();
            Some(TerrainReset {
                sections: self.desired.sections.iter().copied().collect(),
                retained,
            })
        } else {
            None
        };
        let mut pages = Vec::new();
        let mut bytes = 0u64;
        for pos in self.dirty.iter().copied().collect::<Vec<_>>() {
            if !self.desired.sections.contains(&pos) {
                self.dirty.remove(&pos);
                continue;
            }
            let Some(cached) = self.cache.get(&pos) else {
                self.dirty.remove(&pos);
                continue;
            };
            let cost = terrain_bytes(&cached.page);
            if pages.len() >= self.limits.pages_per_poll
                || bytes.saturating_add(cost) > self.limits.batch_bytes
            {
                break;
            }
            bytes += cost;
            pages.push(TerrainPage {
                light: None,
                at: through,
                digest: cached.digest,
                terrain: cached.page.clone(),
            });
            self.sent.insert(pos, cached.digest);
            self.dirty.remove(&pos);
        }
        if reset.is_none() && pages.is_empty() && self.unavailable.is_empty() {
            return Ok(None);
        }
        self.reset = false;
        self.seq = self
            .seq
            .checked_add(1)
            .ok_or_else(|| Fault::resource_limit("visual sequence exhausted"))?;
        let unavailable = std::mem::take(&mut self.unavailable)
            .into_iter()
            .filter(|(p, _)| self.desired.sections.contains(p))
            .collect();
        Ok(Some(TerrainUpdate {
            binding: self.binding,
            generation: self.generation,
            seq: self.seq,
            through,
            reset,
            pages,
            unavailable,
        }))
    }
}
