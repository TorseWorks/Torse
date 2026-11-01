//! Conservation, concurrency, recovery, and generic protected-component checks.
mod actions;
#[path = "transactions/instance_persistence.rs"]
mod instance_persistence;
mod replica;
mod support;
use support::*;
use torse_inventory::{Inventory, ItemStack};
use torse_sim::{
    CommitFault, Phase, Simulation, SimulationLimits, System, SystemContext, SystemSpec,
    Transaction, changes_from_prepared,
};
use torse_types::{BlockPos, RejectCode, TransactionOutcome};
use torse_world::{BlockStateId, ComponentValue, TerrainQuery};

#[test]
fn competing_machines_cannot_spend_the_same_input() {
    let (mut world, service, catalog, ids) = setup();
    let first = plan(&service, &world, &ids, &request(ids[0], ids[1], 1));
    let second = plan(&service, &world, &ids, &request(ids[0], ids[2], 2));
    let before = world.capture_snapshot(meta(&world)).expect("snapshot");
    let prepared = world.prepare(&first).expect("prepare");
    let staged = changes_from_prepared(world.registry(), &prepared).expect("stage");
    assert_eq!(staged.component_edits.len(), 2);
    drop(prepared);
    assert_eq!(
        world.capture_snapshot(meta(&world)).expect("snapshot"),
        before
    );
    for (index, tx) in Transaction::order(vec![second, first]).iter().enumerate() {
        let result = world.commit(tx).expect("commit");
        if index == 0 {
            assert!(matches!(
                result.outcome,
                TransactionOutcome::Committed { .. }
            ));
        } else {
            assert_eq!(
                result.outcome,
                TransactionOutcome::Rejected(RejectCode::Conflict)
            );
        }
    }
    assert_eq!(world.seq().0, 1);
    assert_eq!(total(&world, &ids, &catalog), 1);
    assert_eq!(
        Inventory::from_fields(fields(&world, ids[1]), &catalog)
            .expect("inventory")
            .slots()[0]
            .as_ref()
            .expect("winner stock")
            .count,
        1
    );
}

#[test]
fn same_container_transfer_is_one_protected_replacement() {
    let (mut world, service, catalog, ids) = setup();
    let mut req = request(ids[0], ids[0], 1);
    req.parameters.insert(
        key("inventory:field/destination_slot"),
        ComponentValue::U32(1),
    );
    let tx = plan(&service, &world, &ids, &req);
    assert_eq!(tx.components.len(), 1);
    world.commit(&tx).expect("commit");
    let inventory = Inventory::from_fields(fields(&world, ids[0]), &catalog).expect("inventory");
    assert!(inventory.slots()[0].is_none());
    assert_eq!(inventory.slots()[1].as_ref().expect("target").count, 1);
    assert_eq!(total(&world, &ids, &catalog), 1);
}

#[test]
fn invalid_amount_slot_capacity_and_item_mismatch_never_publish() {
    for variant in 0..5 {
        let (mut world, service, catalog, ids) = setup();
        let mut req = request(ids[0], ids[1], 1);
        match variant {
            0 => {
                req.parameters
                    .insert(key("inventory:field/count"), ComponentValue::U32(0));
            }
            1 => {
                req.parameters
                    .insert(key("inventory:field/count"), ComponentValue::U32(2));
            }
            2 => {
                req.parameters.insert(
                    key("inventory:field/destination_slot"),
                    ComponentValue::U32(2),
                );
            }
            _ => {
                let mut inventory = Inventory::new(2).expect("inventory");
                inventory
                    .set(
                        0,
                        Some(ItemStack {
                            item: key(if variant == 3 {
                                "base:stone"
                            } else {
                                "base:glass_slab"
                            }),
                            count: 64,
                            data: Default::default(),
                        }),
                        &catalog,
                    )
                    .expect("stack");
                let handle = world.entities().handle_of(ids[1]).expect("target");
                let schemas = world.schemas().clone();
                world
                    .entities_mut()
                    .components_mut(handle)
                    .expect("components")
                    .insert(&schemas, &torse_inventory::schema(), inventory.fields())
                    .expect("install");
            }
        }
        let before = world.capture_snapshot(meta(&world)).expect("snapshot");
        assert!(
            service
                .plan(&world, &actor(&ids), Phase::Interaction, &req)
                .is_err()
        );
        assert_eq!(
            world.capture_snapshot(meta(&world)).expect("snapshot"),
            before
        );
    }
}

#[test]
fn schema_owner_cannot_be_spoofed_by_another_transaction_source() {
    let (mut world, service, _, ids) = setup();
    let mut tx = plan(&service, &world, &ids, &request(ids[0], ids[1], 1));
    tx.source = key("base:untrusted_module");
    assert_eq!(
        world.commit(&tx).expect("commit").outcome,
        TransactionOutcome::Rejected(RejectCode::PermissionDenied)
    );
    assert_eq!(world.seq().0, 0);
}

