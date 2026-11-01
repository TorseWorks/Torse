//! Persistence scenario: save a real world to disk, then rebuild it in a fresh authority.
//!
//! This is B5's executable evidence. Earlier scenarios exercise the exchange payloads in
//! memory only; here the same payloads pass through `WorldCodec`, the record envelope, a real
//! `FileJournal`, and real `FileCheckpoints`, and recovery starts from the files alone.
//!
//! The run follows the contract order for every commit:
//!
//! 1. The authority prepares a transaction; the world does not change.
//! 2. The journal stages the encoded record, reserving queue capacity.
//! 3. The authority publishes, and the journal accepts the staged token immediately.
//! 4. The journal flushes, and only then is the commit considered durable.
//!
//! After three commits (terrain, entity movement, inventory transfer) every storage handle
//! is dropped. A second authority built from freshly compiled content reopens the
//! checkpoint, restores the snapshot, and replays the journal tail through `Recovery`. The
//! recovered world must equal the live one, and the next commit must continue numbering.
//!
//! The snapshot format is the M1 interim format (world format version 1). This scenario
//! checks save and load behavior; it is not a compatibility promise for files it writes.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use torse_codec::{CodecLimits, MIN_WORLD_DEPTH, WorldCodec};
use torse_content::{compile, m0_fixture_pack};
use torse_inventory::{Inventory, ItemCatalog, ItemStack};
use torse_sim::{OrderingKey, Phase, Transaction, WorldAuthority, changes_from_prepared};
use torse_storage::{
    CheckpointConfig, CheckpointStore, CommitJournal, FileCheckpointBackend, FileCheckpoints,
    FileJournal, JournalConfig, scan_journal,
};
use torse_types::{
    AuthorityData, Binding, BlockPos, CommitCursor, CommitRecord, Compatibility, ContentKey,
    EntityHandle, EntityId, Fault, OutputBudget, SectionPos, SnapshotMeta, TransactionId,
    WorldEpoch, WorldId, WorldSnapshot,
};
use torse_world::{
    BlockStateId, BlockStateRegistry, ChangeSet, ComponentEdit, ComponentSchemas, Position,
    Section, Snapshot, WorldBounds,
};

/// Authority model for the scenario. Input, output, and replica types belong to later stages
/// (B6 and B7); the storage path only needs the snapshot and change payloads.
pub struct WorldModel;

impl AuthorityData for WorldModel {
    type Snapshot = Snapshot;
    type Changes = ChangeSet;
    type Input = ();
    type Output = ();
    type ReplicaSnapshot = ();
    type ReplicaChanges = ();
}

type Codec = WorldCodec<WorldModel>;
type Journal = FileJournal<WorldModel, Codec>;
type Checkpoints = FileCheckpoints<WorldModel, Codec>;

const WORLD: WorldId = WorldId(43);
const SIDE: u32 = 16;

/// Large enough for a few dense 16^3 sections (8 KiB of cells each) plus entities.
const LIMITS: CodecLimits = CodecLimits {
    max_frame_bytes: 1 << 20,
    max_field_bytes: 1 << 16,
    max_depth: MIN_WORLD_DEPTH,
};

const BUDGET: OutputBudget = OutputBudget {
    max_items: 1 << 16,
    max_bytes: 1 << 20,
};

/// Acceptance counters printed by the command-line runner.
#[derive(Debug, Default)]
pub struct PersistenceReport {
    /// Encoded length of the published checkpoint envelope.
    pub snapshot_bytes: u64,
    /// Sections, non-empty states, and entities held by the checkpoint.
    pub sections: usize,
    pub states: usize,
    pub entities: usize,
    /// Complete journal records and their total framed length after the run.
    pub journal_records: u64,
    pub journal_bytes: u64,
    /// Position reached by replaying the journal, and the next commit after recovery.
    pub recovered_seq: u64,
    pub continued_seq: u64,
    /// The snapshot read back from disk equals the one that was published.
    pub loaded_equal: bool,
    /// The recovered world equals the live world, including entities and components.
    pub recovered_equal: bool,
}

type ScenarioResult<T> = Result<T, Box<dyn std::error::Error>>;

/// Run the scenario in a fresh scratch directory and remove it on success.
///
/// On failure the directory is kept and named in the error, so the files can be inspected.
pub fn run() -> ScenarioResult<PersistenceReport> {
    let root = scratch_directory()?;
    match exercise(&root) {
        Ok(report) => {
            remove_scratch(&root)?;
            Ok(report)
        }
        Err(error) => Err(format!("{error} (files kept in {})", root.display()).into()),
    }
}

