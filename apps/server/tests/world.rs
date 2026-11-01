//! Real world transactions use the durable host, not a parallel scenario coordinator.

mod support;

use std::cell::Cell;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::rc::Rc;

use support::{Directory, WORLD};
use torse_codec::{CodecLimits, WorldCodec};
use torse_content::{compile, m0_fixture_pack};
use torse_inventory::{Inventory, ItemCatalog, ItemStack};
use torse_server::*;
use torse_sim::{OrderingKey, Phase, Transaction, WorldAuthority};
use torse_storage::*;
use torse_types::*;
use torse_world::*;

type Codec = WorldCodec<LocalWorldModel>;
type Files = FileServer<WorldHost, Codec>;

fn key(value: &str) -> ContentKey {
    ContentKey::parse(value).unwrap()
}

fn policy() -> FileServerConfig {
    let limits = CodecLimits {
        max_frame_bytes: 1_048_576,
        max_field_bytes: 65_536,
        max_depth: 12,
    };
    let budget = OutputBudget::new(8192, 1_048_576);
    FileServerConfig {
        server: ServerConfig {
            snapshot_budget: budget,
            max_invalidations: 0,
            shutdown_max_steps: 8,
            checkpoint_predecessors: 1,
        },
        journal: JournalConfig {
            max_segment_bytes: 16_777_216,
            max_segment_files: 1024,
            max_pending_records: 2,
            max_pending_bytes: 2_097_152,
            codec_budget: budget,
            codec_limits: limits,
        },
        checkpoint: CheckpointConfig {
            codec_limits: limits,
            codec_budget: budget,
            max_manifest_bytes: 8192,
            max_snapshot_files: 4,
        },
    }
}

fn world() -> WorldAuthority {
    let mut pack = m0_fixture_pack();
    torse_inventory::register_content(&mut pack);
    let content = compile(pack).unwrap();
    let mut world = WorldAuthority::new(
        WORLD,
        BlockStateRegistry::build(&content).unwrap(),
        ComponentSchemas::build(&content),
        16,
        WorldBounds::new(0, 32).unwrap(),
        support::compatibility(),
    )
    .unwrap();
    for x in [-1, 0] {
        let pos = SectionPos::new(x, 0, 0);
        world.store_mut().request(pos).unwrap();
        world
            .store_mut()
            .install(pos, Section::empty(16).unwrap())
            .unwrap();
        world.store_mut().finish_load(pos).unwrap();
    }
    let schemas = world.schemas().clone();
    let catalog = ItemCatalog::new([(key("base:stone"), 64)]).unwrap();
    for index in 0..3 {
        let mut inventory = Inventory::new(1).unwrap();
        if index == 0 {
            inventory
                .set(
                    0,
                    Some(ItemStack {
                        data: Default::default(),
                        item: key("base:stone"),
                        count: 1,
                    }),
                    &catalog,
                )
                .unwrap();
        }
        let handle = world
            .entities_mut()
            .spawn(Position::new(0.5 + index as f64, 2.0, 2.0))
            .unwrap();
        world
            .entities_mut()
            .components_mut(handle)
            .unwrap()
            .insert(&schemas, &torse_inventory::schema(), inventory.fields())
            .unwrap();
    }
    // A deleted identity remains consumed by every subsequent checkpoint and recovery.
    let deleted = world
        .entities_mut()
        .spawn(Position::new(4.0, 2.0, 2.0))
        .unwrap();
    world.entities_mut().despawn(deleted).unwrap();
    world
}

fn binding(directory: &WorldDirectory) -> Binding {
    Binding {
        world: WORLD,
        epoch: directory.epoch(),
        compatibility: support::compatibility(),
    }
}

fn create(directory: &Directory, config: FileServerConfig) -> Files {
    Files::create_file(
        directory.acquire(),
        support::compatibility(),
        Codec::new(config.journal.codec_limits).unwrap(),
        config,
        |binding| WorldHost::new(binding, world()),
    )
    .unwrap()
}

fn open(directory: &Directory) -> Result<Files, Fault> {
    let fresh = world();
    let config = policy();
    Files::open_file(
        directory.acquire(),
        support::compatibility(),
        Codec::new(config.journal.codec_limits).unwrap(),
        config,
        |binding| {
            Ok(WorldHostRecovery::new(
                binding,
                fresh.registry(),
                fresh.schemas(),
            ))
        },
    )
}