#[test]
fn external_component_change_faults_before_any_mixed_write() {
    let (mut world, service, _, ids) = setup();
    let mut tx = plan(&service, &world, &ids, &request(ids[0], ids[1], 1));
    let pos = BlockPos::new(8, 2, 2);
    tx.write_block(
        pos,
        world
            .registry()
            .default_state(&key("base:stone"))
            .expect("stone"),
    );
    let prepared = world.prepare(&tx).expect("prepare");
    let handle = world.entities().handle_of(ids[1]).expect("handle");
    world
        .entities_mut()
        .components_mut(handle)
        .expect("components")
        .remove(&torse_inventory::schema());
    assert!(matches!(
        world.publish(prepared),
        Err(CommitFault::Invariant(_))
    ));
    assert_eq!(
        world.store().query(pos),
        TerrainQuery::Ready(BlockStateId::EMPTY)
    );
    assert_eq!(world.seq().0, 0);
}

#[test]
fn snapshots_and_component_log_tails_recover_exactly_then_continue() {
    let (mut world, service, catalog, ids) = setup();
    let baseline = world.capture_snapshot(meta(&world)).expect("snapshot");
    let mut tx = plan(&service, &world, &ids, &request(ids[0], ids[1], 1));
    // Mix movement and component replacement to exercise the shared boundary.
    let handle = world.entities().handle_of(ids[0]).expect("handle");
    let entity = world.entities().get(handle).expect("entity");
    tx.move_entity(
        handle,
        entity.id,
        entity.position,
        [0.5, 0.0, 0.0],
        [0.3; 3],
    );
    let prepared = world.prepare(&tx).expect("prepare");
    let record = changes_from_prepared(world.registry(), &prepared).expect("record");
    world.publish(prepared).expect("publish");
    let mut recovery = world.begin_restore();
    recovery.restore_snapshot(&baseline).expect("restore");
    recovery.replay(&record).expect("replay");
    let mut recovered = recovery.finish().expect("finish");
    assert_eq!(
        recovered
            .capture_snapshot(meta(&recovered))
            .expect("snapshot"),
        world.capture_snapshot(meta(&world)).expect("snapshot")
    );
    let next = plan(&service, &recovered, &ids, &request(ids[1], ids[2], 2));
    recovered.commit(&next).expect("continue");
    assert_eq!(recovered.seq().0, 2);
    assert_eq!(total(&recovered, &ids, &catalog), 1);
}

#[test]
fn malformed_component_replay_is_atomic_with_terrain_and_movement() {
    for variant in 0..4 {
        let (world, service, _, ids) = setup();
        let tx = plan(&service, &world, &ids, &request(ids[0], ids[1], 1));
        let prepared = world.prepare(&tx).expect("prepare");
        let mut record = changes_from_prepared(world.registry(), &prepared).expect("record");
        match variant {
            0 => record
                .component_edits
                .push(record.component_edits[0].clone()),
            1 => record.component_edits[0].schema = key("unknown:schema"),
            2 => record.component_edits[0].previous = None,
            _ => {
                record.component_edits[0]
                    .fields
                    .as_mut()
                    .expect("fields")
                    .insert(key("inventory:field/slots"), ComponentValue::Bool(true));
            }
        }
        record.edits.push(torse_world::ChangeEdit {
            pos: BlockPos::new(8, 2, 2),
            content: torse_world::CellContent::Block {
                block: key("base:stone"),
                values: Vec::new(),
            },
        });
        let (mut store, mut entities, registry, schemas, _) = world.into_parts();
        assert!(
            record
                .apply_with_components(&mut store, &mut entities, &registry, &schemas)
                .is_err()
        );
        assert_eq!(
            store.query(BlockPos::new(8, 2, 2)),
            TerrainQuery::Ready(BlockStateId::EMPTY)
        );
        assert!(
            record
                .apply_with_entities(&mut store, &mut entities, &registry)
                .is_err()
        );
    }
}

struct Machine {
    spec: SystemSpec,
    tx: Transaction,
}
impl System for Machine {
    fn spec(&self) -> &SystemSpec {
        &self.spec
    }
    fn run(&mut self, context: &mut SystemContext<'_>) {
        context.push(self.tx.clone());
    }
}

