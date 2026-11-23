//! Background preparation of a single durable generated parking batch.
#[cfg(feature = "parking-profile")]
use std::time::{Duration, Instant};
use std::{collections::BTreeSet, thread::JoinHandle};

use torse_sim::durable::{ParkingReservation, SectionChange};
use torse_storage::CommitJournal;
use torse_types::*;
use torse_world::Snapshot;

use crate::{
    CommitStep, HostCheckpointStore, InvalidationSink, Server,
    coordinator::{DurablePublication, PublishedStep},
    execution::{
        ExecutionHost, ExecutionModel,
        streaming::{bind_parked_page, reference},
    },
    installs::SectionInstallLimits,
    section_pages::SectionPages,
};

#[derive(Debug)]
pub enum ParkingProgress {
    Idle,
    Pending,
    Stale {
        position: SectionPos,
    },
    Canceled,
    Finished {
        commit: CommitStep,
        /// File-data synchronizations for the bundle. Directory metadata may
        /// need its own barrier on platforms that support it.
        page_file_syncs: u32,
    },
}

/// Timings for a successfully published generated parking batch.
#[cfg(feature = "parking-profile")]
#[derive(Clone, Copy, Debug, Default)]
pub struct ParkingTimings {
    /// From admission until the background worker started, including ID reservation.
    pub writer_start_wait: Duration,
    /// Encoding, file publication, and worker-side result preparation.
    pub writer_work: Duration,
    /// From worker completion until the host took its result.
    pub result_wait: Duration,
    /// Host validation and publication work, including attempts blocked by a busy journal.
    pub publication_work: Duration,
    /// Waiting for the published journal record to become durable.
    pub durability_wait: Duration,
}

struct SavedBatch {
    changes: Vec<SectionChange>,
    page_file_syncs: u32,
    #[cfg(feature = "parking-profile")]
    writer_interval: (Instant, Instant),
}

pub(crate) struct ParkingJob {
    worker: Option<JoinHandle<Result<SavedBatch, Fault>>>,
    allocator: u128,
    input: Option<Vec<Snapshot>>,
    saved: Option<SavedBatch>,
    pages: SectionPages,
    canceled: bool,
    #[cfg(test)]
    before_write: Option<std::sync::mpsc::Receiver<()>>,
    positions: Vec<(SectionPos, Option<Revision>)>,
    _memory: Option<MemoryLease>,
    #[cfg(feature = "parking-profile")]
    admitted_at: Instant,
    #[cfg(feature = "parking-profile")]
    timings: ParkingTimings,
}

