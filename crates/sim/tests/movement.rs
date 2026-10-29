//! A4 acceptance and fault-boundary tests against the assembled authority.

use torse_content::{compile, m0_fixture_pack};
use torse_sim::{
    CommitFault, OrderingKey, Phase, Simulation, SimulationLimits, System, SystemContext,
    SystemSpec, Transaction, WorldAuthority, changes_from_committed,
};
use torse_types::{
    BlockPos, CommitCursor, Compatibility, ContentKey, Digest, EntityHandle, EntityId, RejectCode,
    SectionPos, SnapshotMeta, TransactionId, TransactionOutcome, TransactionResult, WorldId,
};
use torse_world::{
    BlockStateId, BlockStateRegistry, ComponentSchemas, EntityMove, Position, Section,
    TerrainQuery, WorldBounds,
};

fn key(value: &str) -> ContentKey {
    ContentKey::parse(value).expect("fixture key")
}

fn authority() -> WorldAuthority {
    let content = compile(m0_fixture_pack()).expect("fixture content");
    let registry = BlockStateRegistry::build(&content).expect("registry");
    let schemas = ComponentSchemas::build(&content);
    let mut authority = WorldAuthority::new(
        WorldId(17),
        registry,
        schemas,
        16,
        WorldBounds::new(0, 32).expect("bounds"),
        Compatibility {
            authority_schema: 1,
            rules_digest: Digest([1; 32]),
            content_digest: Digest([2; 32]),
        },
    )
    .expect("authority");
    for x in [-1, 0] {
        let pos = SectionPos::new(x, 0, 0);
        authority.store_mut().request(pos).expect("request");
        authority
            .store_mut()
            .install(pos, Section::empty(16).expect("section"))
            .expect("install");
        authority.store_mut().finish_load(pos).expect("finish");
    }
    authority
}

fn transaction(id: u128) -> Transaction {
    Transaction::new(
        TransactionId(id),
        key("base:motion"),
        OrderingKey {
            phase: Phase::Simulation,
            rule_priority: 0,
            stable_task_key: id as u64,
            local_sequence: 0,
        },
    )
}

fn movement(authority: &WorldAuthority, handle: EntityHandle, delta: [f64; 3]) -> Transaction {
    let entity = authority.entities().get(handle).expect("live entity");
    let mut tx = transaction(1);
    tx.move_entity(handle, entity.id, entity.position, delta, [0.3, 0.9, 0.3]);
    tx
}

fn spawn(authority: &mut WorldAuthority, x: f64) -> EntityHandle {
    authority
        .entities_mut()
        .spawn(Position::new(x, 2.0, 2.0))
        .expect("spawn")
}

#[test]
fn movement_and_prediction_above_build_ceiling_land_on_the_highest_layer() {
    let mut world = authority();
    let upper = SectionPos::new(0, 1, 0);
    world.store_mut().request(upper).unwrap();
    world
        .store_mut()
        .install(upper, Section::empty(16).unwrap())
        .unwrap();
    world.store_mut().finish_load(upper).unwrap();
    let stone = world.registry().default_state(&key("base:stone")).unwrap();
    world
        .store_mut()
        .edit(BlockPos::new(2, 31, 2), stone)
        .unwrap();
    let start = Position::new(2.5, 35.0, 2.5);
    assert!(
        torse_sim::movement::observe_body(
            world.store(),
            world.registry(),
            start,
            torse_sim::player::WALKER_HALF
        )
        .unwrap()
        .is_empty()
    );
    let handle = world.entities_mut().spawn(start).unwrap();
    let predicted = torse_sim::player::predict_step(
        &world,
        start,
        torse_sim::player::WALKER_HALF,
        torse_world::PlayerMotion {
            vertical_micros: 0,
            grounded: false,
        },
        torse_sim::player::MoveInput {
            seq: 1,
            wish: [0; 3],
            jump: false,
        },
    )
    .unwrap()
    .0;
    let mut walker = torse_sim::player::WalkerState::new(handle);
    let first = torse_sim::player::walker_step(
        &mut world,
        &mut walker,
        [0.0; 3],
        false,
        0.05,
        &key("test:walker"),
        TransactionId(1),
    )
    .unwrap()
    .0;
    assert!((first.y - predicted.y).abs() < 1e-7);
    assert!(first.y < start.y);
    for id in 2..80 {
        torse_sim::player::walker_step(
            &mut world,
            &mut walker,
            [0.0; 3],
            false,
            0.05,
            &key("test:walker"),
            TransactionId(id),
        )
        .unwrap();
    }
    let landed = world.entities().get(handle).unwrap().position;
    assert!((landed.y - 32.9).abs() < 1e-6);
    assert!(walker.grounded);
    assert_eq!(
        world.store().query(BlockPos::new(2, 32, 2)),
        TerrainQuery::OutOfBounds
    );
    assert!(
        world
            .store_mut()
            .edit(BlockPos::new(2, 32, 2), stone)
            .is_err()
    );
}