fn exercise(root: &Path) -> ScenarioResult<PersistenceReport> {
    let mut report = PersistenceReport::default();
    let journal_path = root.join("journal.log");
    let checkpoint_root = root.join("checkpoints");
    let codec = Codec::new(LIMITS)?;

    // First process: build a world, publish its baseline, and commit three changes ---------
    let mut live = World::build()?;
    let binding = live.binding(1);
    let baseline = CommitCursor::initial(WORLD);

    // The journal exists, empty and synchronized, before the first checkpoint names it.
    let mut journal = Journal::create(&journal_path, binding, baseline, codec, journal_config())?;
    let mut checkpoints = Checkpoints::open(
        binding,
        codec,
        checkpoint_config(),
        FileCheckpointBackend::open(&checkpoint_root)?,
    )?;
    let published = live.authority.capture_snapshot(live.meta())?;
    report.sections = published.sections.len();
    report.states = published.states.len();
    report.entities = published.entities.len();
    checkpoints.publish_checkpoint(&wrap(&published), journal.durability())?;

    let stone = live.state("base:stone")?;
    let glass = live.state("base:glass_slab")?;
    let mut place = transaction(1, key("base:scenario"));
    place
        .read_block(BlockPos::new(3, 1, 3), BlockStateId::EMPTY)
        .write_block(BlockPos::new(3, 1, 3), stone)
        .read_block(BlockPos::new(-2, 2, 5), BlockStateId::EMPTY)
        .write_block(BlockPos::new(-2, 2, 5), glass);
    commit(&mut live.authority, &mut journal, &place)?;

    // The walker crosses the seam between sections x = 0 and x = -1.
    let walker = live.handles[0];
    let entity = live
        .authority
        .entities()
        .get(walker)
        .ok_or("walker is missing")?;
    let mut walk = transaction(2, key("base:scenario"));
    walk.move_entity(
        walker,
        entity.id,
        entity.position,
        [-1.5, 0.0, 0.0],
        [0.3, 0.9, 0.3],
    );
    commit(&mut live.authority, &mut journal, &walk)?;

    let transfer = live.transfer(3, 2)?;
    commit(&mut live.authority, &mut journal, &transfer)?;
    let expected = live.authority.capture_snapshot(live.meta())?;

    // Release every file lock, as a process exit would.
    drop(journal);
    drop(checkpoints);

    // Second process: nothing survives except the files ------------------------------------
    let fresh = World::build_empty()?;
    let binding = fresh.binding(2);
    let mut checkpoints = Checkpoints::open(
        binding,
        codec,
        checkpoint_config(),
        FileCheckpointBackend::open(&checkpoint_root)?,
    )?;
    let current = checkpoints.current().ok_or("no checkpoint was published")?;
    report.snapshot_bytes = current.snapshot_bytes;
    let loaded = checkpoints
        .load_current()?
        .ok_or("the published checkpoint cannot be read")?;
    report.loaded_equal = loaded.state == published;
    if !report.loaded_equal {
        return Err("the checkpoint read back differs from the published snapshot".into());
    }

    // Restore the snapshot, then feed every journal record after it to the same builder.
    let mut recovery = fresh.authority.begin_restore();
    recovery.restore_snapshot(&loaded.state)?;
    let mut file = fs::File::open(&journal_path)?;
    let scan = scan_journal::<WorldModel, _, _, _>(
        &mut file,
        binding,
        loaded.meta.cursor,
        &codec,
        LIMITS,
        BUDGET,
        |record| {
            recovery
                .replay(&record.changes)
                .map(|_| ())
                .map_err(|error| Fault::invalid_data(format!("replay rejected: {error}")))
        },
    )?;
    drop(file);
    report.journal_records = scan.records;
    report.journal_bytes = scan.valid_bytes;
    report.recovered_seq = recovery.seq().0;
    if scan.incomplete_tail || scan.through.seq.0 != report.recovered_seq {
        return Err("the journal scan and the replayed position disagree".into());
    }
    let mut recovered = recovery.finish()?;

    let actual = recovered.capture_snapshot(SnapshotMeta {
        cursor: CommitCursor {
            world: WORLD,
            seq: recovered.seq(),
        },
        compatibility: recovered.coordinator().compatibility(),
        resume_at: recovered.coordinator().resume_point(),
    })?;
    report.recovered_equal = actual == expected;
    if !report.recovered_equal {
        return Err("the recovered world differs from the live world".into());
    }

    // The reopened journal continues after the replayed tail.
    let (mut journal, reopened) = Journal::open(
        &journal_path,
        binding,
        loaded.meta.cursor,
        codec,
        journal_config(),
    )?;
    if reopened.through != scan.through {
        return Err("reopening the journal found a different position".into());
    }
    let mut again = transaction(4, key("base:scenario"));
    again
        .read_block(BlockPos::new(4, 1, 3), BlockStateId::EMPTY)
        .write_block(BlockPos::new(4, 1, 3), stone);
    commit(&mut recovered, &mut journal, &again)?;
    report.continued_seq = recovered.seq().0;
    if report.continued_seq != report.recovered_seq + 1 {
        return Err("the recovered authority did not continue the commit sequence".into());
    }
    Ok(report)
}

