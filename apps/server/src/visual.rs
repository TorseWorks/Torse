//! Read-only display pages. Disk I/O never installs authoritative sections.
use crate::{HostAuthority, execution::ExecutionHost, section_pages::SectionPages};
use sha2::{Digest as _, Sha256};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque},
    sync::mpsc,
    thread,
};
use torse_codec::wire;
use torse_net::{ReplicaMirror, terrain::terrain_bytes};
use torse_sim::replica::{ReplicaScope, ReplicaSnapshot, WorldReplicaSeed};
use torse_types::*;
use torse_world::{Snapshot, SnapshotCells, SnapshotSection, StateEntry};

mod approval;
mod frame;
pub use approval::VisualTerrainApproval;
pub(crate) use frame::check_frame;

const MAX_VISUAL_SECTIONS: usize = MAX_TERRAIN_INTEREST_SECTIONS;

#[derive(Clone, Copy, Debug)]
pub struct VisualTerrainLimits {
    /// Maximum pages in the current desired window.
    pub max_sections: usize,
    pub cache_bytes: u64,
    pub io_bytes: u64,
    pub batch_bytes: u64,
    /// Encoded and decoded control-frame budget, independent of page batches.
    pub control_bytes: u64,
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
    pub reused_read_pages: u64,
    /// Source checks and publication-history entries consumed by the last slice.
    pub metadata_scanned: usize,
    pub pending_candidates: usize,
    pub light_scanned: usize,
}
impl VisualTerrainLimits {
    fn validate(self, pages: &SectionPages) -> Result<u64, Fault> {
        if self.max_sections == 0
            || self.max_sections > MAX_VISUAL_SECTIONS
            || self.pages_per_poll == 0
            || self.pages_per_poll > 64
            || self.cache_bytes < 4096
            || self.batch_bytes < 4096
            || self.control_bytes < 1024
            || self.io_bytes < pages.budget().max_bytes.saturating_mul(4)
        {
            return Err(Fault::resource_limit("visual terrain limits"));
        }
        self.cache_bytes
            .checked_add(self.io_bytes)
            .and_then(|n| n.checked_add(self.batch_bytes.checked_mul(3)?))
            .and_then(|n| n.checked_add(self.control_bytes.checked_mul(4)?))
            .and_then(|n| n.checked_add(self.max_sections as u64 * 1024))
            .ok_or_else(|| Fault::resource_limit("visual terrain reservation overflow"))
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Stamp {
    Resident(Revision, Option<Revision>),
    Parked(Digest),
    Missing,
}
pub(crate) fn stamp(host: &ExecutionHost, pos: SectionPos) -> Stamp {
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
pub(crate) const TRANSITION_LIMIT: usize = 1024;

/// Recent terrain publications. A peer behind this bounded history must
/// rebuild its proofs from live metadata before using them in a reset.
pub(crate) struct TerrainHistory {
    entries: VecDeque<(CommitSeq, SectionPos)>,
    forgotten_through: CommitSeq,
}

impl Default for TerrainHistory {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            forgotten_through: CommitSeq(0),
        }
    }
}

impl TerrainHistory {
    pub(crate) fn record(
        &mut self,
        seq: CommitSeq,
        positions: impl IntoIterator<Item = SectionPos>,
    ) {
        const MAX_CHANGES: usize = 4096;
        for pos in positions {
            if self.entries.len() == MAX_CHANGES {
                self.forgotten_through = self.entries.pop_front().expect("full history").0;
            }
            self.entries.push_back((seq, pos));
        }
    }

    fn since(
        &self,
        cut: CommitSeq,
    ) -> Option<impl ExactSizeIterator<Item = (CommitSeq, SectionPos)> + '_> {
        (cut >= self.forgotten_through).then(|| {
            let first = self.entries.partition_point(|(seq, _)| *seq <= cut);
            self.entries.range(first..).copied()
        })
    }
}

struct SentPage {
    digest: Digest,
    verified: Option<CommitSeq>,
}

struct Cached {
    stamp: Stamp,
    page: ReplicaSnapshot,
    digest: Digest,
    touched: u64,
    verify_source: bool,
}

type CacheEvictionRank = (bool, u128, Reverse<u64>, SectionPos);

#[derive(Default)]
struct CacheEviction {
    ranked: BinaryHeap<CacheEvictionRank>,
    tied: Vec<CacheEvictionRank>,
}