#[test]
fn ceiling_air_does_not_make_unknown_terrain_or_the_lower_boundary_passable() {
    let mut world = authority();
    let above_unknown = world
        .entities_mut()
        .spawn(Position::new(2.5, 32.5, 2.5))
        .unwrap();
    let tx = movement(&world, above_unknown, [0.0, -1.0, 0.0]);
    assert!(matches!(
        world.commit(&tx).unwrap().outcome,
        TransactionOutcome::Rejected(_)
    ));
    assert_eq!(
        world.entities().get(above_unknown).unwrap().position.y,
        32.5
    );
    let below = world
        .entities_mut()
        .spawn(Position::new(2.5, -1.0, 2.5))
        .unwrap();
    let tx = movement(&world, below, [0.0, 1.0, 0.0]);
    assert!(matches!(
        world.commit(&tx).unwrap().outcome,
        TransactionOutcome::Rejected(_)
    ));
    assert_eq!(world.entities().get(below).unwrap().position.y, -1.0);
}

fn committed(result: TransactionResult<torse_sim::Committed>) -> torse_sim::Committed {
    match result.outcome {
        TransactionOutcome::Committed { value, .. } => value,
        TransactionOutcome::Rejected(code) => panic!("expected commit, got {code:?}"),
    }
}

fn rejected(authority: &mut WorldAuthority, tx: &Transaction, expected: RejectCode) {
    let seq = authority.seq();
    assert_eq!(
        authority.commit(tx).expect("no fault").outcome,
        TransactionOutcome::Rejected(expected)
    );
    assert_eq!(
        authority.seq(),
        seq,
        "a rejection must not consume a sequence"
    );
}

fn snapshot_meta(authority: &WorldAuthority) -> SnapshotMeta {
    SnapshotMeta {
        cursor: CommitCursor {
            world: authority.world(),
            seq: authority.seq(),
        },
        compatibility: authority.coordinator().compatibility(),
        resume_at: authority.coordinator().resume_point(),
    }
}

fn group(tx: Transaction) -> torse_sim::TransactionGroup {
    torse_sim::TransactionGroup::new(tx.id, tx.ordering, vec![tx]).unwrap()
}

#[test]
fn movement_token_refreshes_after_independent_commit_without_changing_serial_output() {
    let mut authority = authority();
    let first = spawn(&mut authority, 2.0);
    let second = spawn(&mut authority, 6.0);
    let tx = group(movement(&authority, second, [1.0, 0.0, 0.0]));
    let token = authority.prevalidate_movement(&tx).unwrap();
    assert!(token.allocated_bytes() > 0);
    committed(
        authority
            .commit(&movement(&authority, first, [0.5, 0.0, 0.0]))
            .unwrap(),
    );
    let seq = authority.seq();
    let fresh = authority.prepare_group(&tx).unwrap();
    let cached = authority.refresh_movement(&token).unwrap();
    assert_eq!(fresh.meta(), cached.meta());
    assert_eq!(fresh.entity_moves(), cached.entity_moves());
    assert_eq!(cached.meta().previous, seq);
    assert_eq!(authority.seq(), seq);
    authority.publish(cached).unwrap();
    assert!(
        authority.refresh_movement(&token).is_err(),
        "old position was consumed"
    );
}