/// Prepare, stage, publish, accept, then flush: the contract order for one commit.
///
/// Staging happens before publication, so a record the codec refuses leaves the world and
/// the sequence unchanged. After publication the token is accepted at once; a failure there
/// is terminal for the run, because the world has already changed.
fn commit(
    authority: &mut WorldAuthority,
    journal: &mut Journal,
    transaction: &Transaction,
) -> ScenarioResult<()> {
    let prepared = authority
        .prepare(transaction)
        .map_err(|error| format!("prepare {}: {error:?}", transaction.id.0))?;
    let changes = changes_from_prepared(authority.registry(), &prepared)?;
    let record = CommitRecord {
        meta: changes.meta,
        changes,
    };
    let staged = journal
        .try_stage(&record)
        .map_err(|error| format!("stage {}: {error:?}", transaction.id.0))?;
    let publication = authority.publish(prepared)?;
    journal.accept(staged)?;
    let durable = journal.flush_through(publication.cursor)?;
    if !durable.covers(publication.cursor) {
        return Err(format!("commit {} is not durable", transaction.id.0).into());
    }
    Ok(())
}

/// One authority plus the handles the scenario drives.
struct World {
    authority: WorldAuthority,
    handles: Vec<EntityHandle>,
    ids: Vec<EntityId>,
    catalog: ItemCatalog,
}

impl World {
    /// Compile content and assemble an authority with no sections or entities.
    ///
    /// Recovery uses this directly: it must not depend on anything the first process built
    /// in memory, only on the same content and the files.
    fn build_empty() -> ScenarioResult<Self> {
        let mut pack = m0_fixture_pack();
        torse_inventory::register_content(&mut pack);
        let content = compile(pack).map_err(|errors| format!("content: {errors:?}"))?;
        let catalog = ItemCatalog::new([(key("base:stone"), 64)])
            .map_err(|code| format!("catalog: {code:?}"))?;
        let mut schemas = ComponentSchemas::build(&content);
        catalog.bind_schemas(&mut schemas)?;
        let authority = WorldAuthority::new(
            WORLD,
            BlockStateRegistry::build(&content)?,
            schemas,
            SIDE,
            WorldBounds::new(0, 32)?,
            Compatibility {
                authority_schema: 1,
                rules_digest: content.authority_digest,
                content_digest: content.presentation_digest,
            },
        )?;
        Ok(Self {
            authority,
            handles: Vec::new(),
            ids: Vec::new(),
            catalog,
        })
    }

    /// Two loaded sections, a stone floor, and two entities with inventories.
    fn build() -> ScenarioResult<Self> {
        let mut world = Self::build_empty()?;
        let stone = world.state("base:stone")?;
        for x in [-1, 0] {
            let pos = SectionPos::new(x, 0, 0);
            let store = world.authority.store_mut();
            store.request(pos)?;
            store.install(pos, Section::empty(SIDE)?)?;
            store.finish_load(pos)?;
        }
        for x in -6..6 {
            world
                .authority
                .store_mut()
                .edit(BlockPos::new(x, 0, 3), stone)?;
        }
        let schemas = world.authority.schemas().clone();
        for (x, stack) in [(0.5, Some(3)), (5.0, None)] {
            let entities = world.authority.entities_mut();
            let handle = entities.spawn(Position::new(x, 2.0, 2.0))?;
            let mut inventory = Inventory::new(2).map_err(|code| format!("{code:?}"))?;
            if let Some(count) = stack {
                let stack = ItemStack {
                    data: Default::default(),
                    item: key("base:stone"),
                    count,
                };
                inventory
                    .set(0, Some(stack), &world.catalog)
                    .map_err(|code| format!("{code:?}"))?;
            }
            entities
                .components_mut(handle)
                .ok_or("spawned entity has no components")?
                .insert(&schemas, &torse_inventory::schema(), inventory.fields())?;
            let id = entities.get(handle).ok_or("spawned entity is missing")?.id;
            world.handles.push(handle);
            world.ids.push(id);
        }
        Ok(world)
    }

    fn state(&self, name: &str) -> ScenarioResult<BlockStateId> {
        Ok(self
            .authority
            .registry()
            .default_state(&key(name))
            .ok_or_else(|| format!("{name} is missing"))?)
    }

    /// The live binding. The epoch changes per process; the persisted identity does not.
    fn binding(&self, epoch: u128) -> Binding {
        Binding {
            world: WORLD,
            epoch: WorldEpoch(epoch),
            compatibility: self.authority.coordinator().compatibility(),
        }
    }

