//! Background preparation of a single durable generated parking batch.
use std::{collections::BTreeSet, thread::JoinHandle};

use torse_sim::durable::SectionChange;
use torse_storage::CommitJournal;
use torse_types::*;
use torse_world::Snapshot;

use crate::{
    CommitStep, HostCheckpointStore, InvalidationSink, Server,
    execution::{ExecutionHost, ExecutionModel, streaming::reference},
    installs::SectionInstallLimits,
    section_pages::SectionPages,
};

#[derive(Debug)]
pub enum ParkingProgress {
    Idle,
    Pending,
    Finished {
        commit: CommitStep,
        /// File-data synchronizations for the bundle. Directory metadata may
        /// need its own barrier on platforms that support it.
        page_file_syncs: u32,
    },
}

struct SavedBatch {
    changes: Vec<SectionChange>,
    page_file_syncs: u32,
}

pub(crate) struct ParkingJob {
    worker: Option<JoinHandle<Result<SavedBatch, Fault>>>,
    _memory: Option<MemoryLease>,
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
    /// Submit one ordered batch of generated pages at the current generation
    /// cut. The worker persists their immutable bytes without borrowing the host.
    /// IDs must extend the current allocator, in page order. A change in the
    /// allocator while the job runs causes rejection at publication.
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
        let mut positions = BTreeSet::new();
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
        let worker = std::thread::Builder::new()
            .name("torse-generated-parking".into())
            .spawn(move || {
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
                })
            })
            .map_err(|error| {
                Fault::new(
                    FaultCode::StorageUnavailable,
                    format!("start generated parking: {error}"),
                )
            })?;
        self.parking = Some(ParkingJob {
            worker: Some(worker),
            _memory: memory,
        });
        Ok(())
    }

    /// Poll the background job. A ready batch publishes at a tick boundary
    /// and waits for one journal barrier before returning durable success.
    pub fn poll_generated_parking(
        &mut self,
        sink: &mut impl InvalidationSink,
    ) -> Result<ParkingProgress, Fault> {
        let host = self.authority()?;
        let Some(job) = self.parking.as_ref() else {
            return Ok(ParkingProgress::Idle);
        };
        if !job.worker.as_ref().expect("parking worker").is_finished()
            || !host.residency_ready(false)
        {
            return Ok(ParkingProgress::Pending);
        }
        let mut job = self.parking.take().expect("parking job");
        let saved = job
            .worker
            .take()
            .expect("parking worker")
            .join()
            .map_err(|_| {
                Fault::new(
                    FaultCode::StorageUnavailable,
                    "generated parking worker panicked",
                )
            })??;
        self.authority.queue_parked_sections(saved.changes)?;
        let commit = self.drive_one(sink)?;
        Ok(ParkingProgress::Finished {
            commit,
            page_file_syncs: saved.page_file_syncs,
        })
    }
}