impl CacheEviction {
    fn take(
        &mut self,
        target: (u128, SectionPos),
        mut is_current: impl FnMut(SectionPos, u64) -> bool,
    ) -> Option<SectionPos> {
        let mut victim = None;
        while let Some(&(outside, distance, touched, pos)) = self.ranked.peek() {
            if !outside && distance < target.0 {
                break;
            }
            let rank = self.ranked.pop().expect("peeked candidate");
            if !is_current(pos, touched.0) {
                continue;
            }
            if outside || (distance, pos) > target {
                victim = Some(pos);
                break;
            }
            // Same distance, but only coordinates strictly after the
            // incoming page qualify. Keep the others for the next page.
            self.tied.push(rank);
        }
        // Insert individually so a handful of ties does not rebuild the heap.
        for rank in self.tied.drain(..) {
            self.ranked.push(rank);
        }
        victim
    }
}

fn distance(pos: SectionPos, center: SectionPos) -> u128 {
    u128::from(pos.x.abs_diff(center.x))
        + u128::from(pos.y.abs_diff(center.y))
        + u128::from(pos.z.abs_diff(center.z))
}
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ColumnPriority {
    horizontal: u128,
    x: i64,
    z: i64,
    vertical: u64,
    y: i64,
}

impl ColumnPriority {
    fn position(&self) -> SectionPos {
        SectionPos::new(self.x, self.y, self.z)
    }
}

