//! Host-owned shared lighting. File reads never run on the authority thread.
use crate::{HostAuthority, execution::ExecutionHost, section_pages::SectionPages};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, mpsc},
    thread,
};
use torse_types::*;
use torse_world::{ArchiveManifest, SectionPhase, Snapshot, SnapshotArchive};
use torse_world_services::lighting::{
    LightFieldDiagnostics, LightFieldLimits, LightOptics, LightOpticsTable, LightPublication,
    LightSource, LightTerrain, SharedLightField,
};

#[derive(Clone, Copy, Debug)]
pub struct HostLightLimits {
    pub input_sections: usize,
    pub input_bytes: usize,
    pub pages_per_poll: usize,
    pub field: LightFieldLimits,
}
/// Optional lighting work per scheduling call. A target check includes its
/// configured, bounded dependency envelope; solver preparation/capture is a
/// non-preemptible unit bounded by `LightFieldLimits::job`, not a time deadline.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostLightWorkBudget {
    pub input_checks: usize,
    pub validation_checks: usize,
    pub readiness_checks: usize,
    pub solver_work: usize,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct HostLightProgress {
    pub input_checks: usize,
    pub validation_checks: usize,
    pub readiness_checks: usize,
    pub published: Option<SectionPos>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Source {
    Resident(Revision, Option<Revision>),
    Parked(Digest),
    Missing,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightInputStatus {
    Pending,
    Ready,
    Missing,
    Corrupt,
    Budget,
}
#[derive(Clone, Copy, Debug)]
pub struct HostLightDiagnostics {
    pub field: LightFieldDiagnostics,
    pub input_bytes: usize,
    pub input_peak: usize,
    pub io_pending: bool,
    pub io_in_flight: usize,
    pub input_sections: usize,
    pub admitted_links: usize,
    /// Targets omitted by best-effort admission, accumulated across requests.
    pub admission_denials: u64,
    pub reserved_bytes: u64,
}
struct Input {
    source: Source,
    status: LightInputStatus,
}
struct ReadResult {
    generation: u64,
    pos: SectionPos,
    digest: Digest,
    result: Result<Snapshot, Fault>,
}
struct Loader {
    sender: Option<mpsc::SyncSender<(u64, SectionPos, Digest)>>,
    receiver: mpsc::Receiver<ReadResult>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Drop for Loader {
    fn drop(&mut self) {
        self.sender.take();
        // A blocked result send must be allowed to finish before joining.
        let (_, empty) = mpsc::sync_channel(1);
        let receiver = std::mem::replace(&mut self.receiver, empty);
        drop(receiver);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
/// Assemble exactly once per host world, then share publications across peers.
/// This object grants no actor permissions. Hosts choose its input/read envelope;
/// network delivery must separately validate each recipient's current interest.
pub struct HostLightField {
    terrain: LightTerrain,
    field: SharedLightField,
    optics: Option<Arc<LightOpticsTable>>,
    limits: HostLightLimits,
    interests: BTreeMap<u128, BTreeSet<SectionPos>>,
    centers: BTreeMap<u128, SectionPos>,
    needed: BTreeSet<SectionPos>,
    input_order: Vec<SectionPos>,
    input_cursor: usize,
    target_order: Vec<(u128, SectionPos)>,
    target_ranks: BTreeMap<SectionPos, u128>,
    target_cursor: usize,
    inputs: BTreeMap<SectionPos, Input>,
    loader: Loader,
    in_flight: usize,
    io_capacity: usize,
    admission_denials: u64,
    io_generation: u64,
    page_budget: OutputBudget,
    _memory: Option<MemoryLease>,
    input_peak: usize,
    reserved_bytes: u64,
}
fn source(host: &ExecutionHost, pos: SectionPos) -> Source {
    let store = host.world().store();
    if matches!(
        store.phase(pos),
        Some(SectionPhase::Ready | SectionPhase::Active)
    ) {
        Source::Resident(
            store.revision(pos).expect("readable section revision"),
            store.retired_revision(pos),
        )
    } else {
        host.parked_sections()
            .get(&pos)
            .map_or(Source::Missing, |p| Source::Parked(p.digest))
    }
}
impl HostLightField {
    pub fn new(
        host: &ExecutionHost,
        pages: SectionPages,
        limits: HostLightLimits,
    ) -> Result<Self, Fault> {
        Self::with_optics(host, pages, limits, None)
    }

    pub fn new_with_optics(
        host: &ExecutionHost,
        pages: SectionPages,
        limits: HostLightLimits,
        optics: Arc<LightOpticsTable>,
    ) -> Result<Self, Fault> {
        Self::with_optics(host, pages, limits, Some(optics))
    }

    fn with_optics(
        host: &ExecutionHost,
        pages: SectionPages,
        limits: HostLightLimits,
        optics: Option<Arc<LightOpticsTable>>,
    ) -> Result<Self, Fault> {
        if optics.as_ref().is_some_and(|table| {
            table.registry_identity() != host.world().registry().authority_digest()
        }) {
            return Err(Fault::invalid_data("light optics registry mismatch"));
        }
        if limits.pages_per_poll == 0 || limits.pages_per_poll > 64 {
            return Err(Fault::resource_limit("light ingestion limit"));
        }
        let terrain = LightTerrain::new(
            host.binding(),
            host.world().store().side(),
            host.world().store().bounds(),
            host.world().registry(),
            limits.input_sections,
            limits.input_bytes,
        )?;
        let field = SharedLightField::new(host.binding(), limits.field)?;
        // Resident capture throughput need not reserve dozens of decoded file
        // pages. Pipeline a small bounded number through the disk worker.
        let io_capacity = limits.pages_per_poll.min(4);
        let reserve = (limits.input_bytes as u64)
            .checked_add(limits.field.cache_bytes as u64)
            .and_then(|n| n.checked_add(limits.field.solver_cache_bytes as u64))
            .and_then(|n| n.checked_add(field.diagnostics().work_reservation))
            .and_then(|n| {
                n.checked_add(
                    pages
                        .budget()
                        .max_bytes
                        .checked_mul(4 * (io_capacity as u64 + 1))?,
                )
            })
            .and_then(|n| n.checked_add(limits.input_sections as u64 * 512))
            .and_then(|n| n.checked_add(limits.field.interests as u64 * 128))
            .and_then(|n| n.checked_add(limits.field.sections as u64 * 128))
            .ok_or_else(|| Fault::resource_limit("shared lighting reservation"))?;
        let memory = host
            .memory_pool()
            .map(|p| p.reserve(MemoryClass::Mesh, reserve, EvictionRank::Pinned))
            .transpose()?;
        let page_budget = pages.budget();
        let (sender, jobs) = mpsc::sync_channel::<(u64, SectionPos, Digest)>(io_capacity);
        let (results, receiver) = mpsc::sync_channel(io_capacity);
        let handle = thread::Builder::new()
            .name("torse-light-pages".into())
            .spawn(move || {
                while let Ok((generation, pos, digest)) = jobs.recv() {
                    let result = pages.get(digest);
                    if results
                        .send(ReadResult {
                            generation,
                            pos,
                            digest,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        Ok(Self {
            terrain,
            field,
            optics,
            limits,
            interests: BTreeMap::new(),
            centers: BTreeMap::new(),
            needed: BTreeSet::new(),
            input_order: Vec::new(),
            input_cursor: 0,
            target_order: Vec::new(),
            target_ranks: BTreeMap::new(),
            target_cursor: 0,
            inputs: BTreeMap::new(),
            loader: Loader {
                sender: Some(sender),
                receiver,
                handle: Some(handle),
            },
            in_flight: 0,
            io_capacity,
            admission_denials: 0,
            io_generation: 0,
            page_budget,
            _memory: memory,
            input_peak: 0,
            reserved_bytes: reserve,
        })
    }
    pub fn subscribe(&mut self, consumer: u128, sections: &[SectionPos]) -> Result<(), Fault> {
        // Compute the bounded source union before changing either subscription.
        if sections.len() > self.limits.field.sections {
            return Err(Fault::resource_limit("light target limit"));
        }
        let targets: BTreeSet<_> = self
            .interests
            .iter()
            .filter(|(id, _)| **id != consumer)
            .flat_map(|(_, s)| s.iter().copied())
            .chain(sections.iter().copied())
            .collect();
        let needed = self.dependencies(&targets)?;
        self.field.subscribe(consumer, sections)?;
        self.interests
            .insert(consumer, sections.iter().copied().collect());
        self.centers.insert(consumer, SectionPos::ZERO);
        self.set_needed(needed);
        Ok(())
    }
    /// Admit the nearest targets that fit this optional service's budgets.
    /// Omitted targets must retain the recipient's local lighting fallback.
    /// Other consumers keep their admitted inputs; overlapping targets share
    /// the same computation. A new request replaces all of this consumer's old
    /// links, including when no new target fits. Retry on interest change.
    pub fn subscribe_near(
        &mut self,
        consumer: u128,
        sections: &[SectionPos],
        center: SectionPos,
    ) -> Result<usize, Fault> {
        // Validate before changing subscriptions, even if every target is denied.
        for &target in sections {
            self.validate_target(target)?;
        }
        let mut targets: BTreeSet<_> = self
            .interests
            .iter()
            .filter(|(id, _)| **id != consumer)
            .flat_map(|(_, s)| s.iter().copied())
            .collect();
        let mut needed = self.dependencies(&targets)?;
        let links: usize = self
            .interests
            .iter()
            .filter(|(id, _)| **id != consumer)
            .map(|(_, s)| s.len())
            .sum();
        let available = self.interests.contains_key(&consumer)
            || self.interests.len() < self.limits.field.consumers;
        let mut ordered = sections.to_vec();
        ordered.sort_unstable();
        ordered.dedup();
        let requested = ordered.len();
        ordered.sort_unstable_by_key(|&p| (distance(p, center), p.x, p.z, p.y));
        let mut admitted = BTreeSet::new();
        if available {
            for target in ordered {
                if links + admitted.len() == self.limits.field.interests
                    || (!targets.contains(&target) && targets.len() == self.limits.field.sections)
                {
                    continue;
                }
                let envelope = match self.dependencies(&[target].into()) {
                    Ok(envelope) => envelope,
                    Err(e) if e.code == FaultCode::ResourceLimit => continue,
                    Err(e) => return Err(e),
                };
                let added = envelope.iter().filter(|p| !needed.contains(p)).count();
                if needed.len() + added > self.limits.input_sections {
                    continue;
                }
                needed.extend(envelope);
                targets.insert(target);
                admitted.insert(target);
            }
        }
        let count = admitted.len();
        if admitted.is_empty() {
            self.unsubscribe(consumer);
        } else {
            self.field
                .subscribe(consumer, &admitted.iter().copied().collect::<Vec<_>>())?;
            self.interests.insert(consumer, admitted);
            self.centers.insert(consumer, center);
            self.set_needed(needed);
        }
        self.admission_denials = self
            .admission_denials
            .saturating_add((requested - count) as u64);
        Ok(count)
    }
    pub fn unsubscribe(&mut self, consumer: u128) {
        self.field.unsubscribe(consumer);
        self.interests.remove(&consumer);
        self.centers.remove(&consumer);
        let targets = self
            .interests
            .values()
            .flat_map(|s| s.iter().copied())
            .collect();
        let needed = self
            .dependencies(&targets)
            .expect("subset of admitted lighting sources");
        self.set_needed(needed);
    }
    fn set_needed(&mut self, needed: BTreeSet<SectionPos>) {
        self.inputs.retain(|p, _| needed.contains(p));
        let removed: Vec<_> = self
            .terrain
            .positions()
            .filter(|p| !needed.contains(p))
            .collect();
        for pos in removed {
            self.terrain.remove(pos);
        }
        self.needed = needed;
        self.input_order = self.needed.iter().copied().collect();
        let centers = &self.centers;
        self.input_order.sort_unstable_by_key(|&p| {
            (
                centers.values().map(|&c| distance(p, c)).min().unwrap_or(0),
                p.x,
                p.z,
                std::cmp::Reverse(p.y),
            )
        });
        self.input_cursor = 0;
        let mut targets = BTreeMap::<SectionPos, u128>::new();
        for (&consumer, positions) in &self.interests {
            for &pos in positions {
                let rank = distance(pos, self.centers[&consumer]);
                targets
                    .entry(pos)
                    .and_modify(|old| *old = (*old).min(rank))
                    .or_insert(rank);
            }
        }
        self.target_order = targets.iter().map(|(&pos, &rank)| (rank, pos)).collect();
        self.target_ranks = targets;
        self.target_order.sort_unstable();
        self.target_cursor = 0;
    }
    fn validate_target(&self, target: SectionPos) -> Result<(), Fault> {
        let side = i128::from(self.terrain.side());
        let bounds = self.terrain.bounds();
        let base = i128::from(target.y) * side;
        if base >= i128::from(bounds.max_y) || base + side <= i128::from(bounds.min_y) {
            return Err(Fault::invalid_data("light target outside world bounds"));
        }
        Ok(())
    }
    fn dependencies(&self, targets: &BTreeSet<SectionPos>) -> Result<BTreeSet<SectionPos>, Fault> {
        let side = i128::from(self.terrain.side());
        let bounds = self.terrain.bounds();
        let ymin = i128::from(bounds.min_y).div_euclid(side);
        let ymax = (i128::from(bounds.max_y) - 1).div_euclid(side);
        let mut needed = BTreeSet::new();
        let mut columns = BTreeSet::new();
        for &target in targets {
            self.validate_target(target)?;
            if !columns.insert((target.x, target.z)) {
                continue;
            }
            let first = |v: i64| (i128::from(v) * side - 15).div_euclid(side);
            let last = |v: i64| (i128::from(v) * side + side + 14).div_euclid(side);
            let width = last(target.x) - first(target.x) + 1;
            let depth = last(target.z) - first(target.z) + 1;
            if width * depth * (ymax - ymin + 1) > self.limits.input_sections as i128 {
                return Err(Fault::resource_limit("light source envelope"));
            }
            for x in first(target.x)..=last(target.x) {
                for y in ymin..=ymax {
                    for z in first(target.z)..=last(target.z) {
                        let pos = SectionPos::new(
                            i64::try_from(x).map_err(invalid)?,
                            i64::try_from(y).map_err(invalid)?,
                            i64::try_from(z).map_err(invalid)?,
                        );
                        needed.insert(pos);
                        if needed.len() > self.limits.input_sections {
                            return Err(Fault::resource_limit("light source union"));
                        }
                    }
                }
            }
        }
        Ok(needed)
    }
    /// Explicitly retry unavailable pages and reverify backing files, including
    /// cached pages. A cached result stays hidden until its content is verified.
    pub fn refresh(&mut self) -> Result<(), Fault> {
        self.field.retry();
        self.io_generation = self
            .io_generation
            .checked_add(1)
            .ok_or_else(|| Fault::resource_limit("light I/O generations"))?;
        self.inputs.clear();
        let positions: Vec<_> = self.terrain.positions().collect();
        for pos in positions {
            self.terrain.suspend(pos);
        }
        Ok(())
    }
    pub fn input_status(&self, pos: SectionPos) -> Option<LightInputStatus> {
        self.inputs.get(&pos).map(|p| p.status)
    }
    pub fn diagnostics(&self) -> LightFieldDiagnostics {
        self.field.diagnostics()
    }
    pub fn optics_identity(&self) -> Digest {
        self.optics.as_deref().map_or(
            self.terrain.binding().compatibility.rules_digest,
            LightOpticsTable::identity,
        )
    }
    pub fn input_bytes(&self) -> usize {
        self.terrain.allocated_bytes()
    }
    pub fn resource_diagnostics(&self) -> HostLightDiagnostics {
        HostLightDiagnostics {
            field: self.field.diagnostics(),
            input_bytes: self.input_bytes(),
            input_peak: self.input_peak,
            io_pending: self.in_flight != 0,
            io_in_flight: self.in_flight,
            input_sections: self.needed.len(),
            admitted_links: self.interests.values().map(BTreeSet::len).sum(),
            admission_denials: self.admission_denials,
            reserved_bytes: self.reserved_bytes,
        }
    }
    pub fn publication(
        &self,
        consumer: u128,
        target: SectionPos,
        host: &ExecutionHost,
    ) -> Option<&Arc<LightPublication>> {
        // Source transitions must be observed before exposing an old publication.
        if host.binding() != self.terrain.binding() {
            return None;
        }
        let optics: &dyn LightOptics = match self.optics.as_deref() {
            Some(table) => table,
            None => host.world().registry(),
        };
        self.field
            .publication(consumer, target, &self.terrain, optics)
            .filter(|publication| {
                publication
                    .sky
                    .dependencies()
                    .chain(publication.emitted.dependencies())
                    .all(|p| {
                        self.inputs
                            .get(&p)
                            .is_some_and(|input| source(host, p) == input.source)
                    })
            })
    }

    pub fn poll(&mut self, host: &ExecutionHost, work: usize) -> Result<Option<SectionPos>, Fault> {
        Ok(self
            .poll_with_budget(
                host,
                HostLightWorkBudget {
                    input_checks: usize::MAX,
                    validation_checks: usize::MAX,
                    readiness_checks: usize::MAX,
                    solver_work: work,
                },
            )?
            .published)
    }
    /// Budgeted input and target scans rotate independently. Publication also
    /// rechecks authority source stamps, so an unvisited changed source cannot
    /// escape through an older publication while its input scan is deferred.
    pub fn poll_with_budget(
        &mut self,
        host: &ExecutionHost,
        budget: HostLightWorkBudget,
    ) -> Result<HostLightProgress, Fault> {
        let mut progress = HostLightProgress::default();
        if host.binding() != self.terrain.binding() {
            return Err(Fault::invalid_data("shared lighting world changed"));
        }
        for _ in 0..self.in_flight.min(budget.input_checks) {
            match self.loader.receiver.try_recv() {
                Ok(result) => {
                    progress.input_checks += 1;
                    self.in_flight -= 1;
                    if result.generation == self.io_generation
                        && self.needed.contains(&result.pos)
                        && source(host, result.pos) == Source::Parked(result.digest)
                    {
                        let accepted = result.result.and_then(|page| {
                            if page.meta.cursor.world != host.binding().world
                                || page.meta.compatibility != host.binding().compatibility
                                || page.side != self.terrain.side()
                                || page.bounds != self.terrain.bounds()
                                || page.sections.len() != 1
                                || page.sections[0].pos != result.pos
                            {
                                return Err(Fault::invalid_data("incompatible light page"));
                            }
                            SnapshotArchive::validate_legacy(
                                &page,
                                &ArchiveManifest::default(),
                                self.page_budget,
                            )?;
                            self.terrain.insert_snapshot(
                                &page.sections[0],
                                &page.states,
                                host.world().registry(),
                            )
                        });
                        let status = match accepted {
                            Ok(_) => LightInputStatus::Ready,
                            Err(e) => {
                                self.terrain.remove(result.pos);
                                if e.code == FaultCode::ResourceLimit {
                                    LightInputStatus::Budget
                                } else {
                                    LightInputStatus::Corrupt
                                }
                            }
                        };
                        self.inputs.insert(
                            result.pos,
                            Input {
                                source: Source::Parked(result.digest),
                                status,
                            },
                        );
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(Fault::invalid_data("lighting page worker stopped"));
                }
            }
        }
        let scan = (budget.input_checks - progress.input_checks).min(self.input_order.len());
        let mut positions = Vec::with_capacity(scan);
        for _ in 0..scan {
            positions.push(self.input_order[self.input_cursor]);
            self.input_cursor = (self.input_cursor + 1) % self.input_order.len();
        }
        progress.input_checks += scan;
        for &pos in &positions {
            if self
                .inputs
                .get(&pos)
                .is_none_or(|i| i.source != source(host, pos))
            {
                self.terrain.suspend(pos);
            }
        }
        let mut captured = 0;
        for &pos in &positions {
            let current = source(host, pos);
            if self.inputs.get(&pos).is_some_and(|i| i.source == current) {
                continue;
            }
            if captured == self.limits.pages_per_poll {
                continue;
            }
            match current {
                Source::Resident(..) => {
                    captured += 1;
                    let accepted = self.terrain.insert(
                        pos,
                        host.world()
                            .store()
                            .section(pos)
                            .expect("resident source")
                            .clone(),
                        host.world().registry(),
                    );
                    let status = match accepted {
                        Ok(_) => LightInputStatus::Ready,
                        Err(e) => {
                            self.terrain.remove(pos);
                            if e.code == FaultCode::ResourceLimit {
                                LightInputStatus::Budget
                            } else {
                                LightInputStatus::Corrupt
                            }
                        }
                    };
                    self.inputs.insert(
                        pos,
                        Input {
                            source: current,
                            status,
                        },
                    );
                }
                Source::Missing => {
                    self.terrain.remove(pos);
                    self.inputs.insert(
                        pos,
                        Input {
                            source: current,
                            status: LightInputStatus::Missing,
                        },
                    );
                }
                Source::Parked(digest) => {
                    if self.in_flight < self.io_capacity {
                        self.loader
                            .sender
                            .as_ref()
                            .expect("open page worker")
                            .try_send((self.io_generation, pos, digest))
                            .map_err(invalid)?;
                        self.in_flight += 1;
                        self.inputs.insert(
                            pos,
                            Input {
                                source: current,
                                status: LightInputStatus::Pending,
                            },
                        );
                        captured += 1;
                    }
                }
            }
        }
        let optics: &dyn LightOptics = match self.optics.as_deref() {
            Some(table) => table,
            None => host.world().registry(),
        };
        progress.validation_checks =
            self.field
                .inputs_changed_bounded(&self.terrain, optics, budget.validation_checks);
        self.input_peak = self.input_peak.max(self.input_bytes());
        if budget.readiness_checks == 0 || budget.solver_work == 0 {
            return Ok(progress);
        }
        let mut ready = BTreeMap::new();
        let mut columns = BTreeMap::new();
        let mut selected: Option<(u128, SectionPos)> = None;
        let active = self.field.active_target();
        if let Some(target) = active {
            let rank = self.target_ranks.get(&target).copied();
            ready.insert(
                target,
                self.ready_priority(host, target, rank, &mut columns)?,
            );
            progress.readiness_checks += 1;
        }
        let scan =
            (budget.readiness_checks - progress.readiness_checks).min(self.target_order.len());
        for _ in 0..scan {
            let (rank, target) = self.target_order[self.target_cursor];
            self.target_cursor = (self.target_cursor + 1) % self.target_order.len();
            progress.readiness_checks += 1;
            if Some(target) == active || !self.field.needs_work(target) {
                continue;
            }
            let priority = self.ready_priority(host, target, Some(rank), &mut columns)?;
            ready.insert(target, priority);
            if let Some(rank) = priority {
                selected = Some(selected.map_or((rank, target), |old| old.min((rank, target))));
            }
        }
        // Ordinary invalidation cancels stale work; it is not a server failure.
        let stale = self.field.diagnostics().stale;
        let result = match self.field.poll_selected(
            &self.terrain,
            optics,
            budget.solver_work,
            selected.map(|(_, p)| p),
            |p| ready.get(&p).copied().flatten(),
        ) {
            Err(e)
                if e.code == FaultCode::InvalidData && self.field.diagnostics().stale > stale =>
            {
                Ok(None)
            }
            other => other,
        }?;
        progress.published = result;
        Ok(progress)
    }
    fn ready_priority(
        &self,
        host: &ExecutionHost,
        target: SectionPos,
        rank: Option<u128>,
        columns: &mut BTreeMap<(i64, i64), bool>,
    ) -> Result<Option<u128>, Fault> {
        let Some(rank) = rank else {
            return Ok(None);
        };
        let column = (target.x, target.z);
        let verified = if let Some(&verified) = columns.get(&column) {
            verified
        } else {
            let envelope = self.dependencies(&[target].into())?;
            let verified = envelope.iter().all(|p| {
                self.inputs.get(p).is_some_and(|i| {
                    matches!(
                        i.status,
                        LightInputStatus::Ready | LightInputStatus::Missing
                    ) && source(host, *p) == i.source
                })
            });
            columns.insert(column, verified);
            verified
        };
        Ok(verified.then_some(rank))
    }
}
fn distance(pos: SectionPos, center: SectionPos) -> u128 {
    u128::from(pos.x.abs_diff(center.x).max(pos.z.abs_diff(center.z)))
}
fn invalid(e: impl std::fmt::Display) -> Fault {
    Fault::invalid_data(e.to_string())
}
