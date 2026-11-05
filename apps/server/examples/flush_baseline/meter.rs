//! Example-local observers: no production interface or persistence policy changes.
use std::{cell::Cell, fs::File, io, io::Write, rc::Rc, time::Instant};
use torse_server::{bootstrap as base, execution::ExecutionModel};
use torse_sim::durable::RuntimeUpdate;
use torse_storage::{CommitJournal, Journal, JournalBackend, MemoryBackend};
use torse_types::*;

#[derive(Clone, Copy, Debug, Default)]
pub struct Counts {
    pub requests: u64,
    pub hits: u64,
    pub syncs: u64,
    pub file_syncs: u64,
    pub records: u64,
    pub bytes: u64,
    pub writes: u64,
    pub written: u64,
    pub sync_ns: u64,
    pub append_ns: u64,
    pub flush_ns: u64,
    pub freezes: u64,
    pub advances: u64,
    pub tasks: u64,
}

#[derive(Clone)]
pub struct Meter {
    pub counts: Rc<Cell<Counts>>,
    pub enabled: bool,
}
impl Meter {
    pub fn new(enabled: bool) -> Self {
        Self {
            counts: Rc::new(Cell::new(Counts::default())),
            enabled,
        }
    }
    pub fn reset(&self) {
        self.counts.set(Counts::default());
    }
    fn update(&self, f: impl FnOnce(&mut Counts)) {
        if self.enabled {
            let mut c = self.counts.get();
            f(&mut c);
            self.counts.set(c);
        }
    }
    fn start(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }
}
fn elapsed(start: Option<Instant>) -> u64 {
    start.map_or(0, |s| {
        s.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
    })
}

/// A retained memory control or an explicitly unsegmented file adapter.
/// File writes and sync_all match the legacy FileBackend operations. Segment
/// rotation/reclamation and directory synchronization are outside this experiment.
pub enum Store {
    Memory(MemoryBackend),
    File(File),
}
pub struct Backend {
    pub store: Store,
    pub meter: Meter,
}
struct Writes<'a> {
    file: &'a mut File,
    meter: &'a Meter,
}
impl Write for Writes<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let result = self.file.write(bytes);
        self.meter.update(|c| {
            c.writes += 1;
            if let Ok(n) = &result {
                c.written += *n as u64;
            }
        });
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
impl JournalBackend for Backend {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        let start = self.meter.start();
        let result = match &mut self.store {
            Store::Memory(store) => store.append(bytes),
            Store::File(file) => Writes {
                file,
                meter: &self.meter,
            }
            .write_all(bytes),
        };
        self.meter.update(|c| {
            c.append_ns += elapsed(start);
            if result.is_ok() {
                c.records += 1;
                c.bytes += bytes.len() as u64;
            }
        });
        result
    }
    fn sync(&mut self) -> io::Result<()> {
        let start = self.meter.start();
        self.meter.update(|c| {
            c.syncs += 1;
            c.file_syncs += u64::from(matches!(self.store, Store::File(_)));
        });
        let result = match &mut self.store {
            Store::Memory(store) => store.sync(),
            Store::File(file) => file.sync_all(),
        };
        self.meter.update(|c| c.sync_ns += elapsed(start));
        result
    }
}

type Inner = Journal<ExecutionModel, base::Codec, Backend>;
pub struct Observed {
    pub inner: Inner,
    pub meter: Meter,
}
impl CommitJournal<ExecutionModel> for Observed {
    type Staged = <Inner as CommitJournal<ExecutionModel>>::Staged;
    fn try_stage(
        &mut self,
        record: &ModelCommit<ExecutionModel>,
    ) -> Result<Self::Staged, StageError> {
        assert!(!matches!(
            record.changes.outcome,
            Some(torse_sim::durable::ReplyStatus::Rejected(_))
        ));
        let result = self.inner.try_stage(record);
        if result.is_ok() {
            self.meter.update(|c| match &record.changes.runtime {
                RuntimeUpdate::Full(state) if state.work.is_some() => c.freezes += 1,
                RuntimeUpdate::Full(_) => c.advances += 1,
                RuntimeUpdate::Progress(_) => c.tasks += 1,
            });
        }
        result
    }
    fn accept(&mut self, staged: Self::Staged) -> Result<(), Fault> {
        self.inner.accept(staged)
    }
    fn flush_through(&mut self, target: CommitCursor) -> Result<Durability, Fault> {
        let covered = self.inner.durability().covers(target);
        let start = self.meter.start();
        let result = self.inner.flush_through(target);
        self.meter.update(|c| {
            c.requests += 1;
            c.hits += u64::from(covered && result.is_ok());
            c.flush_ns += elapsed(start);
        });
        result
    }
    fn durability(&self) -> Durability {
        self.inner.durability()
    }
    fn reclaim(&mut self, floor: CommitCursor) -> Result<(), Fault> {
        self.inner.reclaim(floor)
    }
}