fn transaction(id: u128) -> Transaction {
    Transaction::new(
        TransactionId(id),
        torse_inventory::owner(),
        OrderingKey {
            phase: Phase::Interaction,
            rule_priority: 0,
            stable_task_key: id as u64,
            local_sequence: 0,
        },
    )
}

fn block(world: &WorldAuthority, id: u128, x: i64) -> Transaction {
    let mut tx = transaction(id);
    tx.read_block(BlockPos::new(x, 1, 1), BlockStateId::EMPTY)
        .write_block(
            BlockPos::new(x, 1, 1),
            world.registry().default_state(&key("base:stone")).unwrap(),
        );
    tx
}

fn mixed(world: &WorldAuthority, destination: EntityId) -> Transaction {
    let mut tx = block(world, 1, 2);
    let source = world.entities().handle_of(EntityId(1)).unwrap();
    let entity = world.entities().get(source).unwrap();
    tx.move_entity(
        source,
        entity.id,
        entity.position,
        [-1.0, 0.0, 0.0],
        [0.3, 0.9, 0.3],
    );
    let schema = torse_inventory::schema();
    let empty = Inventory::new(1).unwrap().fields();
    let held = entity.components.component(&schema).unwrap().clone();
    for (id, fields) in [(EntityId(1), empty), (destination, held)] {
        let handle = world.entities().handle_of(id).unwrap();
        tx.components.push(ComponentEdit {
            id,
            schema: schema.clone(),
            previous: Some(
                world
                    .entities()
                    .get(handle)
                    .unwrap()
                    .components
                    .component(&schema)
                    .unwrap()
                    .clone(),
            ),
            fields: Some(fields),
        });
    }
    tx
}

fn capture(world: &WorldAuthority) -> Snapshot {
    world
        .capture_snapshot(SnapshotMeta {
            cursor: CommitCursor {
                world: WORLD,
                seq: world.seq(),
            },
            compatibility: support::compatibility(),
            resume_at: world.coordinator().resume_point(),
        })
        .unwrap()
}

fn lifecycle_plan(world: &WorldAuthority, id: u128) -> Transaction {
    let mut tx = block(world, id, 5);
    let old = world.entities().handle_of(EntityId(1)).unwrap();
    let entity = world.entities().get(old).unwrap();
    tx.despawn_entity(old, entity).spawn_entity(
        Position::new(-0.5, 2.0, 2.0),
        [(
            torse_inventory::schema(),
            entity
                .components
                .component(&torse_inventory::schema())
                .unwrap()
                .clone(),
        )]
        .into_iter()
        .collect(),
    );
    let survivor = world.entities().handle_of(EntityId(2)).unwrap();
    let entity = world.entities().get(survivor).unwrap();
    tx.move_entity(
        survivor,
        entity.id,
        entity.position,
        [0.5, 0.0, 0.0],
        [0.3, 0.9, 0.3],
    );
    tx
}

#[test]
fn lifecycle_preparation_drop_staging_rejection_and_restart_preserve_allocation() {
    let directory = Directory::new();
    let mut config = policy();
    config.journal.codec_budget.max_bytes = 1;
    let mut server = create(&directory, config);
    let live = server.authority().unwrap().world();
    let before = capture(live);
    let tx = lifecycle_plan(live, 1);
    let prepared = live.prepare(&tx).unwrap();
    assert_eq!(
        prepared.entity_lifecycle().unwrap().spawned[0].id,
        EntityId(5)
    );
    drop(prepared);
    assert_eq!(capture(server.authority().unwrap().world()), before);
    for _ in 0..2 {
        assert!(matches!(
            server.commit_transaction(&tx).unwrap(),
            LocalCommit::Blocked(StageError::TooLarge)
        ));
        assert_eq!(capture(server.authority().unwrap().world()), before);
    }
    drop(server);
    let mut server = open(&directory).unwrap();
    let old_handle = server
        .authority()
        .unwrap()
        .world()
        .entities()
        .handle_of(EntityId(1))
        .unwrap();
    assert!(matches!(
        server.commit_transaction(&tx).unwrap(),
        LocalCommit::Durable(_)
    ));
    let live = server.authority().unwrap().world();
    assert!(live.entities().get(old_handle).is_none());
    assert!(live.entities().handle_of(EntityId(1)).is_none());
    let created = live.entities().handle_of(EntityId(5)).unwrap();
    assert_eq!(live.entities().get(created).unwrap().position.x, -0.5);
    assert_eq!(live.entities().next_entity_id(), 6);
    let expected = capture(live);
    drop(server);
    let mut server = open(&directory).unwrap();
    assert_eq!(capture(server.authority().unwrap().world()), expected);
    server.checkpoint().unwrap();
    let live = server.authority().unwrap().world();
    let handle = live.entities().handle_of(EntityId(5)).unwrap();
    let mut remove = transaction(2);
    remove.despawn_entity(handle, live.entities().get(handle).unwrap());
    assert!(matches!(
        server.commit_transaction(&remove).unwrap(),
        LocalCommit::Durable(_)
    ));
    drop(server);
    let mut server = open(&directory).unwrap();
    assert_eq!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id(),
        6
    );
    let mut create = transaction(3);
    create.spawn_entity(Position::new(1.0, 2.0, 2.0), Default::default());
    assert!(matches!(
        server.commit_transaction(&create).unwrap(),
        LocalCommit::Durable(_)
    ));
    assert!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .handle_of(EntityId(6))
            .is_some()
    );
}