impl Drop for ParkingJob {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl<J: CommitJournal<ExecutionModel>, S: HostCheckpointStore<ExecutionModel>>
    Server<ExecutionHost, J, S>
{
    /// The most recent successful generated parking batch, if one has finished.
    #[cfg(feature = "parking-profile")]
    pub fn last_generated_parking_timings(&self) -> Option<ParkingTimings> {
        self.last_parking_timings
    }

    /// Submit one ordered batch of generated pages at the current generation
    /// cut. The first poll admits attached IDs durably before starting the writer.
    /// Input IDs extend the generation cut's allocator in page order.
    pub fn begin_generated_parking(
        &mut self,
        pages: SectionPages,
        input: Vec<Snapshot>,
        limits: SectionInstallLimits,
    ) -> Result<(), Fault> {
        let host = self.authority()?;
        if self.parking.is_some()
            || self
                .residency
                .as_ref()
                .is_some_and(|scheduler| !scheduler.is_idle())
            || !host.residency_ready(false)
        {
            return Err(Fault::invalid_data(
                "generated parking requires idle page I/O and tick boundary",
            ));
        }
        if !(1..=1024).contains(&limits.max_pages)
            || input.is_empty()
            || input.len() > limits.max_pages
            || input.len() > host.section_install_limit
            || limits.max_bytes < 4
        {
            return Err(Fault::resource_limit("generated parking batch limits"));
        }
        let memory = host
            .memory_pool()
            .map(|pool| pool.reserve(MemoryClass::Codec, limits.max_bytes, EvictionRank::Pinned))
            .transpose()?;
        let meta = torse_sim::streaming::page_meta(host.world());
        let allocator = host.world().entities().next_entity_id();
        let mut positions = BTreeSet::new();
        let mut observed = Vec::with_capacity(input.len());
        let mut bytes = 0u64;
        for page in &input {
            let [section] = page.sections.as_slice() else {
                return Err(Fault::invalid_data("generated parking page shape"));
            };
            if page.meta != meta
                || page.side != host.world().store().side()
                || page.bounds != host.world().store().bounds()
                || !positions.insert(section.pos)
                || host.parked_sections().contains_key(&section.pos)
                || host.world().store().phase(section.pos).is_some()
            {
                return Err(Fault::invalid_data(
                    "generated parking generation cut or position",
                ));
            }
            let cost = pages.measure(page, limits.max_bytes / 4)?;
            observed.push((
                section.pos,
                host.world().store().retired_revision(section.pos),
            ));
            let components = page
                .entities
                .iter()
                .flat_map(|e| &e.components)
                .fold(0u64, |n, c| n.saturating_add(c.accounted_bytes()));
            bytes = bytes
                .saturating_add(cost.owned_bytes.max(cost.encoded_bytes))
                .saturating_add(components)
                .saturating_add(512);
            if bytes > limits.max_bytes / 4 {
                return Err(Fault::resource_limit("generated parking input bytes"));
            }
        }
        self.parking = Some(ParkingJob {
            worker: None,
            allocator,
            input: Some(input),
            saved: None,
            pages,
            canceled: false,
            #[cfg(test)]
            before_write: None,
            positions: observed,
            _memory: memory,
            #[cfg(feature = "parking-profile")]
            admitted_at: Instant::now(),
            #[cfg(feature = "parking-profile")]
            timings: ParkingTimings::default(),
        });
        Ok(())
    }

    /// Cancel pending publication. Reserved IDs are never returned to the allocator.
    pub fn cancel_generated_parking(&mut self) -> Result<(), Fault> {
        self.authority()?;
        if let Some(job) = self.parking.as_mut() {
            job.canceled = true;
            self.authority.clear_parking_reservation();
        }
        Ok(())
    }

    /// A busy journal leaves the queued input or completed bundle in the job.
    pub fn poll_generated_parking(
        &mut self,
        sink: &mut impl InvalidationSink,
    ) -> Result<ParkingProgress, Fault> {
        let host = self.authority()?;
        let Some(job) = self.parking.as_ref() else {
            return Ok(ParkingProgress::Idle);
        };
        if job
            .worker
            .as_ref()
            .is_some_and(|writer| !writer.is_finished())
            || (!job.canceled && !host.residency_ready(false))
        {
            return Ok(ParkingProgress::Pending);
        }
        let mut job = self.parking.take().expect("parking job");
        let result = self.advance_generated_parking(&mut job, sink);
        if matches!(result, Ok(ParkingProgress::Pending)) {
            self.parking = Some(job);
        } else {
            self.authority.clear_parking_reservation();
        }
        result
    }

    fn advance_generated_parking(
        &mut self,
        job: &mut ParkingJob,
        sink: &mut impl InvalidationSink,
    ) -> Result<ParkingProgress, Fault> {
        if job.canceled {
            return Ok(ParkingProgress::Canceled);
        }
        let world = self.authority.world();
        for &(pos, retired) in &job.positions {
            if world.store().phase(pos).is_some()
                || world.store().retired_revision(pos) != retired
                || self.authority.parked_sections().contains_key(&pos)
            {
                return Ok(ParkingProgress::Stale { position: pos });
            }
        }
        if let Some(input) = job.input.as_ref() {
            let first = world.entities().next_entity_id();
            let mut original = job.allocator;
            for page in input {
                for entity in &page.entities {
                    if entity.id != EntityId(original) {
                        return Err(Fault::invalid_data("invalid generated parking identity"));
                    }
                    original = original
                        .checked_add(1)
                        .ok_or_else(|| Fault::resource_limit("generation allocator exhausted"))?;
                }
                if page.next_entity_id != original {
                    return Err(Fault::invalid_data("invalid generated parking watermark"));
                }
            }
            let count = original - job.allocator;
            if count > 0 {
                let end = first
                    .checked_add(count)
                    .ok_or_else(|| Fault::resource_limit("parking allocator exhausted"))?;
                self.authority
                    .queue_parking_reservation(ParkingReservation { first, end })?;
                match self.drive_one(sink)? {
                    CommitStep::Blocked(StageError::Busy) => return Ok(ParkingProgress::Pending),
                    CommitStep::Blocked(error) => return Err(parking_stage_fault(error)),
                    CommitStep::Durable(_) => {}
                    CommitStep::Idle => {
                        return Err(Fault::invalid_data("parking reservation disappeared"));
                    }
                }
            }
            let mut input = job.input.take().expect("queued generated parking");
            let mut next = first;
            for page in &mut input {
                if !page.entities.is_empty() && first != job.allocator {
                    let mut binding = reference(page, Digest::default());
                    for id in &mut binding.entities {
                        *id = EntityId(next);
                        next += 1;
                    }
                    let bound = bind_parked_page(page, &binding)?;
                    if let std::borrow::Cow::Owned(bound) = bound {
                        *page = bound;
                    }
                }
            }
            job.worker = Some(start_writer(
                job.pages.clone(),
                input,
                #[cfg(test)]
                job.before_write.take(),
            )?);
            return Ok(ParkingProgress::Pending);
        }
        #[cfg(feature = "parking-profile")]
        let publication_started = Instant::now();
        if let Some(worker) = job.worker.take() {
            let saved = worker.join().map_err(|_| {
                Fault::new(
                    FaultCode::StorageUnavailable,
                    "generated parking worker panicked",
                )
            })??;
            #[cfg(feature = "parking-profile")]
            {
                let (started, finished) = saved.writer_interval;
                job.timings.writer_start_wait = started.duration_since(job.admitted_at);
                job.timings.writer_work = finished.duration_since(started);
                job.timings.result_wait = publication_started.duration_since(finished);
            }
            for change in &saved.changes {
                if job.pages.digest(&change.page)? != change.reference.digest {
                    return Err(Fault::invalid_data("generated parking digest mismatch"));
                }
            }
            job.saved = Some(saved);
        }
        let saved = job.saved.take().expect("written parking bundle");
        self.authority.queue_parked_sections(saved.changes)?;
        let publication = self.publish_one(sink, None)?;
        #[cfg(feature = "parking-profile")]
        {
            job.timings.publication_work += publication_started.elapsed();
        }
        match publication {
            PublishedStep::Blocked(StageError::Busy, prepared) => {
                job.saved = Some(SavedBatch {
                    changes: ExecutionHost::take_blocked_parking(prepared),
                    page_file_syncs: saved.page_file_syncs,
                    #[cfg(feature = "parking-profile")]
                    writer_interval: saved.writer_interval,
                });
                Ok(ParkingProgress::Pending)
            }
            PublishedStep::Blocked(error, _) => Err(parking_stage_fault(error)),
            PublishedStep::Published(publication) => {
                #[cfg(feature = "parking-profile")]
                let durability_started = Instant::now();
                let durability = self.flush_to(publication.at)?;
                #[cfg(feature = "parking-profile")]
                {
                    job.timings.durability_wait = durability_started.elapsed();
                    self.last_parking_timings = Some(job.timings);
                }
                Ok(ParkingProgress::Finished {
                    commit: CommitStep::Durable(DurablePublication {
                        publication,
                        durability,
                    }),
                    page_file_syncs: saved.page_file_syncs,
                })
            }
            PublishedStep::Idle => Err(Fault::invalid_data(
                "generated parking publication disappeared",
            )),
        }
    }
}

fn parking_stage_fault(error: StageError) -> Fault {
    match error {
        StageError::TooLarge => Fault::resource_limit("generated parking journal record"),
        StageError::InvalidRecord(fault) | StageError::Stopped(fault) => fault,
        StageError::Busy => unreachable!("busy jobs remain queued"),
    }
}

fn start_writer(
    pages: SectionPages,
    input: Vec<Snapshot>,
    #[cfg(test)] before_write: Option<std::sync::mpsc::Receiver<()>>,
) -> Result<JoinHandle<Result<SavedBatch, Fault>>, Fault> {
    std::thread::Builder::new()
        .name("torse-generated-parking".into())
        .spawn(move || {
            #[cfg(feature = "parking-profile")]
            let started = Instant::now();
            #[cfg(test)]
            if let Some(gate) = before_write {
                gate.recv()
                    .map_err(|_| Fault::invalid_data("parking test gate disconnected"))?;
            }
            let (digests, synced) = pages.put_bundle(&input)?;
            let changes = input
                .into_iter()
                .zip(digests)
                .map(|(page, digest)| SectionChange {
                    reference: reference(&page, digest),
                    page,
                    load: false,
                    fresh: true,
                })
                .collect();
            Ok(SavedBatch {
                changes,
                page_file_syncs: u32::from(synced),
                #[cfg(feature = "parking-profile")]
                writer_interval: (started, Instant::now()),
            })
        })
        .map_err(|error| {
            Fault::new(
                FaultCode::StorageUnavailable,
                format!("start generated parking: {error}"),
            )
        })
}

#[cfg(test)]
#[allow(dead_code)]
#[path = "../examples/dense_install_baseline/workload.rs"]
mod fixture;

#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod support;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoReplicas;
    use std::time::{Duration, Instant};

