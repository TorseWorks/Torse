//! Dedicated writer for bounded batches of immutable resident snapshots.
use super::*;
use std::collections::BTreeMap;

pub(super) struct RetireResult {
    pub(super) pages: Vec<Completed>,
    pub(super) syncs: u32,
}

type RetireResponse = Result<RetireResult, (SectionPos, Fault)>;

pub(super) struct RetireWorker {
    pub(super) sender: Option<mpsc::SyncSender<Vec<Snapshot>>>,
    pub(super) receiver: mpsc::Receiver<RetireResponse>,
    handle: Option<JoinHandle<()>>,
}

impl RetireWorker {
    pub(super) fn new(pages: SectionPages) -> Result<Self, Fault> {
        let (sender, requests) = mpsc::sync_channel::<Vec<Snapshot>>(1);
        let (results, receiver) = mpsc::sync_channel(1);
        let handle = thread::Builder::new()
            .name("torse-page-retire".into())
            .spawn(move || {
                while let Ok(batch) = requests.recv() {
                    let pos = batch[0].sections[0].pos;
                    let result = pages
                        .put_bundle(&batch)
                        .map(|(digests, synced)| RetireResult {
                            pages: batch
                                .into_iter()
                                .zip(digests)
                                .map(|(page, digest)| Completed {
                                    page,
                                    digest,
                                    load: false,
                                    fresh: false,
                                })
                                .collect(),
                            syncs: u32::from(synced),
                        });
                    if results.send(result.map_err(|error| (pos, error))).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| Fault::resource_limit(format!("start retire worker: {error}")))?;
        Ok(Self {
            sender: Some(sender),
            receiver,
            handle: Some(handle),
        })
    }
}

impl Drop for RetireWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl ResidencyScheduler {
    pub(super) fn begin_retire<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        &mut self,
        server: &Server<ExecutionHost, J, S>,
        required: &BTreeSet<SectionPos>,
        unit: u64,
        read_needed: bool,
    ) -> Result<(), Fault> {
        let host = server.authority()?;
        let store = host.world().store();
        // A change of interest can request a read while this write is pending.
        let reserve_read = read_needed || self.config.max_io_bytes / unit >= 2;
        let budget = self.config.max_io_bytes - if reserve_read { unit } else { 0 };
        let count = self.batch_pages.min((budget / unit) as usize);
        let Some(first) = store.positions().find(|pos| {
            !required.contains(pos) && store.phase(*pos) == Some(torse_world::SectionPhase::Ready)
        }) else {
            return Ok(());
        };
        if count == 0 {
            return Ok(());
        }
        let charge = unit * count as u64;
        let memory = match host
            .memory_pool()
            .map(|pool| pool.reserve(MemoryClass::Codec, charge, EvictionRank::Pinned))
            .transpose()
        {
            Ok(value) => value,
            Err(error) if error.code == FaultCode::ResourceLimit && read_needed => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut batch = Vec::with_capacity(count);
        let mut observed = BTreeMap::new();
        let mut encoded = 12u64; // bundle header
        for pos in store
            .positions()
            .filter(|pos| {
                !required.contains(pos)
                    && store.phase(*pos) == Some(torse_world::SectionPhase::Ready)
            })
            .take(count)
        {
            let page = host.world().capture_section(pos, self.pages.budget())?;
            let cost = self.pages.measure(&page, self.pages.budget().max_bytes)?;
            encoded = encoded
                .saturating_add(36)
                .saturating_add(cost.encoded_bytes);
            if encoded > self.pages.budget().max_bytes {
                break;
            }
            observed.insert(pos, store.revision(pos).expect("ready revision"));
            batch.push(page);
        }
        if batch.is_empty() {
            return Err(Fault::resource_limit(format!(
                "retirement bundle cannot fit {first:?}"
            )));
        }
        self.retire_worker
            .sender
            .as_ref()
            .expect("retire worker")
            .try_send(batch)
            .map_err(|_| {
                Fault::new(
                    FaultCode::StorageUnavailable,
                    "retire worker queue unavailable",
                )
            })?;
        self.retire_charge = charge;
        self.retire_started = Some(Instant::now());
        self.retire_memory = memory;
        self.retire_observed = observed;
        Ok(())
    }

    pub(super) fn poll_retire<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        &mut self,
        server: &mut Server<ExecutionHost, J, S>,
        sink: &mut impl InvalidationSink,
        required: &BTreeSet<SectionPos>,
        global: bool,
    ) -> Result<Option<ResidencyProgress>, Fault> {
        if self.retire_ready.is_none() {
            match self.retire_worker.receiver.try_recv() {
                Ok(Ok(result)) => self.retire_ready = Some(result),
                Ok(Err((pos, error))) => {
                    self.release_retire();
                    return Ok(Some(ResidencyProgress::Batch(ResidencyBatchReport {
                        committed: Vec::new(),
                        durability: server.durability(),
                        blocked: None,
                        failure: Some((pos, error)),
                        page_file_syncs: 0,
                    })));
                }
                Err(mpsc::TryRecvError::Empty) => return Ok(None),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.release_retire();
                    return Err(Fault::new(
                        FaultCode::StorageUnavailable,
                        "retire worker disconnected",
                    ));
                }
            }
        }
        if !global && !server.authority()?.residency_ready(true) {
            return Ok(None);
        }
        let mut ready = self.retire_ready.take().expect("completed retire writer");
        let store = server.authority()?.world().store();
        ready.pages.retain(|entry| {
            let pos = entry.page.sections[0].pos;
            !global
                && !required.contains(&pos)
                && store.phase(pos) == Some(torse_world::SectionPhase::Ready)
                && store.revision(pos).as_ref() == self.retire_observed.get(&pos)
        });
        let result = server.apply_residency_batch(&self.pages, ready.pages, sink);
        if result.is_err() {
            self.release_retire();
        }
        let (mut report, retry) = result?;
        report.page_file_syncs = ready.syncs;
        if retry.is_empty() {
            self.release_retire();
        } else {
            self.retire_ready = Some(RetireResult {
                pages: retry,
                syncs: 0,
            });
        }
        Ok(Some(ResidencyProgress::Batch(report)))
    }

    fn release_retire(&mut self) {
        self.retire_charge = 0;
        self.retire_started = None;
        self.retire_memory = None;
        self.retire_observed.clear();
    }
}