#[test]
fn lifecycle_rejections_and_stale_publication_never_partially_edit_the_world() {
    let mut world = world();
    let tx = lifecycle_plan(&world, 1);
    let before = capture(&world);
    for variant in 0..9 {
        let mut bad = tx.clone();
        let expected = match variant {
            0 => {
                bad.source = key("base:intruder");
                RejectCode::PermissionDenied
            }
            1 => {
                bad.despawns[0].previous.position_bits[0] = 1.0f64.to_bits();
                RejectCode::Conflict
            }
            2 => {
                bad.despawns[0].previous.components.clear();
                RejectCode::Conflict
            }
            3 => {
                bad.despawns.push(bad.despawns[0].clone());
                RejectCode::InvalidData
            }
            4 => {
                bad.spawns[0].position_bits[0] = f64::NAN.to_bits();
                RejectCode::InvalidData
            }
            5 => {
                bad.spawns[0].position_bits[0] = 128.0f64.to_bits();
                RejectCode::InvalidData
            }
            6 => {
                bad.spawns
                    .resize(torse_world::MAX_ENTITY_LIFECYCLE + 1, bad.spawns[0].clone());
                RejectCode::QuotaExceeded
            }
            7 => {
                bad.movements[0].id = EntityId(1);
                RejectCode::InvalidData
            }
            _ => {
                bad.despawns[0].handle.generation += 1;
                RejectCode::MissingTarget
            }
        };
        assert_eq!(
            world.prepare(&bad).unwrap_err(),
            torse_sim::PrepareError::Rejected(expected)
        );
        assert_eq!(capture(&world), before);
    }
    let prepared = world.prepare(&tx).unwrap();
    let handle = world.entities().handle_of(EntityId(1)).unwrap();
    world
        .entities_mut()
        .move_to(handle, Position::new(1.0, 2.0, 2.0))
        .unwrap();
    let changed = capture(&world);
    assert!(world.publish(prepared).is_err());
    assert_eq!(capture(&world), changed);
    let prepared = world
        .prepare(&{
            let mut create = transaction(2);
            create.spawn_entity(Position::new(1.0, 2.0, 2.0), Default::default());
            create
        })
        .unwrap();
    let unrelated = world
        .entities_mut()
        .spawn(Position::new(2.0, 2.0, 2.0))
        .unwrap();
    assert!(world.entities().get(unrelated).is_some());
    let changed = capture(&world);
    assert!(world.publish(prepared).is_err());
    assert_eq!(capture(&world), changed);
}

#[test]
fn invalid_lifecycle_tail_never_applies_valid_terrain_or_survivor_movement() {
    let mut world = world();
    let prepared = world.prepare(&lifecycle_plan(&world, 1)).unwrap();
    let original = torse_sim::changes_from_prepared(world.registry(), &prepared).unwrap();
    let snapshot = capture(&world);
    for variant in 0..6 {
        let mut bad = original.clone();
        let change = bad.entity_lifecycle.as_mut().unwrap();
        match variant {
            0 => change.next_id += 1,
            1 => change.previous_next_id -= 1,
            2 => change.spawned[0].id = EntityId(1),
            3 => change.despawned[0].position_bits[0] = 2.0f64.to_bits(),
            4 => {
                change.spawned[0]
                    .components
                    .insert(key("base:missing"), Default::default());
            }
            _ => change.spawned[0].position_bits[1] = 64.0f64.to_bits(),
        }
        let mut restored = snapshot.restore(world.registry(), world.schemas()).unwrap();
        assert!(
            bad.apply_with_components(
                &mut restored.store,
                &mut restored.entities,
                world.registry(),
                world.schemas(),
            )
            .is_err()
        );
        assert_eq!(
            Snapshot::capture(
                snapshot.meta,
                &restored.store,
                &restored.entities,
                world.registry()
            )
            .unwrap(),
            snapshot
        );
    }
    world.publish(prepared).unwrap();
    assert_eq!(world.entities().next_entity_id(), 6);
}