    #[test]
    fn paused_writer_preserves_terrain_only_and_reserved_pages() {
        let dir = support::Directory::new();
        let (mut server, pages, template) = fixture::create(dir.acquire()).unwrap();
        for (x, has_entities) in [(2, false), (4, true)] {
            let host = server.authority().unwrap();
            let first = host.world().entities().next_entity_id();
            let mut column = fixture::column(host, x, 0, &template, false);
            if !has_entities {
                for page in &mut column {
                    page.entities.clear();
                    page.next_entity_id = first;
                }
            }
            server
                .begin_generated_parking(
                    pages.clone(),
                    column.clone(),
                    SectionInstallLimits {
                        max_pages: 24,
                        max_bytes: 8 * 1024 * 1024,
                    },
                )
                .unwrap();
            let (release, blocked) = std::sync::mpsc::channel();
            server.parking.as_mut().unwrap().before_write = Some(blocked);
            assert!(matches!(
                server.poll_generated_parking(&mut NoReplicas).unwrap(),
                ParkingProgress::Pending
            ));
            let end = first + if has_entities { 3 } else { 0 };
            let host = server.authority().unwrap();
            assert_eq!(host.world().entities().next_entity_id(), end);
            let near = fixture::column(host, x + 1, 0, &template, false);
            server
                .install_section_batch(&pages, near, &mut NoReplicas)
                .unwrap();
            for page in &column {
                assert!(!pages.path(pages.digest(page).unwrap()).exists());
            }
            release.send(()).unwrap();
            let timeout = Instant::now() + Duration::from_secs(10);
            loop {
                match server.poll_generated_parking(&mut NoReplicas).unwrap() {
                    ParkingProgress::Pending => std::thread::yield_now(),
                    ParkingProgress::Finished {
                        page_file_syncs: 1, ..
                    } => break,
                    other => panic!("unexpected parking progress: {other:?}"),
                }
                assert!(Instant::now() < timeout);
            }
            let host = server.authority().unwrap();
            assert_eq!(host.world().entities().next_entity_id(), end + 3);
            for page in column {
                let reference = &host.parked_sections()[&page.sections[0].pos];
                assert_eq!(pages.get(reference.digest).unwrap(), page);
            }
        }
    }
}
