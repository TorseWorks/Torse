//! Nonblocking residency scheduling with one bounded, world-owned page I/O worker.
use std::collections::BTreeSet;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use torse_storage::CommitJournal;
use torse_types::*;
use torse_world::{Snapshot, WorldBounds};

use crate::execution::{ExecutionHost, ExecutionModel};
use crate::section_pages::SectionPages;
use crate::{CommitStep, GenerationIdentity, HostCheckpointStore, InvalidationSink, Server};

#[derive(Clone, Copy, Debug)]
pub struct ResidencyConfig {
    pub max_resident: usize,
    /// Reserve page input, codec scratch, decoded output, and handoff before capture.
    pub max_io_bytes: u64,
}
impl ResidencyConfig {
    fn validate(self) -> Result<(), Fault> {
        if self.max_resident == 0 || self.max_resident > 4096 || self.max_io_bytes == 0 {
            return Err(Fault::invalid_data("invalid residency limits"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct GenerationContext {
    pub meta: SnapshotMeta,
    pub side: u32,
    pub bounds: WorldBounds,
    pub next_entity_id: u128,
}

/// Trusted bounded generator, isolated from the mutable authority and its runtime handles.
/// Output depends only on identity, context shape, and position, never execution order.
pub trait GenerationSource: Send + Sync + 'static {
    fn identity(&self) -> GenerationIdentity;
    fn generate(&self, context: GenerationContext, pos: SectionPos) -> Result<Snapshot, Fault>;
}

#[derive(Debug)]
pub enum ResidencyProgress {
    Idle,
    Pending,
    BudgetBlocked,
    CapacityBlocked,
    Unavailable(SectionPos),
    Published(CommitStep),
    Batch(ResidencyBatchReport),
}

/// An explicit durable prefix. Uncommitted desired pages are selected again on
/// the next poll. A fatal journal error instead returns Err and stops the host;
/// only reopening can establish the surviving complete log prefix.
#[derive(Debug)]
pub struct ResidencyBatchReport {
    pub committed: Vec<SectionPos>,
    pub durability: Durability,
    pub blocked: Option<StageError>,
    pub failure: Option<(SectionPos, Fault)>,
}

enum Request {
    Save(Snapshot),
    Load(SectionPos, Digest),
    Generate(SectionPos, GenerationContext),
}
pub(crate) struct Completed {
    pub(crate) page: Snapshot,
    pub(crate) digest: Digest,
    pub(crate) load: bool,
    pub(crate) fresh: bool,
}
struct Worker {
    sender: Option<mpsc::SyncSender<Vec<Request>>>,
    receiver: mpsc::Receiver<Vec<(SectionPos, Result<Completed, Fault>)>>,
    handle: Option<JoinHandle<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.sender.take();
        // At most one job exists. Finish it before the owner's world lease is dropped.
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
impl Worker {
    fn new(
        pages: SectionPages,
        generator: Option<Arc<dyn GenerationSource>>,
    ) -> Result<Self, Fault> {
        let (sender, requests) = mpsc::sync_channel::<Vec<Request>>(1);
        let (results, receiver) = mpsc::sync_channel(1);
        let handle = thread::Builder::new()
            .name("torse-page-io".into())
            .spawn(move || {
                while let Ok(batch) = requests.recv() {
                    let mut completed = Vec::with_capacity(batch.len());
                    for request in batch {
                        let pos = match &request {
                            Request::Save(page) => page.sections[0].pos,
                            Request::Load(pos, _) | Request::Generate(pos, _) => *pos,
                        };
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                            || -> Result<Completed, Fault> {
                                match request {
                                    Request::Save(page) => {
                                        let digest = pages.put(&page)?;
                                        Ok(Completed {
                                            page,
                                            digest,
                                            load: false,
                                            fresh: false,
                                        })
                                    }
                                    Request::Load(pos, digest) => {
                                        let page = pages.get(digest)?;
                                        if page.sections[0].pos != pos {
                                            return Err(Fault::invalid_data(
                                                "loaded page position mismatch",
                                            ));
                                        }
                                        Ok(Completed {
                                            page,
                                            digest,
                                            load: true,
                                            fresh: false,
                                        })
                                    }
                                    Request::Generate(pos, context) => {
                                        let source = generator.as_ref().ok_or_else(|| {
                                            Fault::invalid_data("generation source missing")
                                        })?;
                                        let page = source.generate(context, pos)?;
                                        if page.meta != context.meta
                                            || page.side != context.side
                                            || page.bounds != context.bounds
                                            || page.next_entity_id != context.next_entity_id
                                            || !page.entities.is_empty()
                                            || page.sections.len() != 1
                                            || page.sections[0].pos != pos
                                        {
                                            return Err(Fault::invalid_data(
                                                "generated page does not match requested context",
                                            ));
                                        }
                                        let digest = pages.digest(&page)?;
                                        Ok(Completed {
                                            page,
                                            digest,
                                            load: true,
                                            fresh: true,
                                        })
                                    }
                                }
                            },
                        ))
                        .unwrap_or_else(|_| {
                            Err(Fault::new(
                                FaultCode::InvalidState,
                                "page I/O worker panicked",
                            ))
                        });
                        let failed = result.is_err();
                        completed.push((pos, result));
                        if failed {
                            break;
                        }
                    }
                    if results.send(completed).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| Fault::resource_limit(format!("start page I/O worker: {e}")))?;
        Ok(Self {
            sender: Some(sender),
            receiver,
            handle: Some(handle),
        })
    }
}

pub(crate) struct ResidencyScheduler {
    binding: Binding,
    config: ResidencyConfig,
    pages: SectionPages,
    generator: Option<Arc<dyn GenerationSource>>,
    identity: Option<GenerationIdentity>,
    desired: BTreeSet<SectionPos>,
    worker: Worker,
    busy: bool,
    ready: Option<Completed>,
    batch_ready: Vec<(SectionPos, Result<Completed, Fault>)>,
    batch_pages: usize,
    memory: Option<MemoryLease>,
}

impl<J: CommitJournal<ExecutionModel>, S: HostCheckpointStore<ExecutionModel>>
    Server<ExecutionHost, J, S>
{
    /// Configure between drive calls. The worker is owned by the server and cannot outlive
    /// its world lease. Existing generators are verified before a worker starts.
    pub fn enable_residency(
        &mut self,
        config: ResidencyConfig,
        pages: SectionPages,
        generator: Option<Arc<dyn GenerationSource>>,
        desired: &[SectionPos],
    ) -> Result<(), Fault> {
        config.validate()?;
        if !self.authority()?.residency_ready(false) {
            return Err(Fault::invalid_data(
                "configure residency at an idle tick boundary",
            ));
        }
        let desired = interests(
            desired,
            config.max_resident,
            self.authority.world().store().side(),
        )?;
        let identity = generator.as_ref().map(|source| source.identity());
        if let Some(identity) = &identity {
            identity.bind(self.storage_root())?;
        }
        // Drop any previous worker under the same lease, before starting another one.
        self.residency.take();
        let worker = Worker::new(pages.clone(), generator.clone())?;
        self.residency = Some(ResidencyScheduler {
            binding: self.binding(),
            config,
            pages,
            generator,
            identity,
            desired,
            worker,
            busy: false,
            ready: None,
            batch_ready: Vec::new(),
            batch_pages: 1,
            memory: None,
        });
        Ok(())
    }

    pub fn set_residency_interest(&mut self, desired: &[SectionPos]) -> Result<(), Fault> {
        self.authority()?;
        let scheduler = self
            .residency
            .as_mut()
            .ok_or_else(|| Fault::invalid_data("residency is not configured"))?;
        scheduler.desired = interests(
            desired,
            scheduler.config.max_resident,
            self.authority.world().store().side(),
        )?;
        Ok(())
    }

    /// Last admitted host interest, excluding independent event and player pins.
    pub fn residency_interest(&self) -> Option<&BTreeSet<SectionPos>> {
        self.residency.as_ref().map(|scheduler| &scheduler.desired)
    }

    /// Opt into at most 1024 prepared load/install pages per dispatch and durable
    /// batch. max_io_bytes still caps the entire batch, including worker results.
    /// Configure only when idle; default remains the original one-page path.
    pub fn set_residency_batch_pages(&mut self, pages: usize) -> Result<(), Fault> {
        self.authority()?;
        if !(1..=1024).contains(&pages) {
            return Err(Fault::invalid_data("residency batch pages must be 1..1024"));
        }
        let scheduler = self
            .residency
            .as_mut()
            .ok_or_else(|| Fault::invalid_data("residency is not configured"))?;
        if !scheduler.is_idle() {
            return Err(Fault::invalid_data("page I/O is in flight"));
        }
        scheduler.batch_pages = pages;
        Ok(())
    }

    /// One bounded dispatch or publication batch per call. Page reads/decoding
    /// never block this call; accepting ready pages waits for its final durable
    /// barrier. Default is one page. Page preparation failures are retryable;
    /// journal write/sync failures stop the host and require recovery.
    pub fn poll_residency(
        &mut self,
        sink: &mut impl InvalidationSink,
    ) -> Result<ResidencyProgress, Fault> {
        self.authority()?;
        let Some(mut scheduler) = self.residency.take() else {
            return Ok(ResidencyProgress::Idle);
        };
        let result = scheduler.poll(self, sink);
        self.residency = Some(scheduler);
        result
    }
}

fn interests(
    desired: &[SectionPos],
    limit: usize,
    side: u32,
) -> Result<BTreeSet<SectionPos>, Fault> {
    if desired.len() > limit {
        return Err(Fault::resource_limit(
            "residency interest capacity exceeded",
        ));
    }
    for pos in desired {
        pos.to_block(LocalPos { x: 0, y: 0, z: 0 }, side)
            .map_err(|_| Fault::invalid_data("unrepresentable residency position"))?;
    }
    Ok(desired.iter().copied().collect())
}

impl ResidencyScheduler {
    pub(crate) fn is_idle(&self) -> bool {
        !self.busy && self.ready.is_none() && self.batch_ready.is_empty()
    }
    fn poll<J: CommitJournal<ExecutionModel>, S: HostCheckpointStore<ExecutionModel>>(
        &mut self,
        server: &mut Server<ExecutionHost, J, S>,
        sink: &mut impl InvalidationSink,
    ) -> Result<ResidencyProgress, Fault> {
        if server.binding() != self.binding
            || self.generator.as_ref().map(|source| source.identity()) != self.identity
        {
            return Err(Fault::incompatible(
                "residency world or generation identity changed",
            ));
        }
        let tickets = server
            .authority()?
            .event_tickets(false, self.config.max_resident)?;
        let mut required = self.desired.clone();
        for pos in tickets
            .sections
            .into_iter()
            .chain(server.authority()?.player_sections())
        {
            if required.len() >= self.config.max_resident && !required.contains(&pos) {
                return Ok(ResidencyProgress::CapacityBlocked);
            }
            required.insert(pos);
        }
        if self.busy {
            match self.worker.receiver.try_recv() {
                Ok(mut results) => {
                    self.busy = false;
                    if self.batch_pages > 1 {
                        self.batch_ready = results;
                    } else {
                        match results.pop().expect("nonempty worker response").1 {
                            Ok(completed) => self.ready = Some(completed),
                            Err(error) => {
                                self.memory = None;
                                return Err(error);
                            }
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => return Ok(ResidencyProgress::Pending),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.busy = false;
                    self.memory = None;
                    return Err(Fault::new(
                        FaultCode::StorageUnavailable,
                        "page I/O worker disconnected",
                    ));
                }
            }
        }
        if !self.batch_ready.is_empty() {
            if !server.authority.residency_ready(false) {
                return Ok(ResidencyProgress::Pending);
            }
            let mut ready = Vec::new();
            let mut failure = None;
            for (pos, result) in std::mem::take(&mut self.batch_ready) {
                match result {
                    Ok(page) => {
                        let pos = page.page.sections[0].pos;
                        if (page.load && required.contains(&pos))
                            || (!page.load && !required.contains(&pos) && !tickets.global)
                        {
                            ready.push(page);
                        }
                    }
                    Err(error) => {
                        failure = Some((pos, error));
                        break;
                    }
                }
            }
            let applied = server.apply_residency_batch(&self.pages, ready, sink);
            if applied.is_err() {
                self.memory = None;
            }
            let (mut report, retry) = applied?;
            self.batch_ready = retry
                .into_iter()
                .map(|page| (page.page.sections[0].pos, Ok(page)))
                .collect();
            // The ordered worker stops at its first error. The next poll retries
            // desired missing pages; earlier successful pages are already durable.
            if let Some((pos, error)) = failure {
                if !self.batch_ready.is_empty() {
                    self.batch_ready.push((pos, Err(error)));
                } else if report.failure.is_none() && report.blocked.is_none() {
                    report.failure = Some((pos, error));
                }
            }
            if self.batch_ready.is_empty() {
                self.memory = None;
            }
            return Ok(ResidencyProgress::Batch(report));
        }
        if let Some(completed) = self.ready.take() {
            let pos = completed.page.sections[0].pos;
            let wanted = if completed.load {
                required.contains(&pos)
            } else {
                !required.contains(&pos) && !tickets.global
            };
            if !wanted {
                drop(completed);
                self.memory = None;
                return Ok(ResidencyProgress::Pending);
            }
            if !server.authority.residency_ready(!completed.load) {
                self.ready = Some(completed);
                return Ok(ResidencyProgress::Pending);
            }
            // Pure preparation revalidates revisions, parked references, and entity identity.
            let applied = server.apply_residency_page(
                completed.page,
                completed.digest,
                completed.load,
                completed.fresh,
                sink,
            );
            self.memory = None;
            match applied {
                Ok(step) => return Ok(ResidencyProgress::Published(step)),
                Err(error)
                    if error.code == FaultCode::ResourceLimit
                        && server.state() == crate::ServerState::Running =>
                {
                    return Ok(ResidencyProgress::BudgetBlocked);
                }
                Err(error)
                    if error.code == FaultCode::InvalidData
                        && server.state() == crate::ServerState::Running =>
                {
                    return Ok(ResidencyProgress::Pending);
                }
                Err(error) => return Err(error),
            }
        }
        if !server.authority.residency_ready(false) {
            return Ok(ResidencyProgress::Pending);
        }
        let charge = self
            .pages
            .budget()
            .max_bytes
            .checked_mul(4)
            .ok_or_else(|| Fault::resource_limit("page I/O reservation overflow"))?;
        if charge > self.config.max_io_bytes {
            return Ok(ResidencyProgress::BudgetBlocked);
        }
        let page_count = self
            .batch_pages
            .min((self.config.max_io_bytes / charge) as usize)
            .min(
                self.config
                    .max_resident
                    .saturating_sub(server.authority.world().store().len())
                    .max(1),
            );
        let charge = charge
            .checked_mul(page_count as u64)
            .ok_or_else(|| Fault::resource_limit("batch page reservation overflow"))?;
        let memory = if let Some(pool) = server.authority.memory_pool() {
            match pool.reserve(MemoryClass::Codec, charge, EvictionRank::Pinned) {
                Ok(lease) => Some(lease),
                Err(error) if error.code == FaultCode::ResourceLimit => {
                    return Ok(ResidencyProgress::BudgetBlocked);
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let world = server.authority.world();
        let missing = required
            .iter()
            .find(|pos| world.store().section(**pos).is_none())
            .copied();
        let victim = if tickets.global || !server.authority.residency_ready(true) {
            None
        } else {
            world.store().positions().find(|pos| {
                !required.contains(pos)
                    && world.store().phase(*pos) == Some(torse_world::SectionPhase::Ready)
            })
        };
        let request =
            if let Some(pos) = missing.filter(|_| world.store().len() < self.config.max_resident) {
                if let Some(page) = server.authority.parked_sections().get(&pos) {
                    Request::Load(pos, page.digest)
                } else if self.generator.is_some() {
                    Request::Generate(
                        pos,
                        GenerationContext {
                            meta: torse_sim::streaming::page_meta(world),
                            side: world.store().side(),
                            bounds: world.store().bounds(),
                            next_entity_id: world.entities().next_entity_id(),
                        },
                    )
                } else {
                    return Ok(ResidencyProgress::Unavailable(pos));
                }
            } else if let Some(pos) = victim {
                Request::Save(world.capture_section(pos, self.pages.budget())?)
            } else if missing.is_some() || world.store().len() > self.config.max_resident {
                return Ok(ResidencyProgress::CapacityBlocked);
            } else {
                return Ok(ResidencyProgress::Idle);
            };
        let loading = matches!(&request, Request::Load(..) | Request::Generate(..));
        let mut batch = vec![request];
        if loading {
            for pos in required
                .iter()
                .filter(|pos| world.store().section(**pos).is_none())
                .skip(1)
                .take(page_count.saturating_sub(1))
                .copied()
            {
                if let Some(page) = server.authority.parked_sections().get(&pos) {
                    batch.push(Request::Load(pos, page.digest));
                } else if self.generator.is_some() {
                    batch.push(Request::Generate(
                        pos,
                        GenerationContext {
                            meta: torse_sim::streaming::page_meta(world),
                            side: world.store().side(),
                            bounds: world.store().bounds(),
                            next_entity_id: world.entities().next_entity_id(),
                        },
                    ));
                } else {
                    break;
                }
            }
        }
        self.worker
            .sender
            .as_ref()
            .expect("live worker")
            .try_send(batch)
            .map_err(|_| Fault::new(FaultCode::StorageUnavailable, "page I/O queue unavailable"))?;
        self.busy = true;
        self.memory = memory;
        Ok(ResidencyProgress::Pending)
    }
}
