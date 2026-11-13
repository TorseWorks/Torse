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
    LightFieldDiagnostics, LightFieldLimits, LightPublication, LightSource, LightTerrain,
    SharedLightField,
};

#[derive(Clone, Copy, Debug)]
pub struct HostLightLimits {
    pub input_sections: usize,
    pub input_bytes: usize,
    pub pages_per_poll: usize,
    pub field: LightFieldLimits,
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
    limits: HostLightLimits,
    interests: BTreeMap<u128, BTreeSet<SectionPos>>,
    needed: BTreeSet<SectionPos>,
    inputs: BTreeMap<SectionPos, Input>,
    loader: Loader,
    busy: bool,
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
        let reserve = (limits.input_bytes as u64)
            .checked_add(limits.field.cache_bytes as u64)
            .and_then(|n| n.checked_add(field.diagnostics().work_reservation))
            .and_then(|n| n.checked_add(pages.budget().max_bytes.checked_mul(4)?))
            .and_then(|n| n.checked_add(limits.input_sections as u64 * 512))
            .and_then(|n| n.checked_add(limits.field.interests as u64 * 128))
            .ok_or_else(|| Fault::resource_limit("shared lighting reservation"))?;
        let memory = host
            .memory_pool()
            .map(|p| p.reserve(MemoryClass::Mesh, reserve, EvictionRank::Pinned))
            .transpose()?;
        let page_budget = pages.budget();
        let (sender, jobs) = mpsc::sync_channel::<(u64, SectionPos, Digest)>(1);
        let (results, receiver) = mpsc::sync_channel(1);
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
            limits,
            interests: BTreeMap::new(),
            needed: BTreeSet::new(),
            inputs: BTreeMap::new(),
            loader: Loader {
                sender: Some(sender),
                receiver,
                handle: Some(handle),
            },
            busy: false,
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
        self.set_needed(needed);
        Ok(())
    }
    pub fn unsubscribe(&mut self, consumer: u128) {
        self.field.unsubscribe(consumer);
        self.interests.remove(&consumer);
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
    }
    fn dependencies(&self, targets: &BTreeSet<SectionPos>) -> Result<BTreeSet<SectionPos>, Fault> {
        let side = i128::from(self.terrain.side());
        let bounds = self.terrain.bounds();
        let ymin = i128::from(bounds.min_y).div_euclid(side);
        let ymax = (i128::from(bounds.max_y) - 1).div_euclid(side);
        let mut needed = BTreeSet::new();
        for &target in targets {
            let base = i128::from(target.y) * side;
            if base >= i128::from(bounds.max_y) || base + side <= i128::from(bounds.min_y) {
                return Err(Fault::invalid_data("light target outside world bounds"));
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
    pub fn input_bytes(&self) -> usize {
        self.terrain.allocated_bytes()
    }
    pub fn resource_diagnostics(&self) -> HostLightDiagnostics {
        HostLightDiagnostics {
            field: self.field.diagnostics(),
            input_bytes: self.input_bytes(),
            input_peak: self.input_peak,
            io_pending: self.busy,
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
        self.field
            .publication(consumer, target, &self.terrain, host.world().registry())
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
        if host.binding() != self.terrain.binding() {
            return Err(Fault::invalid_data("shared lighting world changed"));
        }
        if self.busy {
            match self.loader.receiver.try_recv() {
                Ok(result) => {
                    self.busy = false;
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
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(Fault::invalid_data("lighting page worker stopped"));
                }
            }
        }
        // Suspend all changed sources before advancing any light job. Captures
        // below are bounded separately from the read-only metadata scan.
        for &pos in &self.needed {
            if self
                .inputs
                .get(&pos)
                .is_none_or(|i| i.source != source(host, pos))
            {
                self.terrain.suspend(pos);
            }
        }
        let mut captured = 0;
        for &pos in &self.needed {
            let current = source(host, pos);
            if self.inputs.get(&pos).is_some_and(|i| i.source == current) {
                continue;
            }
            if captured == self.limits.pages_per_poll {
                break;
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
                    if !self.busy {
                        self.loader
                            .sender
                            .as_ref()
                            .expect("open page worker")
                            .try_send((self.io_generation, pos, digest))
                            .map_err(invalid)?;
                        self.busy = true;
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
        self.field
            .inputs_changed(&self.terrain, host.world().registry());
        self.input_peak = self.input_peak.max(self.input_bytes());
        // A transient source handover must not replace reusable light with a
        // calculation against unknown terrain while verification is pending.
        if self.inputs.len() != self.needed.len()
            || self.inputs.iter().any(|(&p, input)| {
                input.status == LightInputStatus::Pending || source(host, p) != input.source
            })
        {
            return Ok(None);
        }
        // Ordinary invalidation cancels stale work; it is not a server failure.
        let stale = self.field.diagnostics().stale;
        match self
            .field
            .poll(&self.terrain, host.world().registry(), work)
        {
            Err(e)
                if e.code == FaultCode::InvalidData && self.field.diagnostics().stale > stale =>
            {
                Ok(None)
            }
            other => other,
        }
    }
}
fn invalid(e: impl std::fmt::Display) -> Fault {
    Fault::invalid_data(e.to_string())
}