#[test]
fn legacy_paths_reject_lifecycle_and_committed_payload_keeps_the_complete_change() {
    let mut world = world();
    let tx = lifecycle_plan(&world, 1);
    assert!(world.coordinator().prepare(world.store(), &tx).is_err());
    assert!(
        world
            .coordinator()
            .prepare_with_entities(world.store(), world.entities(), world.registry(), &tx,)
            .is_err()
    );
    let prepared = world.prepare(&tx).unwrap();
    let changes = torse_sim::changes_from_prepared(world.registry(), &prepared).unwrap();
    let snapshot = capture(&world);
    let mut restored = snapshot.restore(world.registry(), world.schemas()).unwrap();
    assert!(
        changes
            .apply(&mut restored.store, world.registry())
            .is_err()
    );
    assert!(
        changes
            .apply_with_entities(
                &mut restored.store,
                &mut restored.entities,
                world.registry(),
            )
            .is_err()
    );
    assert_eq!(
        Snapshot::capture(
            snapshot.meta,
            &restored.store,
            &restored.entities,
            world.registry()
        )
        .unwrap(),
        snapshot
    );
    let result = world.commit(&tx).unwrap();
    let TransactionOutcome::Committed { value, .. } = result.outcome else {
        panic!("expected commit")
    };
    assert_eq!(
        torse_sim::changes_from_committed(world.registry(), &value).unwrap(),
        changes
    );
}

#[test]
fn allocator_exhaustion_rejects_creation_but_allows_removal() {
    let mut world = world();
    world
        .entities_mut()
        .restore_next_entity_id(u128::MAX - 1)
        .unwrap();
    let mut create = transaction(1);
    create.spawn_entity(Position::new(1.0, 2.0, 2.0), Default::default());
    world.commit(&create).unwrap();
    assert_eq!(world.entities().next_entity_id(), u128::MAX);
    let before = capture(&world);
    assert_eq!(
        world.prepare(&create).unwrap_err(),
        torse_sim::PrepareError::Rejected(RejectCode::QuotaExceeded)
    );
    assert_eq!(capture(&world), before);
    let handle = world.entities().handle_of(EntityId(u128::MAX - 1)).unwrap();
    let mut remove = transaction(2);
    remove.despawn_entity(handle, world.entities().get(handle).unwrap());
    world.commit(&remove).unwrap();
    assert_eq!(world.entities().next_entity_id(), u128::MAX);
    assert!(
        world
            .entities()
            .handle_of(EntityId(u128::MAX - 1))
            .is_none()
    );
    // Earlier restore APIs also permit the maximum identity itself. It can still be
    // removed even though the allocator sentinel forbids creating any further identity.
    let handle = world
        .entities_mut()
        .insert_restored(
            EntityId(u128::MAX),
            Position::new(1.0, 2.0, 2.0),
            ComponentSet::new(),
        )
        .unwrap();
    let mut remove = transaction(3);
    remove.despawn_entity(handle, world.entities().get(handle).unwrap());
    world.commit(&remove).unwrap();
    assert!(world.entities().get(handle).is_none());
}

#[test]
fn mixed_commit_checkpoint_tail_recovery_and_shutdown_preserve_the_complete_world() {
    let directory = Directory::new();
    let mut server = create(&directory, policy());
    let old_epoch = server.binding().epoch;
    let world = server.authority().unwrap().world();
    let tx = mixed(world, EntityId(2));
    let competitor = mixed(world, EntityId(3));
    assert!(matches!(
        server.commit_transaction(&tx).unwrap(),
        LocalCommit::Durable(_)
    ));
    assert!(matches!(
        server.commit_transaction(&competitor).unwrap(),
        LocalCommit::Rejected(RejectCode::Conflict)
    ));
    server.checkpoint().unwrap();
    let tx = block(server.authority().unwrap().world(), 2, 3);
    let LocalCommit::Durable(result) = server.commit_transaction(&tx).unwrap() else {
        panic!("not durable")
    };
    assert_eq!(result.publication.at.seq, CommitSeq(2));
    assert_eq!(result.durability.through, DurableSeq(2));
    let expected = capture(server.authority().unwrap().world());
    drop(server);
    OpenOptions::new()
        .append(true)
        .open(directory.0.join("journal.log"))
        .unwrap()
        .write_all(b"TOR")
        .unwrap();
    let mut server = open(&directory).unwrap();
    assert_ne!(server.binding().epoch, old_epoch);
    assert_eq!(capture(server.authority().unwrap().world()), expected);
    let world = server.authority().unwrap().world();
    let moved = world.entities().handle_of(EntityId(1)).unwrap();
    assert_eq!(world.entities().get(moved).unwrap().position.x, -0.5);
    assert_eq!(world.entities().next_entity_id(), 5);
    let tx = block(world, 3, 4);
    assert!(matches!(
        server.commit_transaction(&tx).unwrap(),
        LocalCommit::Durable(_)
    ));
    assert_eq!(
        server
            .shutdown(Tick(0), &mut NoReplicas)
            .unwrap()
            .through
            .seq,
        CommitSeq(3)
    );
    assert_eq!(
        open(&directory).unwrap().authority().unwrap().world().seq(),
        CommitSeq(3)
    );
}