    fn meta(&self) -> SnapshotMeta {
        SnapshotMeta {
            cursor: CommitCursor {
                world: WORLD,
                seq: self.authority.seq(),
            },
            compatibility: self.authority.coordinator().compatibility(),
            resume_at: self.authority.coordinator().resume_point(),
        }
    }

    /// Move `count` stone from the first entity's slot 0 to the second entity's slot 0.
    ///
    /// Built as the inventory module would build it: both containers carry their observed
    /// fields as the precondition, and the transaction is authored by the schema owner.
    fn transfer(&self, id: u128, count: u32) -> ScenarioResult<Transaction> {
        let schema = torse_inventory::schema();
        let read = |handle: EntityHandle| -> ScenarioResult<_> {
            Ok(self
                .authority
                .entities()
                .get(handle)
                .and_then(|entity| entity.components.component(&schema))
                .ok_or("inventory component is missing")?
                .clone())
        };
        let (source, destination) = (read(self.handles[0])?, read(self.handles[1])?);
        let mut from = Inventory::from_fields(&source, &self.catalog)
            .map_err(|code| format!("source inventory: {code:?}"))?;
        let mut to = Inventory::from_fields(&destination, &self.catalog)
            .map_err(|code| format!("destination inventory: {code:?}"))?;
        let held = from.slots()[0].clone().ok_or("source slot is empty")?;
        let left = (held.count > count).then(|| held.with_count(held.count - count));
        let moved = held.with_count(count);
        from.set(0, left, &self.catalog)
            .map_err(|code| format!("{code:?}"))?;
        to.set(0, Some(moved), &self.catalog)
            .map_err(|code| format!("{code:?}"))?;

        let mut tx = transaction(id, torse_inventory::owner());
        for (entity, previous, fields) in [
            (self.ids[0], source, from.fields()),
            (self.ids[1], destination, to.fields()),
        ] {
            tx.components.push(ComponentEdit {
                id: entity,
                schema: schema.clone(),
                previous: Some(previous),
                fields: Some(fields),
            });
        }
        Ok(tx)
    }
}

fn transaction(id: u128, source: ContentKey) -> Transaction {
    Transaction::new(
        TransactionId(id),
        source,
        OrderingKey {
            phase: Phase::Interaction,
            rule_priority: 0,
            stable_task_key: id as u64,
            local_sequence: 0,
        },
    )
}

fn wrap(snapshot: &Snapshot) -> WorldSnapshot<Snapshot> {
    WorldSnapshot {
        meta: snapshot.meta,
        state: snapshot.clone(),
    }
}

fn journal_config() -> JournalConfig {
    JournalConfig {
        max_segment_bytes: 16_777_216,
        max_segment_files: 1024,
        max_pending_records: 4,
        max_pending_bytes: 1 << 20,
        codec_budget: BUDGET,
        codec_limits: LIMITS,
    }
}

fn checkpoint_config() -> CheckpointConfig {
    CheckpointConfig {
        codec_limits: LIMITS,
        codec_budget: BUDGET,
        max_manifest_bytes: 1 << 16,
        max_snapshot_files: 4,
    }
}

fn key(value: &str) -> ContentKey {
    ContentKey::parse(value).expect("scenario keys are valid")
}

// Scratch files -----------------------------------------------------------------------------
//
// Every run owns a new directory under the workspace `.work` area, never a system temp path.
// Cleanup removes only the files this scenario creates, one level deep, and never follows a
// computed directory tree, so a wrong path fails instead of deleting unrelated data.

fn scratch_directory() -> ScenarioResult<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.work/persistence");
    fs::create_dir_all(&parent)?;
    let index = NEXT.fetch_add(1, Ordering::Relaxed);
    let root = parent.join(format!("{}-{index}", std::process::id()));
    // `create_dir` fails if the directory exists, so a stale run is never reused.
    fs::create_dir(&root)?;
    fs::create_dir(root.join("checkpoints"))?;
    Ok(root)
}

fn remove_scratch(root: &Path) -> ScenarioResult<()> {
    let checkpoints = root.join("checkpoints");
    for entry in fs::read_dir(&checkpoints)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            return Err(format!("unexpected entry {}", entry.path().display()).into());
        }
        fs::remove_file(entry.path())?;
    }
    fs::remove_dir(&checkpoints)?;
    fs::remove_file(root.join("journal.log"))?;
    fs::remove_dir(root)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistence_scenario_recovers_the_live_world() {
        let report = run().expect("scenario completes");
        assert!(report.loaded_equal);
        assert!(report.recovered_equal);
        assert_eq!(report.journal_records, 3);
        assert_eq!(report.recovered_seq, 3);
        assert_eq!(report.continued_seq, 4);
        assert_eq!(report.sections, 2);
        assert_eq!(report.entities, 2);
    }
}