#[test]
fn machine_tick_uses_the_same_component_conflict_boundary() {
    let (world, _, catalog, ids) = setup();
    let mut sim = Simulation::new(world.world(), 50, SimulationLimits::default());
    for n in [2, 1] {
        let mut request = request(ids[0], ids[n], n as u128);
        request.ordering.phase = Phase::Simulation;
        let tx = torse_inventory::plan_transfer(
            &actor(&ids).view(&world),
            &catalog,
            torse_inventory::Transfer {
                source: ids[0],
                source_slot: 0,
                destination: ids[n],
                destination_slot: 0,
                count: 1,
            },
            request.transaction,
            request.ordering,
        )
        .expect("native operation");
        sim.register(Box::new(Machine {
            spec: SystemSpec {
                id: key(&format!("base:machine{n}")),
                phase: Phase::Simulation,
                rule_priority: 0,
                before: Vec::new(),
                after: Vec::new(),
            },
            tx,
        }))
        .expect("register");
    }
    let (mut store, mut entities, registry, schemas, _) = world.into_parts();
    let result = sim
        .tick_with_components(&mut store, &mut entities, &registry, &schemas, 10)
        .expect("tick");
    assert_eq!((result.committed, result.rejected, sim.seq().0), (1, 1, 1));
    let total: u64 = ids
        .iter()
        .map(|id| {
            let entity = entities
                .get(entities.handle_of(*id).expect("handle"))
                .expect("entity");
            Inventory::from_fields(
                entity
                    .components
                    .component(&torse_inventory::schema())
                    .expect("fields"),
                &catalog,
            )
            .expect("inventory")
            .slots()
            .iter()
            .flatten()
            .map(|s| u64::from(s.count))
            .sum::<u64>()
        })
        .sum();
    assert_eq!(total, 1);
}

#[test]
fn harvest_task_breaks_the_block_and_moves_the_item_atomically() {
    use torse_sim::OrderingKey;
    use torse_sim::TransactionGroup;
    use torse_sim::action::ActionOutput;

    let (mut world, service, catalog, ids) = setup();
    let actor = actor(&ids);

    // Two stone blocks stand in the world module's cells; the source
    // container holds one stone. Both task instances are planned once
    // against this snapshot, like a frozen machine plan.
    let stone = world
        .registry()
        .default_state(&key("base:stone"))
        .expect("stone");
    let mut place = Transaction::new(
        torse_types::TransactionId(900),
        torse_sim::block_actions::owner(),
        OrderingKey {
            phase: Phase::Interaction,
            rule_priority: 0,
            stable_task_key: 900,
            local_sequence: 0,
        },
    );
    for x in [8i64, 9] {
        place
            .read_block(BlockPos::new(x, 2, 2), BlockStateId::EMPTY)
            .write_block(BlockPos::new(x, 2, 2), stone);
    }
    match world.commit(&place).expect("no fault").outcome {
        TransactionOutcome::Committed { .. } => {}
        outcome => panic!("placement committed: {outcome:?}"),
    }

    let break_member = |x: i64, id: u128| {
        let mut member = Transaction::new(
            torse_types::TransactionId(id),
            torse_sim::block_actions::owner(),
            OrderingKey {
                phase: Phase::Interaction,
                rule_priority: 0,
                stable_task_key: id as u64,
                local_sequence: 0,
            },
        );
        member
            .read_block(BlockPos::new(x, 2, 2), stone)
            .write_block(BlockPos::new(x, 2, 2), BlockStateId::EMPTY);
        member
    };
    let planned = service
        .plan(
            &world,
            &actor,
            Phase::Interaction,
            &request(ids[0], ids[1], 901),
        )
        .expect("transfer plans");
    let ActionOutput::Mutation(transfer_member) = planned else {
        panic!("transfer is a mutation");
    };
    let group = |x: i64, id: u128| {
        TransactionGroup::new(
            torse_types::TransactionId(id),
            OrderingKey {
                phase: Phase::Interaction,
                rule_priority: 0,
                stable_task_key: 1000,
                local_sequence: 0,
            },
            vec![break_member(x, id * 10), (*transfer_member).clone()],
        )
        .expect("group")
    };

    let before = world.seq().0;
    match world
        .commit_group(&group(8, 901))
        .expect("no fault")
        .outcome
    {
        TransactionOutcome::Committed { .. } => {}
        outcome => panic!("harvest committed: {outcome:?}"),
    }
    assert_eq!(world.seq().0, before + 1, "one sequence for the task");
    assert!(matches!(
        world.store().query(BlockPos::new(8, 2, 2)),
        TerrainQuery::Ready(state) if state.is_empty()
    ));
    let inventory_of = |id: torse_types::EntityId| {
        let handle = world.entities().handle_of(id).expect("entity");
        let fields = world
            .entities()
            .get(handle)
            .expect("entity")
            .components
            .component(&torse_inventory::schema())
            .expect("inventory");
        Inventory::from_fields(fields, &catalog).expect("inventory")
    };
    assert!(inventory_of(ids[0]).slots()[0].is_none());
    assert_eq!(
        inventory_of(ids[1]).slots()[0],
        Some(ItemStack {
            item: key("base:stone"),
            count: 1,
            data: Default::default(),
        })
    );

    // The frozen twin task still expects the consumed item; its block would
    // break, but the stale inventory member rejects the whole group.
    let stalled = world.seq().0;
    assert_eq!(
        world
            .commit_group(&group(9, 902))
            .expect("no fault")
            .outcome,
        TransactionOutcome::Rejected(RejectCode::Conflict)
    );
    assert_eq!(world.seq().0, stalled);
    assert!(matches!(
        world.store().query(BlockPos::new(9, 2, 2)),
        TerrainQuery::Ready(state) if !state.is_empty()
    ));
}