fn column_priority(pos: SectionPos, center: SectionPos) -> ColumnPriority {
    ColumnPriority {
        horizontal: u128::from(pos.x.abs_diff(center.x)) + u128::from(pos.z.abs_diff(center.z)),
        x: pos.x,
        z: pos.z,
        vertical: pos.y.abs_diff(center.y),
        y: pos.y,
    }
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
    refresh: u64,
    items: Vec<Item>,
}
struct Prepared {
    pos: SectionPos,
    stamp: Stamp,
    result: Result<(ReplicaSnapshot, Digest), TerrainUnavailable>,
}
struct Response {
    generation: u64,
    refresh: u64,
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

pub(crate) struct VisualStart {
    pub center: SectionPos,
    pub generation: u64,
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
    refresh: u64,
    reused_read_pages: u64,
    seq: u64,
    reset: bool,
    cache: BTreeMap<SectionPos, Cached>,
    bytes: u64,
    clock: u64,
    dirty: BTreeSet<ColumnPriority>,
    unavailable: BTreeMap<SectionPos, TerrainUnavailable>,
    observed: BTreeMap<SectionPos, Stamp>,
    sent: BTreeMap<SectionPos, SentPage>,
    proof_cut: CommitSeq,
    proof_floor: CommitSeq,
    light_sent: BTreeMap<SectionPos, u64>,
    light_seq: u64,
    reserved_bytes: u64,
    wire_budget: OutputBudget,
    checked_control_sections: usize,
    codec: torse_codec::CodecLimits,
    order: Vec<SectionPos>,
    scan_cursor: usize,
    last_scan: usize,
    light_cursor: usize,
    last_light_scan: usize,
    candidates: BTreeSet<ColumnPriority>,
    _memory: Option<MemoryLease>,
}
impl VisualSession {
    pub(crate) fn desired_positions(&self) -> Vec<SectionPos> {
        self.desired.sections.iter().copied().collect()
    }
    pub(crate) fn center(&self) -> SectionPos {
        self.center
    }
    pub(crate) fn check_request(&mut self, desired: &ReplicaScope) -> Result<(), Fault> {
        self.approved
            .check_desired(desired, self.limits.max_sections)?;
        if desired == &self.desired {
            return Ok(());
        }
        self.generation
            .checked_add(1)
            .ok_or_else(|| Fault::resource_limit("visual generation exhausted"))?;
        // Both reset collections contain fixed-width values with fixed
        // decoder charges per entry. Moving or shrinking an approved window
        // within the checked count fits the same immutable session budgets.
        let sections = desired.sections.len();
        if sections > self.checked_control_sections {
            Self::check_control(
                self.binding,
                desired,
                self.limits.control_bytes,
                self.wire_budget,
                self.codec,
            )?;
            self.checked_control_sections = sections;
        }
        Ok(())
    }
    fn check_control(
        binding: Binding,
        desired: &ReplicaScope,
        control_bytes: u64,
        mut budget: OutputBudget,
        codec: torse_codec::CodecLimits,
    ) -> Result<(), Fault> {
        budget.max_bytes = budget.max_bytes.min(control_bytes);
        // Every desired page may eventually have a retained-content proof.
        // Digest values do not change codec cost. This upper bound guarantees
        // that a later reset fits even after the cache has filled completely.
        let reset = TerrainReset {
            sections: desired.sections.iter().copied().collect(),
            retained: desired
                .sections
                .iter()
                .map(|&p| (p, Digest([0; 32])))
                .collect(),
        };
        let message = crate::network::ServerMessage::Terrain(TerrainUpdate {
            binding,
            generation: 1,
            seq: 1,
            through: CommitCursor::initial(binding.world),
            reset: Some(reset),
            pages: Vec::new(),
            unavailable: Vec::new(),
        });
        check_frame(&message, budget, codec)
    }
    fn prioritize(&mut self) {
        self.candidates = self
            .desired
            .sections
            .iter()
            .map(|&p| column_priority(p, self.center))
            .collect();
        self.order.clear();
        self.order
            .extend(self.candidates.iter().map(ColumnPriority::position));
        self.dirty = self
            .dirty
            .iter()
            .map(|key| column_priority(key.position(), self.center))
            .collect();
        self.scan_cursor = 0;
        self.light_cursor = 0;
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
            optics: field.optics_identity(),
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
        budget: OutputBudget,
        codec: torse_codec::CodecLimits,
        checks: usize,
    ) -> Result<Option<TerrainLightUpdate>, Fault> {
        self.last_light_scan = 0;
        if self.reset {
            return Ok(None);
        }
        let mut pages = Vec::new();
        let mut bytes = 0u64;
        for _ in 0..checks.min(self.order.len()) {
            if pages.len() == self.limits.pages_per_poll {
                break;
            }
            let pos = self.order[self.light_cursor];
            self.light_cursor = (self.light_cursor + 1) % self.order.len();
            self.last_light_scan += 1;
            let Some(cached) = self.cache.get(&pos) else {
                continue;
            };
            if self.sent.get(&pos).map(|page| page.digest) != Some(cached.digest) {
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
        let mut message = crate::network::ServerMessage::Light(TerrainLightUpdate {
            binding: self.binding,
            interest_generation: self.generation,
            seq,
            pages,
        });
        loop {
            match check_frame(&message, budget, codec) {
                Ok(_) => break,
                Err(error) if error.code == FaultCode::ResourceLimit => {
                    let crate::network::ServerMessage::Light(update) = &mut message else {
                        unreachable!()
                    };
                    if update.pages.len() == 1 {
                        // Lighting is optional: a terrain-capable connection
                        // may have no room for a light page. Keep local fallback
                        // and leave the publication unsent for a later retry.
                        return Ok(None);
                    }
                    update.pages.truncate(update.pages.len() / 2);
                }
                Err(error) => return Err(error),
            }
        }
        let crate::network::ServerMessage::Light(mut update) = message else {
            unreachable!()
        };
        // Keep distance priority while selecting and shrinking the frame, then
        // satisfy the mirror's strict wire order. Reordering fixed-width pages
        // does not change the encoded frame size.
        update.pages.sort_unstable_by_key(|page| page.pos);
        for light in &update.pages {
            self.light_sent.insert(light.pos, light.generation);
        }
        self.light_seq = seq;
        Ok(Some(update))
    }
    pub(crate) fn defer_inline_lights(&mut self, update: &mut TerrainUpdate<ReplicaSnapshot>) {
        for page in &mut update.pages {
            if let Some(light) = page.light.take() {
                self.light_sent.remove(&light.pos);
            }
        }
    }
    pub(crate) fn new(
        host: &ExecutionHost,
        pages: SectionPages,
        approved: VisualTerrainApproval,
        desired: ReplicaScope,
        start: VisualStart,
        limits: VisualTerrainLimits,
        network: crate::network::NetworkLimits,
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
        Self::check_control(
            host.binding(),
            &desired,
            limits.control_bytes,
            network.budget,
            network.codec,
        )?;
        let VisualStart { center, generation } = start;
        let checked_control_sections = desired.sections.len();
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
                            refresh: job.refresh,
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
        let mut session = Self {
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
            generation,
            refresh: 0,
            reused_read_pages: 0,
            seq: 0,
            reset: true,
            cache: BTreeMap::new(),
            bytes: 0,
            clock: 0,
            dirty: BTreeSet::new(),
            unavailable: BTreeMap::new(),
            observed: BTreeMap::new(),
            sent: BTreeMap::new(),
            proof_cut: host.cursor().seq,
            proof_floor: CommitSeq(0),
            light_sent: BTreeMap::new(),
            light_seq: 0,
            reserved_bytes: charge,
            wire_budget: network.budget,
            checked_control_sections,
            codec: network.codec,
            order: Vec::new(),
            scan_cursor: 0,
            last_scan: 0,
            light_cursor: 0,
            last_light_scan: 0,
            candidates: BTreeSet::new(),
            _memory: memory,
        };
        session.prioritize();
        Ok(session)
    }
    pub(crate) fn request(
        &mut self,
        desired: ReplicaScope,
        center: SectionPos,
        refresh: bool,
    ) -> Result<(), Fault> {
        self.check_request(&desired)?;
        self.request_admitted(desired, center, refresh)
    }
    /// The caller must have completed check_request before changing other
    /// subsystems. Avoid serializing the full reset twice in that transaction.
    pub(crate) fn request_admitted(
        &mut self,
        desired: ReplicaScope,
        center: SectionPos,
        refresh: bool,
    ) -> Result<(), Fault> {
        let moved = self.center != center;
        self.center = center;
        if !refresh && desired == self.desired {
            if moved {
                self.prioritize();
            }
            return Ok(());
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| Fault::resource_limit("visual generation exhausted"))?;
        if refresh {
            self.refresh = self.generation;
        }
        self.seq = 0;
        self.light_seq = 0;
        self.light_sent.clear();
        self.reset = true;
        self.desired = desired;
        self.dirty.clear();
        self.unavailable.clear();
        self.observed.clear();
        self.prioritize();
        Ok(())
    }
    fn remove(&mut self, pos: SectionPos) {
        if let Some(old) = self.cache.remove(&pos) {
            self.bytes = self.bytes.saturating_sub(terrain_bytes(&old.page));
        }
        self.dirty.remove(&column_priority(pos, self.center));
        self.sent.remove(&pos);
        self.light_sent.remove(&pos);
    }

    fn cache_victim(
        &self,
        eviction: &mut Option<CacheEviction>,
        target: SectionPos,
    ) -> Option<SectionPos> {
        // Rank once when an incoming batch first runs out of space. Pages
        // inserted during this batch are dirty and cannot be evicted yet.
        let eviction = eviction.get_or_insert_with(|| {
            let ranked = self
                .cache
                .iter()
                .filter(|(pos, _)| !self.dirty.contains(&column_priority(**pos, self.center)))
                .map(|(&pos, page)| {
                    (
                        !self.desired.sections.contains(&pos),
                        distance(pos, self.center),
                        Reverse(page.touched),
                        pos,
                    )
                })
                .collect::<Vec<_>>();
            CacheEviction {
                ranked: BinaryHeap::from(ranked),
                tied: Vec::new(),
            }
        });
        eviction.take((distance(target, self.center), target), |pos, touched| {
            self.cache
                .get(&pos)
                .is_some_and(|page| page.touched == touched)
        })
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
            reused_read_pages: self.reused_read_pages,
            metadata_scanned: self.last_scan,
            pending_candidates: self.candidates.len(),
            light_scanned: self.last_light_scan,
        }
    }
    pub(crate) fn poll(
        &mut self,
        host: &ExecutionHost,
        can_send: bool,
    ) -> Result<Option<TerrainUpdate<ReplicaSnapshot>>, Fault> {
        self.poll_with_budget(host, can_send, usize::MAX)
    }

    fn observe_publications(
        &mut self,
        host: &ExecutionHost,
        cut: CommitSeq,
        budget: usize,
    ) -> usize {
        let previous = std::mem::replace(&mut self.proof_cut, cut);
        if previous == cut {
            return 0;
        }
        let Some(changes) = host
            .visual_history
            .since(previous)
            .filter(|changes| changes.len() <= budget)
        else {
            // Old proofs cannot cross a missed or unaffordable mutation range.
            // The current scan can still establish new proofs at this cut.
            self.proof_floor = cut;
            return 0;
        };
        let checked = changes.len();
        for (seq, pos) in changes {
            if let Some(page) = self.sent.get_mut(&pos)
                && page.verified.is_some_and(|verified| verified < seq)
            {
                page.verified = None;
            }
            if self.desired.sections.contains(&pos) {
                // A durable publication can arrive just after the rotating
                // scan passed this position. Give it normal distance priority
                // immediately while keeping the source checks at admission.
                self.candidates.insert(column_priority(pos, self.center));
            }
        }
        checked
    }

    pub(crate) fn poll_with_budget(
        &mut self,
        host: &ExecutionHost,
        can_send: bool,
        metadata_checks: usize,
    ) -> Result<Option<TerrainUpdate<ReplicaSnapshot>>, Fault> {
        self.last_scan = 0;
        self.last_light_scan = 0;
        if metadata_checks == 0 {
            return Ok(None);
        }
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
                    if response.refresh == self.refresh {
                        let overlapping = response.generation != self.generation;
                        let mut eviction = None;
                        for page in response.pages {
                            if !self.desired.sections.contains(&page.pos)
                                || stamp(host, page.pos) != page.stamp
                                || (overlapping && page.result.is_err())
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
                                        self.remove(page.pos);
                                        self.unavailable
                                            .insert(page.pos, TerrainUnavailable::Corrupt);
                                        continue;
                                    }
                                    if let Some(cached) = self.cache.get_mut(&page.pos)
                                        && cached.stamp == page.stamp
                                        && cached.digest == digest
                                    {
                                        cached.verify_source = false;
                                        self.reused_read_pages = self
                                            .reused_read_pages
                                            .saturating_add(u64::from(overlapping));
                                        self.unavailable.remove(&page.pos);
                                        continue;
                                    }
                                    self.remove(page.pos);
                                    let cost = terrain_bytes(&terrain);
                                    while self.bytes.saturating_add(cost) > self.limits.cache_bytes
                                        || self.cache.len() >= self.limits.max_sections
                                    {
                                        let Some(victim) =
                                            self.cache_victim(&mut eviction, page.pos)
                                        else {
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
                                    self.reused_read_pages = self
                                        .reused_read_pages
                                        .saturating_add(u64::from(overlapping));
                                    self.cache.insert(
                                        page.pos,
                                        Cached {
                                            stamp: page.stamp,
                                            page: terrain,
                                            digest,
                                            touched: self.clock,
                                            verify_source: false,
                                        },
                                    );
                                    self.dirty.insert(column_priority(page.pos, self.center));
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
        // Metadata inspection has a separate bounded slice. The priority order
        // is rebuilt only when interest/center changes, never for every batch.
        let checks = if metadata_checks == usize::MAX {
            // Preserve the legacy poll slice. An explicit owner budget is
            // independent of the number of pages allowed in an output batch.
            self.limits.pages_per_poll * 64
        } else {
            metadata_checks
        };
        let through = host.cursor();
        let invalidations = if metadata_checks == usize::MAX {
            0
        } else {
            self.observe_publications(host, through.seq, checks)
        };
        let scan = self.order.len().min(checks - invalidations);
        self.last_scan = invalidations + scan;
        for _ in 0..scan {
            let pos = self.order[self.scan_cursor];
            self.scan_cursor = (self.scan_cursor + 1) % self.order.len();
            let current = stamp(host, pos);
            let mut retained = false;
            let mut verify_source = false;
            let mut stale = false;
            if let Some(page) = self.cache.get_mut(&pos) {
                if page.stamp != current {
                    self.observed.remove(&pos);
                    if host.proves_visual_transition(pos, page.stamp, current) {
                        // Reaffirm at the new durable cursor for the client's
                        // near-data floor. Keep the page while verifying its source.
                        page.stamp = current;
                        page.verify_source = true;
                        self.dirty.insert(column_priority(pos, self.center));
                        self.unavailable.remove(&pos);
                    } else {
                        stale = true;
                    }
                }
                if !stale {
                    retained = true;
                    verify_source = page.verify_source;
                    page.touched = self.clock;
                    if let Some(sent) = self
                        .sent
                        .get_mut(&pos)
                        .filter(|sent| sent.digest == page.digest)
                    {
                        sent.verified = Some(through.seq);
                    } else {
                        self.dirty.insert(column_priority(pos, self.center));
                    }
                }
            }
            if stale {
                self.remove(pos);
                self.unavailable.insert(pos, TerrainUnavailable::Pending);
            }
            let observed = self.observed.get(&pos) == Some(&current);
            if !retained && current == Stamp::Missing && !observed {
                self.observed.insert(pos, current);
                self.unavailable.insert(pos, TerrainUnavailable::Missing);
            } else if !observed || verify_source {
                self.candidates.insert(column_priority(pos, self.center));
            }
        }
        if !self.busy {
            let mut items = Vec::new();
            let mut resident_bytes = 0u64;
            let mut inspected = 0;
            while let Some(candidate) = self.candidates.pop_first() {
                let pos = candidate.position();
                if items.len() >= self.limits.pages_per_poll || inspected >= scan {
                    self.candidates.insert(candidate);
                    break;
                }
                inspected += 1;
                let current = stamp(host, pos);
                if self
                    .cache
                    .get(&pos)
                    .is_some_and(|p| p.stamp == current && !p.verify_source)
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
                                self.candidates.insert(candidate);
                                break;
                            }
                            self.remove(pos);
                            self.observed.insert(pos, current);
                            self.unavailable.insert(pos, reason);
                            continue;
                        }
                    },
                    Stamp::Parked(hash) => Read::Saved(hash),
                    Stamp::Missing => {
                        if self.observed.get(&pos) != Some(&current) {
                            self.observed.insert(pos, current);
                            self.unavailable.insert(pos, TerrainUnavailable::Missing);
                        }
                        continue;
                    }
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
                        refresh: self.refresh,
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
        if self.reset {
            let retained: Vec<_> = if metadata_checks == usize::MAX {
                self.cache
                    .iter()
                    .filter(|(p, c)| {
                        self.desired.sections.contains(p)
                            && c.stamp == stamp(host, **p)
                            && self.sent.get(p).map(|page| page.digest) == Some(c.digest)
                    })
                    .map(|(p, c)| (*p, c.digest))
                    .collect()
            } else {
                self.sent
                    .iter()
                    .filter_map(|(&pos, page)| {
                        if !self.desired.sections.contains(&pos) {
                            return None;
                        }
                        page.verified
                            .filter(|seq| *seq >= self.proof_floor)
                            .map(|_| (pos, page.digest))
                    })
                    .collect()
            };
            for (pos, _) in &retained {
                self.dirty.remove(&column_priority(*pos, self.center));
            }
            // Reset removes every other page from the mirror, even when its
            // cached source can be proven unchanged on a later bounded scan.
            // Only an actual page delivery can restore its retention eligibility.
            self.sent.clear();
            self.proof_cut = through.seq;
            self.sent.extend(retained.iter().map(|&(pos, digest)| {
                (
                    pos,
                    SentPage {
                        digest,
                        verified: Some(through.seq),
                    },
                )
            }));
            let reset = TerrainReset {
                sections: self.desired.sections.iter().copied().collect(),
                retained,
            };
            self.reset = false;
            self.seq = 1;
            return Ok(Some(TerrainUpdate {
                binding: self.binding,
                generation: self.generation,
                seq: self.seq,
                through,
                reset: Some(reset),
                pages: Vec::new(),
                unavailable: Vec::new(),
            }));
        }
        let reset = None;
        let mut pages = Vec::new();
        let mut bytes = 0u64;
        for key in self
            .dirty
            .iter()
            .copied()
            .take(self.limits.pages_per_poll * 2)
            .collect::<Vec<_>>()
        {
            let pos = key.position();
            if !self.desired.sections.contains(&pos) {
                self.dirty.remove(&key);
                continue;
            }
            let Some(cached) = self.cache.get(&pos) else {
                self.dirty.remove(&key);
                continue;
            };
            // A bounded metadata scan may not have reached this dirty page yet.
            // Recheck immediately before delivery so no stale terrain escapes.
            if cached.stamp != stamp(host, pos) {
                self.candidates.insert(key);
                continue;
            }
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
        }
        pages.sort_unstable_by_key(|p| p.terrain.sections[0].pos);
        if reset.is_none() && pages.is_empty() && self.unavailable.is_empty() {
            return Ok(None);
        }
        let seq = self
            .seq
            .checked_add(1)
            .ok_or_else(|| Fault::resource_limit("visual sequence exhausted"))?;
        let keys: Vec<_> = self
            .unavailable
            .keys()
            .copied()
            .take(self.limits.pages_per_poll)
            .collect();
        let unavailable = keys
            .into_iter()
            .map(|p| (p, self.unavailable[&p]))
            .filter(|(p, _)| self.desired.sections.contains(p))
            .collect();
        let mut message = crate::network::ServerMessage::Terrain(TerrainUpdate {
            binding: self.binding,
            generation: self.generation,
            seq,
            through,
            reset,
            pages,
            unavailable,
        });
        loop {
            match check_frame(&message, self.wire_budget, self.codec) {
                Ok(()) => break,
                Err(error) if error.code == FaultCode::ResourceLimit => {
                    let crate::network::ServerMessage::Terrain(update) = &mut message else {
                        unreachable!()
                    };
                    if update.pages.len() > 1 {
                        update.pages.truncate(update.pages.len().div_ceil(2));
                    } else if update.unavailable.len() > 1 {
                        update
                            .unavailable
                            .truncate(update.unavailable.len().div_ceil(2));
                    } else if let Some(page) = update.pages.pop() {
                        let pos = page.terrain.sections[0].pos;
                        self.remove(pos);
                        self.observed.insert(pos, stamp(host, pos));
                        update.unavailable.retain(|(p, _)| *p != pos);
                        update.unavailable.push((pos, TerrainUnavailable::Budget));
                        update.unavailable.sort_unstable_by_key(|(p, _)| *p);
                    } else {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
        let crate::network::ServerMessage::Terrain(update) = message else {
            unreachable!()
        };
        for page in &update.pages {
            let pos = page.terrain.sections[0].pos;
            self.sent.insert(
                pos,
                SentPage {
                    digest: page.digest,
                    verified: Some(through.seq),
                },
            );
            self.dirty.remove(&column_priority(pos, self.center));
        }
        for (pos, _) in &update.unavailable {
            self.unavailable.remove(pos);
        }
        self.seq = seq;
        Ok(Some(update))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eviction_keeps_ties_for_later_pages_and_skips_replaced_entries() {
        let near_tie = SectionPos::new(-2, 0, 0);
        let far_tie = SectionPos::new(2, 0, 0);
        let outside = SectionPos::new(1, 0, 0);
        let replaced = SectionPos::new(3, 0, 0);
        let mut eviction = CacheEviction {
            ranked: BinaryHeap::from([
                (false, 2, Reverse(0), near_tie),
                (false, 2, Reverse(1), far_tie),
                (true, 1, Reverse(2), outside),
                (false, 3, Reverse(0), replaced),
            ]),
            ..Default::default()
        };
        let current = |pos, _| pos != replaced;
        assert_eq!(eviction.take((2, SectionPos::ZERO), current), Some(outside));
        assert_eq!(eviction.take((2, SectionPos::ZERO), current), Some(far_tie));
        assert_eq!(eviction.take((2, SectionPos::ZERO), current), None);
        assert_eq!(
            eviction.take((1, SectionPos::ZERO), current),
            Some(near_tie)
        );
    }

    #[test]
    fn compact_priorities_preserve_coordinate_ties_and_extreme_distances() {
        let center = SectionPos::new(i64::MIN, 0, i64::MAX);
        let mut expected: Vec<_> = [i64::MIN, 0, i64::MAX]
            .into_iter()
            .flat_map(|x| {
                [i64::MIN, 0, i64::MAX]
                    .into_iter()
                    .flat_map(move |z| [-1, 0, 1].map(|y| SectionPos::new(x, y, z)))
            })
            .collect();
        let ranks: BTreeSet<_> = expected
            .iter()
            .map(|&pos| column_priority(pos, center))
            .collect();
        expected.sort_unstable_by_key(|&pos| {
            (
                u128::from(pos.x.abs_diff(center.x)) + u128::from(pos.z.abs_diff(center.z)),
                pos.x,
                pos.z,
                pos.y.abs_diff(center.y),
                pos,
            )
        });
        let actual: Vec<_> = ranks.iter().map(ColumnPriority::position).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn forgotten_batch_entries_require_a_cut_at_or_after_the_publication() {
        let mut history = TerrainHistory::default();
        history.record(CommitSeq(2), (0..4097).map(|x| SectionPos::new(x, 0, 0)));
        assert!(history.since(CommitSeq(1)).is_none());
        assert_eq!(history.since(CommitSeq(2)).unwrap().len(), 0);
        let pos = SectionPos::ZERO;
        history.record(CommitSeq(5), [pos]);
        assert_eq!(
            history.since(CommitSeq(2)).unwrap().collect::<Vec<_>>(),
            [(CommitSeq(5), pos)]
        );
    }
}