#[test]
fn movement_token_rejects_changed_terrain_identity_position_phase_and_compatibility() {
    for change in 0..6 {
        let mut authority = authority();
        let handle = spawn(&mut authority, 2.0);
        let tx = group(movement(&authority, handle, [1.0, 0.0, 0.0]));
        let token = authority.prevalidate_movement(&tx).unwrap();
        match change {
            0 => {
                let stone = authority
                    .registry()
                    .default_state(&key("base:stone"))
                    .unwrap();
                authority
                    .store_mut()
                    .edit(BlockPos::new(2, 2, 2), stone)
                    .unwrap();
            }
            1 => {
                authority.entities_mut().despawn(handle).unwrap();
                let replacement = spawn(&mut authority, 2.0);
                assert_eq!(replacement.slot, handle.slot);
                assert_ne!(replacement.generation, handle.generation);
            }
            2 => {
                committed(
                    authority
                        .commit(&movement(&authority, handle, [0.5, 0.0, 0.0]))
                        .unwrap(),
                );
            }
            3 => {
                let advance = authority.prepare_phase_advance().unwrap();
                authority.publish(advance).unwrap();
            }
            4 => {
                let mut compatibility = authority.coordinator().compatibility();
                compatibility.rules_digest = Digest([9; 32]);
                authority.coordinator_mut().set_compatibility(compatibility);
            }
            _ => {
                authority.store_mut().unload(SectionPos::ZERO).unwrap();
            }
        }
        let before = authority.seq();
        assert!(
            authority.refresh_movement(&token).is_err(),
            "change {change}"
        );
        assert_eq!(authority.seq(), before);
    }
}

#[test]
fn movement_tokens_exclude_explicit_reads_block_writes_and_multiple_movements() {
    let mut authority = authority();
    let handle = spawn(&mut authority, 2.0);
    let second = spawn(&mut authority, 6.0);
    for kind in 0..3 {
        let mut tx = movement(&authority, handle, [1.0, 0.0, 0.0]);
        match kind {
            0 => {
                tx.read_block(BlockPos::new(1, 1, 1), BlockStateId::EMPTY);
            }
            1 => {
                tx.write_block(BlockPos::new(1, 1, 1), BlockStateId::EMPTY);
            }
            _ => {
                tx.movements
                    .push(movement(&authority, second, [1.0, 0.0, 0.0]).movements[0]);
            }
        }
        assert!(authority.prevalidate_movement(&group(tx)).is_err());
    }
}

#[test]
fn dropped_preparation_preserves_position_sequence_and_spatial_index() {
    let mut authority = authority();
    let handle = spawn(&mut authority, 0.5);
    let before = authority
        .capture_snapshot(snapshot_meta(&authority))
        .expect("snapshot");
    let tx = movement(&authority, handle, [-1.0, 0.0, 0.0]);
    let prepared = authority.prepare(&tx).expect("prepare");
    assert_eq!(prepared.entity_moves().len(), 1);
    drop(prepared);
    assert_eq!(
        authority
            .capture_snapshot(snapshot_meta(&authority))
            .expect("snapshot"),
        before
    );
    assert_eq!(authority.entities().indexed_count(), 1);
    let prepared = authority.prepare(&tx).expect("retry prepare");
    let publication = authority.publish(prepared).expect("publish");
    assert_eq!(publication.moved, vec![EntityId(1)]);
    assert_eq!(
        authority
            .entities()
            .entities_in_section(SectionPos::ZERO)
            .count(),
        0
    );
    assert_eq!(
        authority
            .entities()
            .entities_in_section(SectionPos::new(-1, 0, 0))
            .count(),
        1
    );
    assert_eq!(authority.entities().handle_of(EntityId(1)), Some(handle));
}

#[test]
fn stale_position_conflicts_and_reused_slot_cannot_move_a_replacement() {
    let mut authority = authority();
    let handle = spawn(&mut authority, 2.0);
    let tx = movement(&authority, handle, [1.0, 0.0, 0.0]);
    committed(authority.commit(&tx).expect("commit"));
    rejected(&mut authority, &tx, RejectCode::Conflict);
    let id = authority.entities().get(handle).expect("entity").id;
    authority.entities_mut().despawn(handle).expect("despawn");
    let replacement = spawn(&mut authority, 2.0);
    assert_eq!(handle.slot, replacement.slot);
    assert_ne!(handle.generation, replacement.generation);
    assert_eq!(authority.entities().handle_of(id), None);
    rejected(&mut authority, &tx, RejectCode::MissingTarget);
}

#[test]
fn collision_uses_solid_profiles_and_stops_at_walls_and_unloaded_sections() {
    let mut authority = authority();
    let handle = spawn(&mut authority, 2.0);
    let stone = authority
        .registry()
        .default_state(&key("base:stone"))
        .expect("stone");
    for y in 1..=2 {
        authority
            .store_mut()
            .edit(BlockPos::new(4, y, 2), stone)
            .expect("wall");
    }
    let tx = movement(&authority, handle, [12.0, 0.0, 0.0]);
    let record = committed(authority.commit(&tx).expect("wall move"));
    let position = authority.entities().get(handle).expect("entity").position;
    assert!((position.x - 3.7).abs() < 0.001, "wall stops the collider");
    assert_eq!(record.entity_moves[0].position_bits, position.to_bits());
    // A fresh entity can approach the absent section but never enter it.
    let other = spawn(&mut authority, 14.0);
    let tx = movement(&authority, other, [4.0, 0.0, 0.0]);
    committed(authority.commit(&tx).expect("boundary move"));
    assert!((authority.entities().get(other).expect("entity").position.x - 15.7).abs() < 0.001);
}