#[test]
fn oversized_journal_record_preserves_world_and_borrowed_plan_for_retry() {
    let directory = Directory::new();
    let mut config = policy();
    config.journal.codec_budget.max_bytes = 1;
    let mut server = create(&directory, config);
    let before = capture(server.authority().unwrap().world());
    let tx = mixed(server.authority().unwrap().world(), EntityId(2));
    for _ in 0..2 {
        assert!(matches!(
            server.commit_transaction(&tx).unwrap(),
            LocalCommit::Blocked(StageError::TooLarge)
        ));
        assert_eq!(server.state(), ServerState::Running);
        assert_eq!(capture(server.authority().unwrap().world()), before);
        assert_eq!(server.durability().through, DurableSeq(0));
    }
    drop(server);
    let mut server = open(&directory).unwrap();
    assert!(matches!(
        server.commit_transaction(&tx).unwrap(),
        LocalCommit::Durable(_)
    ));
}

#[test]
fn domain_recovery_failure_keeps_journal_and_manifest_tails_unchanged() {
    let directory = Directory::new();
    let mut server = create(&directory, policy());
    let tx = block(server.authority().unwrap().world(), 1, 2);
    server.commit_transaction(&tx).unwrap();
    drop(server);
    let paths = [
        directory.0.join("journal.log"),
        directory.0.join("checkpoints/manifest.log"),
    ];
    for path in &paths {
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"TOR")
            .unwrap();
    }
    let before = paths.each_ref().map(|path| fs::read(path).unwrap());
    // Empty locked content cannot resolve the stone in the otherwise valid journal tail.
    let content = compile(Default::default()).unwrap();
    let registry = BlockStateRegistry::build(&content).unwrap();
    let fresh = world();
    let schemas = fresh.schemas();
    let config = policy();
    let result = Files::open_file(
        directory.acquire(),
        support::compatibility(),
        Codec::new(config.journal.codec_limits).unwrap(),
        config,
        |binding| Ok(WorldHostRecovery::new(binding, &registry, schemas)),
    );
    assert!(result.is_err());
    assert_eq!(paths.each_ref().map(|path| fs::read(path).unwrap()), before);
    assert_eq!(
        open(&directory).unwrap().authority().unwrap().world().seq(),
        CommitSeq(1)
    );
}

struct BarrierFailure(Rc<Cell<bool>>);

