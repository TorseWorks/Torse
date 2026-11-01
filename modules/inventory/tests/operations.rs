#[allow(dead_code)]
mod support;
use std::collections::BTreeMap;
use support::*;
use torse_inventory::{
    Inventory, InventoryOperation as Op, ItemStack, MAX_OPERATIONS, plan_inventory,
};
use torse_sim::action::ActionTarget;
use torse_sim::{Recovery, changes_from_prepared};
use torse_types::{EntityId, RejectCode, TransactionOutcome};
use torse_world::ComponentFields;

fn stack(name: &str, count: u32) -> ItemStack {
    ItemStack::new(key(name), count)
}
fn expected(
    world: &torse_sim::WorldAuthority,
    ids: &[EntityId],
) -> BTreeMap<EntityId, ComponentFields> {
    ids.iter()
        .map(|id| (*id, fields(world, *id).clone()))
        .collect()
}

#[test]
fn ingress_consumption_and_extraction_share_one_exact_container_replacement() {
    let (mut world, _, catalog, ids) = setup();
    let actor = actor(&ids)
        .permission(key("inventory:permission/insert"))
        .permission(key("inventory:permission/consume"))
        .permission(key("inventory:permission/extract"));
    let ordering = request(ids[0], ids[1], 1).ordering;
    let plan = plan_inventory(
        &actor.view(&world),
        &catalog,
        &[Op::Insert {
            target: ids[1],
            slot: 0,
            stack: stack("base:stone", 5),
        }],
        &expected(&world, &ids[1..2]),
        torse_types::TransactionId(1),
        ordering,
    )
    .unwrap();
    assert!(
        Inventory::from_fields(fields(&world, ids[1]), &catalog)
            .unwrap()
            .slots()[0]
            .is_none()
    );
    world.commit(plan.transaction()).unwrap();
    let plan = plan_inventory(
        &actor.view(&world),
        &catalog,
        &[
            Op::Consume {
                target: ids[1],
                slot: 0,
                stack: stack("base:stone", 2),
            },
            Op::Extract {
                target: ids[1],
                slot: 0,
                stack: stack("base:stone", 1),
            },
        ],
        &expected(&world, &ids[1..2]),
        torse_types::TransactionId(2),
        ordering,
    )
    .unwrap();
    assert_eq!(plan.transaction().source, torse_inventory::owner());
    assert_eq!(plan.transaction().components.len(), 1);
    assert_eq!(plan.extracted(), &[stack("base:stone", 1)]);
    world.commit(plan.transaction()).unwrap();
    assert_eq!(
        Inventory::from_fields(fields(&world, ids[1]), &catalog)
            .unwrap()
            .slots()[0],
        Some(stack("base:stone", 2))
    );
}

#[test]
fn swaps_support_different_items_and_same_container_slots() {
    let (mut world, _, catalog, ids) = setup();
    let actor = actor(&ids)
        .permission(key("inventory:permission/insert"))
        .permission(key("inventory:permission/swap"));
    let ordering = request(ids[0], ids[1], 1).ordering;
    let initialize = plan_inventory(
        &actor.view(&world),
        &catalog,
        &[Op::Insert {
            target: ids[1],
            slot: 0,
            stack: stack("base:glass_slab", 3),
        }],
        &expected(&world, &ids[1..2]),
        torse_types::TransactionId(1),
        ordering,
    )
    .unwrap();
    world.commit(initialize.transaction()).unwrap();
    let plan = plan_inventory(
        &actor.view(&world),
        &catalog,
        &[
            Op::Swap {
                left: ids[0],
                left_slot: 0,
                right: ids[1],
                right_slot: 0,
            },
            Op::Swap {
                left: ids[0],
                left_slot: 0,
                right: ids[0],
                right_slot: 1,
            },
        ],
        &expected(&world, &ids[..2]),
        torse_types::TransactionId(2),
        ordering,
    )
    .unwrap();
    assert_eq!(plan.transaction().components.len(), 2);
    world.commit(plan.transaction()).unwrap();
    let a = Inventory::from_fields(fields(&world, ids[0]), &catalog).unwrap();
    assert_eq!(a.slots(), &[None, Some(stack("base:glass_slab", 3))]);
    assert_eq!(
        Inventory::from_fields(fields(&world, ids[1]), &catalog)
            .unwrap()
            .slots()[0],
        Some(stack("base:stone", 1))
    );
}