#[test]
fn mixed_edits_use_post_transaction_collision_and_reject_as_a_group() {
    let mut authority = authority();
    let handle = spawn(&mut authority, 2.0);
    let stone = authority
        .registry()
        .default_state(&key("base:stone"))
        .expect("stone");
    let target = BlockPos::new(4, 2, 2);
    authority.store_mut().edit(target, stone).expect("wall");
    let mut tx = movement(&authority, handle, [4.0, 0.0, 0.0]);
    tx.write_block(target, BlockStateId::EMPTY);
    let record = committed(authority.commit(&tx).expect("mixed commit"));
    assert_eq!(record.edits.len(), 1);
    assert_eq!(
        authority.entities().get(handle).expect("entity").position.x,
        6.0
    );

    // Placing a wall over the starting collider fails before either write.
    let before = authority
        .capture_snapshot(snapshot_meta(&authority))
        .expect("snapshot");
    let mut tx = movement(&authority, handle, [1.0, 0.0, 0.0]);
    tx.write_block(BlockPos::new(6, 2, 2), stone);
    rejected(&mut authority, &tx, RejectCode::Conflict);
    assert_eq!(
        authority
            .capture_snapshot(snapshot_meta(&authority))
            .expect("snapshot"),
        before
    );
}

#[test]
fn publication_rechecks_collision_reads_and_all_entity_targets_before_writes() {
    for terrain_changes in [false, true] {
        let mut authority = authority();
        let first = spawn(&mut authority, 2.0);
        let second = spawn(&mut authority, 8.0);
        let mut tx = movement(&authority, first, [1.0, 0.0, 0.0]);
        let entity = authority.entities().get(second).expect("second");
        tx.move_entity(
            second,
            entity.id,
            entity.position,
            [1.0, 0.0, 0.0],
            [0.3; 3],
        );
        let stone = authority
            .registry()
            .default_state(&key("base:stone"))
            .expect("stone");
        tx.write_block(BlockPos::new(12, 2, 2), stone);
        let prepared = authority.prepare(&tx).expect("prepare");
        if terrain_changes {
            authority
                .store_mut()
                .edit(BlockPos::new(3, 2, 2), stone)
                .expect("external edit");
        } else {
            authority
                .entities_mut()
                .despawn(second)
                .expect("external despawn");
        }
        let before = authority
            .capture_snapshot(snapshot_meta(&authority))
            .expect("snapshot");
        assert!(matches!(
            authority.publish(prepared),
            Err(CommitFault::Invariant(_))
        ));
        assert_eq!(
            authority
                .capture_snapshot(snapshot_meta(&authority))
                .expect("snapshot"),
            before
        );
    }
}

#[test]
fn invalid_numeric_inputs_duplicates_and_quotas_leave_the_authority_unchanged() {
    let mut authority = authority();
    let handle = spawn(&mut authority, 2.0);
    for value in [f64::NAN, f64::INFINITY, -f64::INFINITY, 16.1] {
        let tx = movement(&authority, handle, [value, 0.0, 0.0]);
        rejected(&mut authority, &tx, RejectCode::InvalidData);
    }
    for half in [0.0, -1.0, 0.1, 4.1, f64::NAN] {
        let mut tx = movement(&authority, handle, [1.0, 0.0, 0.0]);
        tx.movements[0].half_bits = [half.to_bits(); 3];
        rejected(&mut authority, &tx, RejectCode::InvalidData);
    }
    let mut tx = movement(&authority, handle, [1.0, 0.0, 0.0]);
    tx.movements.push(tx.movements[0]);
    rejected(&mut authority, &tx, RejectCode::InvalidData);
    tx.movements = vec![tx.movements[0]; torse_sim::movement::MAX_TRANSACTION_MOVEMENTS + 1];
    rejected(&mut authority, &tx, RejectCode::QuotaExceeded);
    assert_eq!(
        authority.entities().get(handle).expect("entity").position.x,
        2.0
    );
}