impl JournalBackend for BarrierFailure {
    fn append(&mut self, _: &[u8]) -> io::Result<()> {
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        if self.0.get() {
            Err(io::Error::other("injected synchronization failure"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn failed_real_world_barrier_stops_reads_and_further_commits() {
    let directory = Directory::new();
    let lease = directory.acquire();
    let binding = binding(&lease);
    let config = policy();
    let codec = Codec::new(config.journal.codec_limits).unwrap();
    let fail = Rc::new(Cell::new(false));
    let journal = Journal::with_backend(
        binding,
        CommitCursor::initial(WORLD),
        codec,
        config.journal,
        BarrierFailure(fail.clone()),
    )
    .unwrap();
    let checkpoints = Checkpoints::open(
        binding,
        codec,
        config.checkpoint,
        MemoryCheckpointBackend::new(4_194_304),
    )
    .unwrap();
    let mut server = Server::initialize(
        lease,
        WorldHost::new(binding, world()).unwrap(),
        journal,
        checkpoints,
        config.server,
    )
    .unwrap();
    fail.set(true);
    let tx = lifecycle_plan(server.authority().unwrap().world(), 1);
    assert!(server.commit_transaction(&tx).is_err());
    assert_eq!(server.state(), ServerState::Faulted);
    assert!(server.authority().is_err());
    assert!(server.commit_transaction(&tx).is_err());
    assert_eq!(server.durability().through, DurableSeq(0));
}

#[test]
fn bounded_snapshot_rejects_dense_cells_and_nested_inventory_before_capture() {
    let mut world = world();
    let tx = block(&world, 1, 2);
    world.commit(&tx).unwrap();
    let before = capture(&world);
    for budget in [
        OutputBudget::new(1, u64::MAX),
        OutputBudget::new(19, u64::MAX),
        OutputBudget::new(u32::MAX, 8192),
    ] {
        let error = world
            .capture_snapshot_with_budget(before.meta, budget)
            .unwrap_err();
        assert_eq!(error.code, FaultCode::ResourceLimit);
        assert_eq!(capture(&world), before);
    }
    assert_eq!(
        world
            .capture_snapshot_with_budget(before.meta, OutputBudget::new(20, 1_048_576))
            .unwrap(),
        before
    );
}

struct ExitBeforeAccept(FileJournal<LocalWorldModel, Codec>);

impl CommitJournal<LocalWorldModel> for ExitBeforeAccept {
    type Staged = Staged;

    fn try_stage(&mut self, record: &ModelCommit<LocalWorldModel>) -> Result<Staged, StageError> {
        self.0.try_stage(record)
    }

    fn accept(&mut self, _: Staged) -> Result<(), Fault> {
        std::process::exit(73)
    }

    fn flush_through(&mut self, target: CommitCursor) -> Result<Durability, Fault> {
        self.0.flush_through(target)
    }

    fn durability(&self) -> Durability {
        self.0.durability()
    }
}

#[test]
fn crash_child() {
    let Some(root) = std::env::var_os("TORSE_WORLD_CRASH_ROOT") else {
        return;
    };
    let lease = WorldDirectory::acquire(&root, WORLD, 8192).unwrap();
    let config = policy();
    let codec = Codec::new(config.journal.codec_limits).unwrap();
    let mode = std::env::var("TORSE_WORLD_CRASH_MODE").unwrap();
    let lifecycle = mode.ends_with("lifecycle");
    let phase = mode.ends_with("phase");
    if mode.starts_with("durable") {
        let mut server =
            Files::create_file(lease, support::compatibility(), codec, config, |binding| {
                WorldHost::new(binding, world())
            })
            .unwrap();
        if phase {
            assert!(matches!(
                server.advance_phase().unwrap(),
                LocalCommit::Durable(_)
            ));
            std::process::exit(74);
        }
        let tx = if lifecycle {
            lifecycle_plan(server.authority().unwrap().world(), 1)
        } else {
            mixed(server.authority().unwrap().world(), EntityId(2))
        };
        assert!(matches!(
            server.commit_transaction(&tx).unwrap(),
            LocalCommit::Durable(_)
        ));
        std::process::exit(74);
    }
    let binding = binding(&lease);
    let path = lease.root().join("checkpoints");
    fs::create_dir(&path).unwrap();
    let checkpoints = Checkpoints::open(
        binding,
        codec,
        config.checkpoint,
        FileCheckpointBackend::open(path).unwrap(),
    )
    .unwrap();
    let journal = FileJournal::create(
        lease.root().join("journal.log"),
        binding,
        CommitCursor::initial(WORLD),
        codec,
        config.journal,
    )
    .unwrap();
    let mut server = Server::initialize(
        lease,
        WorldHost::new(binding, world()).unwrap(),
        ExitBeforeAccept(journal),
        checkpoints,
        config.server,
    )
    .unwrap();
    if phase {
        let _ = server.advance_phase();
        panic!("expected exit between phase publication and journal acceptance");
    }
    let tx = if lifecycle {
        lifecycle_plan(server.authority().unwrap().world(), 1)
    } else {
        mixed(server.authority().unwrap().world(), EntityId(2))
    };
    let _ = server.commit_transaction(&tx);
    panic!("expected exit between publication and journal acceptance");
}

#[test]
fn process_exit_recovers_only_the_durable_world_prefix() {
    for (mode, exit, sequence) in [
        ("before_accept", 73, 0),
        ("durable", 74, 1),
        ("before_accept_lifecycle", 73, 0),
        ("durable_lifecycle", 74, 1),
        ("before_accept_phase", 73, 0),
        ("durable_phase", 74, 1),
    ] {
        let directory = Directory::new();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_child", "--nocapture"])
            .env("TORSE_WORLD_CRASH_ROOT", &directory.0)
            .env("TORSE_WORLD_CRASH_MODE", mode)
            .output()
            .unwrap();
        assert_eq!(
            child.status.code(),
            Some(exit),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        let server = open(&directory).unwrap();
        let mut expected = world();
        if sequence == 1 {
            if mode.ends_with("phase") {
                let prepared = expected.prepare_phase_advance().unwrap();
                expected.publish(prepared).unwrap();
            } else {
                let tx = if mode.ends_with("lifecycle") {
                    lifecycle_plan(&expected, 1)
                } else {
                    mixed(&expected, EntityId(2))
                };
                expected.commit(&tx).unwrap();
            }
        }
        assert_eq!(
            capture(server.authority().unwrap().world()),
            capture(&expected)
        );
        assert_eq!(server.durability().through, DurableSeq(sequence));
    }
}

#[test]
fn local_recovery_rejects_metadata_progress_and_compatibility_mismatches() {
    let world = world();
    let snapshot = capture(&world);
    let binding = Binding {
        world: WORLD,
        epoch: WorldEpoch(2),
        compatibility: support::compatibility(),
    };
    for variant in 0..4 {
        let mut state = snapshot.clone();
        match variant {
            0 => state.meta.resume_at.next_phase = 3,
            1 => state.meta.resume_at.next_step = 1,
            2 => state.meta.compatibility.authority_schema += 1,
            _ => state.meta.cursor.world = WorldId(99),
        }
        let mut recovery = WorldHostRecovery::new(binding, world.registry(), world.schemas());
        assert!(
            recovery
                .restore_snapshot(WorldSnapshot {
                    meta: state.meta,
                    state
                })
                .is_err()
        );
    }
    let mut recovery = WorldHostRecovery::new(binding, world.registry(), world.schemas());
    recovery
        .restore_snapshot(WorldSnapshot {
            meta: snapshot.meta,
            state: snapshot,
        })
        .unwrap();
    assert!(
        recovery
            .finish(CommitCursor {
                world: WORLD,
                seq: CommitSeq(1)
            })
            .is_err()
    );

    let snapshot = capture(&world);
    let mut recovery = WorldHostRecovery::new(binding, world.registry(), world.schemas());
    recovery
        .restore_snapshot(WorldSnapshot {
            meta: snapshot.meta,
            state: snapshot,
        })
        .unwrap();
    let prepared = world.prepare(&block(&world, 1, 2)).unwrap();
    let original = torse_sim::changes_from_prepared(world.registry(), &prepared).unwrap();
    for variant in 0..4 {
        let mut changes = original.clone();
        match variant {
            0 => changes.meta.resume_at.tick = Tick(1),
            1 => changes.meta.compatibility.authority_schema += 1,
            2 => changes.meta.resume_at.next_phase = 1,
            _ => changes.meta.previous = CommitSeq(1),
        }
        assert!(
            recovery
                .replay(CommitRecord {
                    meta: changes.meta,
                    changes
                })
                .is_err()
        );
    }
    assert_eq!(
        recovery
            .finish(CommitCursor::initial(WORLD))
            .unwrap()
            .world()
            .seq(),
        CommitSeq(0)
    );
}

#[test]
fn durable_phase_progress_recovers_at_every_phase_and_checkpoints_only_at_tick_start() {
    let directory = Directory::new();
    let mut server = create(&directory, policy());
    for phase in 0..6 {
        let live = server.authority().unwrap().world();
        assert_eq!(live.coordinator().resume_point().next_phase, phase);
        let tx = block(live, u128::from(phase) + 1, i64::from(phase));
        assert!(matches!(
            server.commit_transaction(&tx).unwrap(),
            LocalCommit::Durable(_)
        ));
        assert!(matches!(
            server.advance_phase().unwrap(),
            LocalCommit::Durable(_)
        ));
        let expected = capture(server.authority().unwrap().world());
        assert_eq!(
            server.durability().through,
            DurableSeq(u64::from(phase + 1) * 2)
        );
        if phase < 5 {
            assert_eq!(
                server.checkpoint().unwrap_err().code,
                FaultCode::InvalidState
            );
        } else {
            server.checkpoint().unwrap();
        }
        drop(server);
        server = open(&directory).unwrap();
        assert_eq!(capture(server.authority().unwrap().world()), expected);
    }
    assert_eq!(
        server.authority().unwrap().resume_point(),
        ResumePoint {
            tick: Tick(1),
            next_phase: 0,
            next_step: 0,
        }
    );
}

#[test]
fn phase_staging_rejection_preserves_progress_and_shutdown_persists_the_next_tick() {
    let directory = Directory::new();
    let mut config = policy();
    config.journal.codec_budget.max_bytes = 1;
    let mut server = create(&directory, config);
    let before = capture(server.authority().unwrap().world());
    for _ in 0..2 {
        assert!(matches!(
            server.advance_phase().unwrap(),
            LocalCommit::Blocked(StageError::TooLarge)
        ));
        assert_eq!(capture(server.authority().unwrap().world()), before);
        assert_eq!(server.durability().through, DurableSeq(0));
    }
    drop(server);
    let mut server = open(&directory).unwrap();
    assert!(matches!(
        server.advance_phase().unwrap(),
        LocalCommit::Durable(_)
    ));
    let report = server.shutdown(Tick(0), &mut NoReplicas).unwrap();
    assert_eq!(report.committed_during_shutdown, 5);
    assert_eq!(report.through.seq, CommitSeq(6));
    let server = open(&directory).unwrap();
    let live = server.authority().unwrap().world();
    assert_eq!(
        live.coordinator().resume_point(),
        ResumePoint {
            tick: Tick(1),
            next_phase: 0,
            next_step: 0,
        }
    );
    let mut expected = before;
    expected.meta.cursor.seq = CommitSeq(6);
    expected.meta.resume_at = live.coordinator().resume_point();
    assert_eq!(capture(live), expected);
}

#[test]
fn bounded_shutdown_failure_leaves_a_recoverable_phase_prefix() {
    let directory = Directory::new();
    let mut config = policy();
    config.server.shutdown_max_steps = 2;
    let mut server = create(&directory, config);
    server.advance_phase().unwrap();
    assert_eq!(
        server.shutdown(Tick(0), &mut NoReplicas).unwrap_err().code,
        FaultCode::ResourceLimit
    );
    let server = open(&directory).unwrap();
    assert_eq!(server.authority().unwrap().resume_point().next_phase, 3);
    assert_eq!(server.durability().through, DurableSeq(3));
    server.shutdown(Tick(0), &mut NoReplicas).unwrap();
    let server = open(&directory).unwrap();
    assert_eq!(server.authority().unwrap().resume_point().tick, Tick(1));
}

#[test]
fn recovery_rejects_progress_skips_rewinds_steps_and_mixed_writes() {
    let mut world = world();
    let snapshot = capture(&world);
    let binding = Binding {
        world: WORLD,
        epoch: WorldEpoch(2),
        compatibility: support::compatibility(),
    };
    let mut recovery = WorldHostRecovery::new(binding, world.registry(), world.schemas());
    recovery
        .restore_snapshot(WorldSnapshot {
            meta: snapshot.meta,
            state: snapshot,
        })
        .unwrap();
    let prepared = world.prepare_phase_advance().unwrap();
    let original = torse_sim::changes_from_prepared(world.registry(), &prepared).unwrap();
    for variant in 0..6 {
        let mut changes = original.clone();
        match variant {
            0 => changes.meta.resume_at.next_phase = 2,
            1 => changes.meta.resume_at.tick = Tick(1),
            2 => changes.meta.resume_at.next_step = 1,
            3 => changes.meta.resume_at.next_phase = 6,
            4 => changes.meta.previous = CommitSeq(1),
            _ => {
                changes.edits = torse_sim::changes_from_prepared(
                    world.registry(),
                    &world.prepare(&block(&world, 1, 2)).unwrap(),
                )
                .unwrap()
                .edits
            }
        }
        assert!(
            recovery
                .replay(CommitRecord {
                    meta: changes.meta,
                    changes
                })
                .is_err()
        );
    }
    recovery
        .replay(CommitRecord {
            meta: original.meta,
            changes: original,
        })
        .unwrap();
    // Rewind with a consecutive sequence must still be rejected.
    let mut rewind = torse_sim::changes_from_prepared(world.registry(), &prepared).unwrap();
    rewind.meta.previous = CommitSeq(1);
    rewind.meta.cursor.seq = CommitSeq(2);
    rewind.meta.resume_at.next_phase = 0;
    assert!(
        recovery
            .replay(CommitRecord {
                meta: rewind.meta,
                changes: rewind
            })
            .is_err()
    );
    let restored = recovery
        .finish(CommitCursor {
            world: WORLD,
            seq: CommitSeq(1),
        })
        .unwrap();
    world.publish(prepared).unwrap();
    assert_eq!(capture(restored.world()), capture(&world));
}
