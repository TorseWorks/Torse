//! Durable snapshots and journal records retain exact instance state.
use super::support::*;
use torse_inventory::Inventory;
use torse_sim::changes_from_prepared;
use torse_types::TransactionOutcome;
use torse_world::{ComponentFields, ComponentValue, SnapshotValue};

#[test]
fn encoded_checkpoint_and_journal_restore_opaque_instances_then_continue() {
    let (mut world, service, catalog, ids) = setup();
    install_instance(&mut world, &catalog, ids[0]);
    let baseline = codec_roundtrip(&world.capture_snapshot(meta(&world)).unwrap());
    let tx = plan(&service, &world, &ids, &request(ids[0], ids[1], 1));
    let prepared = world.prepare(&tx).unwrap();
    let record = codec_roundtrip(&changes_from_prepared(world.registry(), &prepared).unwrap());
    world.publish(prepared).unwrap();
    let checkpoint = codec_roundtrip(&world.capture_snapshot(meta(&world)).unwrap());

    for replay_tail in [false, true] {
        let mut recovery = world.begin_restore();
        recovery
            .restore_snapshot(if replay_tail { &baseline } else { &checkpoint })
            .unwrap();
        if replay_tail {
            recovery.replay(&record).unwrap();
        }
        let mut recovered = recovery.finish().unwrap();
        assert_eq!(
            recovered.capture_snapshot(meta(&recovered)).unwrap(),
            checkpoint
        );
        let transfer = plan(&service, &recovered, &ids, &request(ids[1], ids[2], 2));
        assert!(matches!(
            recovered.commit(&transfer).unwrap().outcome,
            TransactionOutcome::Committed { .. }
        ));
        let inventory = Inventory::from_fields(fields(&recovered, ids[2]), &catalog).unwrap();
        assert_eq!(inventory.slots()[0].as_ref().unwrap().data, instance_data());
        assert_eq!(total(&recovered, &ids, &catalog), 1);
    }
}

fn corrupt_slot(fields: &mut ComponentFields, variant: usize) {
    let ComponentValue::List(slots) = fields.get_mut(&key("inventory:field/slots")).unwrap() else {
        panic!("slots")
    };
    let ComponentValue::Record(slot) = &mut slots[0] else {
        panic!("slot")
    };
    match variant {
        0 => {
            slot.insert(key("inventory:field/data_format"), ComponentValue::U32(99));
        }
        1 => {
            slot.insert(key("inventory:field/count"), ComponentValue::U32(0));
        }
        2 => {
            slot.insert(
                key("inventory:field/item"),
                ComponentValue::Content(key("unknown:item")),
            );
        }
        3 => {
            slot.insert(key("inventory:field/data"), ComponentValue::Bool(false));
        }
        _ => {
            slot.insert(
                key("inventory:field/data"),
                ComponentValue::Record(
                    [(
                        key("extension:large"),
                        ComponentValue::Text(
                            "x".repeat(torse_inventory::MAX_INSTANCE_TEXT_BYTES + 1),
                        ),
                    )]
                    .into(),
                ),
            );
        }
    }
}

#[test]
fn malformed_instance_journal_is_rejected_before_sequence_or_state_changes() {
    let (mut world, service, catalog, ids) = setup();
    install_instance(&mut world, &catalog, ids[0]);
    let baseline = world.capture_snapshot(meta(&world)).unwrap();
    let tx = plan(&service, &world, &ids, &request(ids[0], ids[1], 1));
    let prepared = world.prepare(&tx).unwrap();
    let valid = changes_from_prepared(world.registry(), &prepared).unwrap();
    world.publish(prepared).unwrap();
    let expected = world.capture_snapshot(meta(&world)).unwrap();
    for variant in 0..5 {
        let mut bad = valid.clone();
        let replacement = bad
            .component_edits
            .iter_mut()
            .find(|edit| edit.id == ids[1])
            .unwrap();
        corrupt_slot(replacement.fields.as_mut().unwrap(), variant);
        bad.edits.push(torse_world::ChangeEdit {
            pos: torse_types::BlockPos::new(8, 2, 2),
            content: torse_world::CellContent::Block {
                block: key("base:stone"),
                values: Vec::new(),
            },
        });
        let bad = codec_roundtrip(&bad);
        let mut recovery = world.begin_restore();
        recovery.restore_snapshot(&baseline).unwrap();
        assert!(recovery.replay(&bad).is_err(), "variant {variant}");
        // The rejected tail has consumed neither its sequence nor its source observation.
        recovery.replay(&valid).unwrap();
        let recovered = recovery.finish().unwrap();
        let inventory = Inventory::from_fields(fields(&recovered, ids[1]), &catalog).unwrap();
        assert_eq!(inventory.slots()[0].as_ref().unwrap().data, instance_data());
        assert_eq!(recovered.seq().0, 1);
        assert_eq!(
            recovered.capture_snapshot(meta(&recovered)).unwrap(),
            expected
        );
    }
}

#[test]
fn unknown_instance_format_in_checkpoint_does_not_consume_recovery() {
    let (mut world, _, catalog, ids) = setup();
    install_instance(&mut world, &catalog, ids[0]);
    let valid = world.capture_snapshot(meta(&world)).unwrap();
    let mut bad = valid.clone();
    let component = &mut bad.entities[0].components[0];
    let SnapshotValue::List(slots) = &mut component.fields[0].1 else {
        panic!("slots")
    };
    let SnapshotValue::Record(slot) = &mut slots[0] else {
        panic!("slot")
    };
    slot.iter_mut()
        .find(|(id, _)| id == &key("inventory:field/data_format"))
        .unwrap()
        .1 = SnapshotValue::U32(99);
    let bad = codec_roundtrip(&bad);
    let mut recovery = world.begin_restore();
    assert!(recovery.restore_snapshot(&bad).is_err());
    recovery.restore_snapshot(&valid).unwrap();
    assert_eq!(
        recovery
            .finish()
            .unwrap()
            .capture_snapshot(valid.meta)
            .unwrap(),
        valid
    );
}