#[test]
fn replay_uses_persistent_identity_and_preserves_exact_position_bits() {
    let mut authority = authority();
    let handle = spawn(&mut authority, 0.5);
    let snapshot = authority
        .capture_snapshot(snapshot_meta(&authority))
        .expect("snapshot");
    let mut records = Vec::new();
    for delta in [[-1.0, 0.0, 0.0], [-0.125, 0.25, 0.0]] {
        let tx = movement(&authority, handle, delta);
        let record = committed(authority.commit(&tx).expect("commit"));
        records.push(changes_from_committed(authority.registry(), &record).expect("exchange"));
    }
    let mut recovery = authority.begin_restore();
    recovery.restore_snapshot(&snapshot).expect("restore");
    for record in &records {
        recovery.replay(record).expect("replay");
    }
    let restored = recovery.finish().expect("finish");
    assert_eq!(
        restored
            .capture_snapshot(snapshot_meta(&restored))
            .expect("snapshot"),
        authority
            .capture_snapshot(snapshot_meta(&authority))
            .expect("snapshot")
    );
    assert_eq!(restored.entities().indexed_count(), 1);
}

#[test]
fn bad_movement_replay_cannot_partially_write_terrain_or_other_entities() {
    let mut authority = authority();
    let handle = spawn(&mut authority, 2.0);
    let tx = movement(&authority, handle, [1.0, 0.0, 0.0]);
    let committed = committed(authority.commit(&tx).expect("commit"));
    let mut changes = changes_from_committed(authority.registry(), &committed).expect("exchange");
    changes.edits.push(torse_world::ChangeEdit {
        pos: BlockPos::new(9, 2, 2),
        content: torse_world::CellContent::Block {
            block: key("base:stone"),
            values: Vec::new(),
        },
    });
    let before = authority
        .capture_snapshot(snapshot_meta(&authority))
        .expect("snapshot");
    // The source now differs from the record, so all writes must be refused.
    let (mut store, mut entities, registry, _, _) = authority.into_parts();
    assert!(
        changes
            .apply_with_entities(&mut store, &mut entities, &registry)
            .is_err()
    );
    assert_eq!(
        store.query(BlockPos::new(9, 2, 2)),
        TerrainQuery::Ready(BlockStateId::EMPTY)
    );
    assert_eq!(
        entities.get(handle).expect("entity").position.to_bits(),
        before.entities[0].position_bits
    );
    assert!(
        changes.apply(&mut store, &registry).is_err(),
        "legacy replay must not discard moves"
    );
    changes.entity_moves = vec![EntityMove {
        id: EntityId(99),
        previous_bits: [0; 3],
        position_bits: [0; 3],
    }];
    assert!(
        changes
            .apply_with_entities(&mut store, &mut entities, &registry)
            .is_err()
    );
}

struct Mover {
    spec: SystemSpec,
    handle: EntityHandle,
}

impl System for Mover {
    fn spec(&self) -> &SystemSpec {
        &self.spec
    }
    fn run(&mut self, context: &mut SystemContext<'_>) {
        let entity = context.entities().get(self.handle).expect("system entity");
        let mut tx = transaction(self.spec.rule_priority as u128 + 1);
        tx.ordering.rule_priority = self.spec.rule_priority;
        tx.move_entity(
            self.handle,
            entity.id,
            entity.position,
            [-1.0, 0.0, 0.0],
            [0.3; 3],
        );
        context.push(tx);
    }
}

#[test]
fn tick_arbitration_rejects_the_second_move_from_the_same_phase_view() {
    let mut authority = authority();
    let handle = spawn(&mut authority, 0.5);
    let mut simulation = Simulation::new(authority.world(), 50, SimulationLimits::default());
    for priority in [1, 0] {
        simulation
            .register(Box::new(Mover {
                handle,
                spec: SystemSpec {
                    id: key(&format!("base:mover{priority}")),
                    phase: Phase::Simulation,
                    rule_priority: priority,
                    before: Vec::new(),
                    after: Vec::new(),
                },
            }))
            .expect("register");
    }
    let (mut store, mut entities, registry, _, _) = authority.into_parts();
    let result = simulation
        .tick_with_entities(&mut store, &mut entities, &registry, 10)
        .expect("tick");
    assert_eq!(
        (result.committed, result.rejected, simulation.seq().0),
        (1, 1, 1)
    );
    assert_eq!(entities.get(handle).expect("entity").position.x, -0.5);
    assert_eq!(
        entities
            .entities_in_section(SectionPos::new(-1, 0, 0))
            .count(),
        1
    );
}