#[test]
fn rejected_permissions_capacity_items_slots_versions_and_batches_leave_world_unchanged() {
    let (world, _, catalog, ids) = setup();
    let actor = actor(&ids)
        .permission(key("inventory:permission/insert"))
        .permission(key("inventory:permission/consume"))
        .permission(key("inventory:permission/swap"));
    let ordering = request(ids[0], ids[1], 1).ordering;
    let before = world.capture_snapshot(meta(&world)).unwrap();
    for operation in [
        Op::Insert {
            target: ids[0],
            slot: 0,
            stack: stack("base:glass_slab", 1),
        },
        Op::Insert {
            target: ids[0],
            slot: 0,
            stack: stack("base:stone", 64),
        },
        Op::Consume {
            target: ids[0],
            slot: 0,
            stack: stack("base:stone", 2),
        },
        Op::Consume {
            target: ids[0],
            slot: 0,
            stack: stack("base:stone", 0),
        },
        Op::Insert {
            target: ids[0],
            slot: 2,
            stack: stack("base:stone", 1),
        },
        Op::Insert {
            target: ids[0],
            slot: 0,
            stack: stack("other:unknown", 1),
        },
    ] {
        assert!(
            plan_inventory(
                &actor.view(&world),
                &catalog,
                &[operation],
                &expected(&world, &ids[..1]),
                torse_types::TransactionId(1),
                ordering
            )
            .is_err()
        );
    }
    let op = Op::Consume {
        target: ids[0],
        slot: 0,
        stack: stack("base:stone", 1),
    };
    assert_eq!(
        plan_inventory(
            &support::actor(&ids).view(&world),
            &catalog,
            std::slice::from_ref(&op),
            &expected(&world, &ids[..1]),
            torse_types::TransactionId(1),
            ordering
        )
        .err(),
        Some(RejectCode::PermissionDenied)
    );
    let unauthorized = torse_sim::action::ActorContext::new(key("test:actor"))
        .permission(key("inventory:permission/consume"))
        .target(ActionTarget::Entity(ids[1]));
    assert_eq!(
        plan_inventory(
            &unauthorized.view(&world),
            &catalog,
            std::slice::from_ref(&op),
            &expected(&world, &ids[..1]),
            torse_types::TransactionId(1),
            ordering
        )
        .err(),
        Some(RejectCode::PermissionDenied)
    );
    let stale = [(ids[0], Inventory::new(2).unwrap().fields())].into();
    assert_eq!(
        plan_inventory(
            &actor.view(&world),
            &catalog,
            std::slice::from_ref(&op),
            &stale,
            torse_types::TransactionId(1),
            ordering
        )
        .err(),
        Some(RejectCode::Conflict)
    );
    assert!(
        plan_inventory(
            &actor.view(&world),
            &catalog,
            &vec![op; MAX_OPERATIONS + 1],
            &expected(&world, &ids[..1]),
            torse_types::TransactionId(1),
            ordering
        )
        .is_err()
    );
    assert_eq!(world.capture_snapshot(meta(&world)).unwrap(), before);
}

#[test]
fn concurrent_consumption_conflicts_and_committed_extraction_replays_once() {
    let (mut world, _, catalog, ids) = setup();
    let actor = actor(&ids).permission(key("inventory:permission/extract"));
    let ordering = request(ids[0], ids[1], 1).ordering;
    let expected = expected(&world, &ids[..1]);
    let snapshot = world.capture_snapshot(meta(&world)).unwrap();
    let operation = [Op::Extract {
        target: ids[0],
        slot: 0,
        stack: stack("base:stone", 1),
    }];
    let first = plan_inventory(
        &actor.view(&world),
        &catalog,
        &operation,
        &expected,
        torse_types::TransactionId(1),
        ordering,
    )
    .unwrap();
    let second = plan_inventory(
        &actor.view(&world),
        &catalog,
        &operation,
        &expected,
        torse_types::TransactionId(2),
        ordering,
    )
    .unwrap();
    let prepared = world.prepare(first.transaction()).unwrap();
    let changes = changes_from_prepared(world.registry(), &prepared).unwrap();
    world.publish(prepared).unwrap();
    assert!(matches!(
        world.commit(second.transaction()).unwrap().outcome,
        TransactionOutcome::Rejected(RejectCode::Conflict)
    ));
    let mut recovery = Recovery::new(world.world(), world.registry(), world.schemas());
    recovery.restore_snapshot(&snapshot).unwrap();
    recovery.replay(&changes).unwrap();
    assert!(recovery.replay(&changes).is_err());
    let recovered = recovery.finish().unwrap();
    assert_eq!(
        recovered.capture_snapshot(meta(&recovered)).unwrap(),
        world.capture_snapshot(meta(&world)).unwrap()
    );
}