#[test]
fn prepared_exchange_payload_equals_the_record_after_publication() {
    let mut authority = authority();
    let handle = spawn(&mut authority, 2.0);
    let tx = movement(&authority, handle, [0.5, 0.0, 0.0]);
    let prepared = authority.prepare(&tx).expect("prepare");
    let staged = torse_sim::changes_from_prepared(authority.registry(), &prepared).expect("stage");
    drop(prepared);
    let committed = committed(authority.commit(&tx).expect("commit"));
    assert_eq!(
        staged,
        changes_from_committed(authority.registry(), &committed).expect("payload")
    );
}

#[test]
fn transparent_slab_blocks_falling_but_plant_collision_is_empty() {
    let mut authority = authority();
    let slab = authority
        .registry()
        .default_state(&key("base:glass_slab"))
        .expect("slab");
    let plant = authority
        .registry()
        .default_state(&key("base:plant"))
        .expect("plant");
    authority
        .store_mut()
        .edit(BlockPos::new(2, 1, 2), slab)
        .expect("slab cell");
    authority
        .store_mut()
        .edit(BlockPos::new(6, 2, 2), plant)
        .expect("plant cell");
    let handle = authority
        .entities_mut()
        .spawn(Position::new(2.5, 3.0, 2.5))
        .expect("spawn");
    let mut tx = movement(&authority, handle, [0.0, -2.0, 0.0]);
    tx.movements[0].half_bits = [0.3f64.to_bits(); 3];
    committed(authority.commit(&tx).expect("fall"));
    assert!((authority.entities().get(handle).expect("entity").position.y - 1.8).abs() < 0.001);
    let other = spawn(&mut authority, 5.0);
    let tx = movement(&authority, other, [2.0, 0.0, 0.0]);
    committed(authority.commit(&tx).expect("plant pass"));
    assert_eq!(
        authority.entities().get(other).expect("entity").position.x,
        7.0
    );
}

#[test]
fn newly_loaded_collision_reads_and_unknown_states_cannot_be_ignored() {
    let mut authority = authority();
    let handle = spawn(&mut authority, 14.0);
    let tx = movement(&authority, handle, [4.0, 0.0, 0.0]);
    let prepared = authority.prepare(&tx).expect("prepare at absent boundary");
    let pos = SectionPos::new(1, 0, 0);
    authority.store_mut().request(pos).expect("request");
    authority
        .store_mut()
        .install(pos, Section::empty(16).expect("section"))
        .expect("install");
    authority.store_mut().finish_load(pos).expect("finish");
    assert!(matches!(
        authority.publish(prepared),
        Err(CommitFault::Invariant(_))
    ));
    assert_eq!(authority.seq().0, 0);
    authority
        .store_mut()
        .edit(BlockPos::new(14, 2, 2), BlockStateId(u16::MAX))
        .expect("bad state");
    rejected(&mut authority, &tx, RejectCode::InvalidData);
}

#[test]
fn malformed_replay_moves_are_validated_before_any_terrain_write() {
    for variant in 0..3 {
        let mut authority = authority();
        let handle = spawn(&mut authority, 2.0);
        let tx = movement(&authority, handle, [1.0, 0.0, 0.0]);
        let prepared = authority.prepare(&tx).expect("prepare");
        let mut changes =
            torse_sim::changes_from_prepared(authority.registry(), &prepared).expect("payload");
        match variant {
            0 => changes.entity_moves.push(changes.entity_moves[0]),
            1 => changes.entity_moves[0].position_bits[0] = f64::NAN.to_bits(),
            _ => changes.entity_moves[0].id = EntityId(99),
        }
        changes.edits.push(torse_world::ChangeEdit {
            pos: BlockPos::new(9, 2, 2),
            content: torse_world::CellContent::Block {
                block: key("base:stone"),
                values: Vec::new(),
            },
        });
        let (mut store, mut entities, registry, _, _) = authority.into_parts();
        assert!(
            changes
                .apply_with_entities(&mut store, &mut entities, &registry)
                .is_err()
        );
        assert_eq!(
            store.query(BlockPos::new(9, 2, 2)),
            TerrainQuery::Ready(BlockStateId::EMPTY)
        );
        assert_eq!(entities.get(handle).expect("entity").position.x, 2.0);
        assert_eq!(entities.indexed_count(), 1);
    }
}
