#[allow(dead_code)]
mod support;
use std::collections::BTreeMap;
use support::*;
use torse_inventory::{
    Inventory, InventoryOperation as Op, ItemStack, Transfer, plan_inventory, plan_transfer,
};
use torse_types::{EntityId, RejectCode, TransactionId, TransactionOutcome};
use torse_world::{ComponentFields, ComponentValue, Position};

fn stack(count: u32, durability: u32) -> ItemStack {
    let mut stack = ItemStack::new(key("base:stone"), count);
    stack.data = [
        (key("test:durability"), ComponentValue::U32(durability)),
        (
            key("unknown:extension"),
            ComponentValue::Record(
                [(
                    key("unknown:ordered"),
                    ComponentValue::List(vec![
                        ComponentValue::Text("opaque".into()),
                        ComponentValue::I64(-2),
                    ]),
                )]
                .into(),
            ),
        ),
    ]
    .into();
    stack
}
fn expected(
    world: &torse_sim::WorldAuthority,
    ids: &[EntityId],
) -> BTreeMap<EntityId, ComponentFields> {
    ids.iter()
        .map(|id| (*id, fields(world, *id).clone()))
        .collect()
}
fn initialize(
    world: &mut torse_sim::WorldAuthority,
    catalog: &torse_inventory::ItemCatalog,
    id: EntityId,
    slots: &[Option<ItemStack>],
) {
    let mut inventory = Inventory::new(slots.len()).unwrap();
    for (index, stack) in slots.iter().enumerate() {
        inventory.set(index, stack.clone(), catalog).unwrap();
    }
    let handle = world.entities().handle_of(id).unwrap();
    let schemas = world.schemas().clone();
    world
        .entities_mut()
        .components_mut(handle)
        .unwrap()
        .insert(&schemas, &torse_inventory::schema(), inventory.fields())
        .unwrap();
}
fn inventory(
    world: &torse_sim::WorldAuthority,
    catalog: &torse_inventory::ItemCatalog,
    id: EntityId,
) -> Inventory {
    Inventory::from_fields(fields(world, id), catalog).unwrap()
}

#[test]
fn split_transfer_and_merge_preserve_every_instance_field() {
    let (mut world, _, catalog, ids) = setup();
    initialize(&mut world, &catalog, ids[0], &[Some(stack(5, 90)), None]);
    initialize(&mut world, &catalog, ids[1], &[Some(stack(2, 89)), None]);
    let grant = actor(&ids);
    let transfer = Transfer {
        source: ids[0],
        source_slot: 0,
        destination: ids[1],
        destination_slot: 0,
        count: 2,
    };
    let ordering = request(ids[0], ids[1], 1).ordering;
    assert_eq!(
        plan_transfer(
            &grant.view(&world),
            &catalog,
            transfer,
            TransactionId(1),
            ordering
        )
        .err(),
        Some(RejectCode::Conflict)
    );
    let tx = plan_transfer(
        &grant.view(&world),
        &catalog,
        Transfer {
            destination_slot: 1,
            ..transfer
        },
        TransactionId(1),
        ordering,
    )
    .unwrap();
    world.commit(&tx).unwrap();
    assert_eq!(
        inventory(&world, &catalog, ids[0]).slots()[0],
        Some(stack(3, 90))
    );
    assert_eq!(
        inventory(&world, &catalog, ids[1]).slots()[1],
        Some(stack(2, 90))
    );
    let tx = plan_transfer(
        &grant.view(&world),
        &catalog,
        Transfer {
            destination_slot: 1,
            count: 1,
            ..transfer
        },
        TransactionId(2),
        ordering,
    )
    .unwrap();
    world.commit(&tx).unwrap();
    assert_eq!(
        inventory(&world, &catalog, ids[1]).slots()[1],
        Some(stack(3, 90))
    );
}

#[test]
fn exact_consumption_transforms_and_extraction_are_atomic_and_lossless() {
    let (mut world, _, catalog, ids) = setup();
    initialize(&mut world, &catalog, ids[0], &[Some(stack(3, 90)), None]);
    let grant = actor(&ids)
        .permission(key("inventory:permission/consume"))
        .permission(key("inventory:permission/insert"))
        .permission(key("inventory:permission/extract"))
        .permission(key("inventory:permission/swap"));
    let ordering = request(ids[0], ids[1], 1).ordering;
    let before = world.capture_snapshot(meta(&world)).unwrap();
    for operation in [
        Op::Consume {
            target: ids[0],
            slot: 0,
            stack: stack(1, 89),
        },
        Op::Extract {
            target: ids[0],
            slot: 0,
            stack: stack(1, 89),
        },
    ] {
        assert_eq!(
            plan_inventory(
                &grant.view(&world),
                &catalog,
                &[operation],
                &expected(&world, &ids[..1]),
                TransactionId(1),
                ordering
            )
            .err(),
            Some(RejectCode::Conflict)
        );
    }
    assert_eq!(world.capture_snapshot(meta(&world)).unwrap(), before);
    let operations = [
        Op::Consume {
            target: ids[0],
            slot: 0,
            stack: stack(3, 90),
        },
        Op::Insert {
            target: ids[0],
            slot: 0,
            stack: stack(3, 89),
        },
        Op::Extract {
            target: ids[0],
            slot: 0,
            stack: stack(1, 89),
        },
        Op::Swap {
            left: ids[0],
            left_slot: 0,
            right: ids[0],
            right_slot: 1,
        },
    ];
    let plan = plan_inventory(
        &grant.view(&world),
        &catalog,
        &operations,
        &expected(&world, &ids[..1]),
        TransactionId(1),
        ordering,
    )
    .unwrap();
    assert_eq!(plan.extracted(), &[stack(1, 89)]);
    assert_eq!(world.capture_snapshot(meta(&world)).unwrap(), before);
    world.commit(plan.transaction()).unwrap();
    assert_eq!(
        inventory(&world, &catalog, ids[0]).slots(),
        &[None, Some(stack(2, 89))]
    );
}

#[test]
fn data_only_changes_conflict_at_planning_and_publication() {
    let (mut world, _, catalog, ids) = setup();
    initialize(&mut world, &catalog, ids[0], &[Some(stack(3, 90)), None]);
    let old = expected(&world, &ids[..1]);
    let grant = actor(&ids).permission(key("inventory:permission/consume"));
    let ordering = request(ids[0], ids[1], 1).ordering;
    let operation = [Op::Consume {
        target: ids[0],
        slot: 0,
        stack: stack(1, 90),
    }];
    let plan = plan_inventory(
        &grant.view(&world),
        &catalog,
        &operation,
        &old,
        TransactionId(1),
        ordering,
    )
    .unwrap();
    initialize(&mut world, &catalog, ids[0], &[Some(stack(3, 89)), None]);
    assert_eq!(
        plan_inventory(
            &grant.view(&world),
            &catalog,
            &operation,
            &old,
            TransactionId(2),
            ordering
        )
        .err(),
        Some(RejectCode::Conflict)
    );
    assert_eq!(
        world.commit(plan.transaction()).unwrap().outcome,
        TransactionOutcome::Rejected(RejectCode::Conflict)
    );
    assert_eq!(
        inventory(&world, &catalog, ids[0]).slots()[0],
        Some(stack(3, 89))
    );
}

#[test]
fn aggregate_plan_budget_rejects_many_valid_containers_without_changes() {
    let (mut world, _, catalog, _) = setup();
    let mut large = ItemStack::new(key("base:stone"), 1);
    large.data = (0..3)
        .map(|index| {
            (
                key(&format!("test:text/{index}")),
                ComponentValue::Text("x".repeat(1000)),
            )
        })
        .collect();
    let schemas = world.schemas().clone();
    let mut ids = Vec::new();
    for _ in 0..48 {
        let handle = world
            .entities_mut()
            .spawn(Position::new(2.0, 2.0, 2.0))
            .unwrap();
        let id = world.entities().get(handle).unwrap().id;
        let mut inventory = Inventory::new(16).unwrap();
        for slot in 0..16 {
            inventory.set(slot, Some(large.clone()), &catalog).unwrap();
        }
        world
            .entities_mut()
            .components_mut(handle)
            .unwrap()
            .insert(&schemas, &torse_inventory::schema(), inventory.fields())
            .unwrap();
        ids.push(id);
    }
    let operations: Vec<_> = ids
        .chunks_exact(2)
        .map(|pair| Op::Swap {
            left: pair[0],
            left_slot: 0,
            right: pair[1],
            right_slot: 0,
        })
        .collect();
    let before = world.capture_snapshot(meta(&world)).unwrap();
    let grant = actor(&ids).permission(key("inventory:permission/swap"));
    assert_eq!(
        plan_inventory(
            &grant.view(&world),
            &catalog,
            &operations,
            &expected(&world, &ids),
            TransactionId(1),
            request(ids[0], ids[1], 1).ordering
        )
        .err(),
        Some(RejectCode::QuotaExceeded)
    );
    assert_eq!(world.capture_snapshot(meta(&world)).unwrap(), before);
}

#[test]
fn typed_values_list_order_and_opaque_extensions_prevent_merges() {
    let (mut world, _, catalog, ids) = setup();
    let original = stack(2, 90);
    let mut variants = Vec::new();
    let mut typed = original.clone();
    typed
        .data
        .insert(key("test:durability"), ComponentValue::I64(90));
    variants.push(typed);
    let mut ordered = original.clone();
    let ComponentValue::Record(extension) =
        ordered.data.get_mut(&key("unknown:extension")).unwrap()
    else {
        panic!("record");
    };
    let ComponentValue::List(values) = extension.get_mut(&key("unknown:ordered")).unwrap() else {
        panic!("list");
    };
    values.reverse();
    variants.push(ordered);
    let mut extended = original.clone();
    extended
        .data
        .insert(key("unknown:new"), ComponentValue::Bool(false));
    variants.push(extended);
    let ordering = request(ids[0], ids[1], 1).ordering;
    let grant = actor(&ids).permission(key("inventory:permission/insert"));
    initialize(&mut world, &catalog, ids[0], &[Some(original), None]);
    let before = world.capture_snapshot(meta(&world)).unwrap();
    for variant in variants {
        assert_eq!(
            plan_inventory(
                &grant.view(&world),
                &catalog,
                &[Op::Insert {
                    target: ids[0],
                    slot: 0,
                    stack: variant
                }],
                &expected(&world, &ids[..1]),
                TransactionId(1),
                ordering
            )
            .err(),
            Some(RejectCode::Conflict)
        );
    }
    let strict = catalog
        .clone()
        .with_policy(
            &key("base:stone"),
            torse_inventory::ItemPolicy {
                mergeable: false,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        plan_inventory(
            &grant.view(&world),
            &strict,
            &[Op::Insert {
                target: ids[0],
                slot: 0,
                stack: stack(1, 90)
            }],
            &expected(&world, &ids[..1]),
            TransactionId(1),
            ordering
        )
        .err(),
        Some(RejectCode::Conflict)
    );
    assert_eq!(world.capture_snapshot(meta(&world)).unwrap(), before);
}

#[test]
fn transformations_require_both_permissions_and_late_failure_discards_drafts() {
    let (mut world, _, catalog, ids) = setup();
    initialize(&mut world, &catalog, ids[0], &[Some(stack(2, 90)), None]);
    let operations = [
        Op::Consume {
            target: ids[0],
            slot: 0,
            stack: stack(2, 90),
        },
        Op::Insert {
            target: ids[0],
            slot: 0,
            stack: stack(2, 89),
        },
        Op::Extract {
            target: ids[0],
            slot: 0,
            stack: stack(3, 89),
        },
    ];
    let ordering = request(ids[0], ids[1], 1).ordering;
    let before = world.capture_snapshot(meta(&world)).unwrap();
    for grant in [
        actor(&ids).permission(key("inventory:permission/consume")),
        actor(&ids).permission(key("inventory:permission/insert")),
    ] {
        assert_eq!(
            plan_inventory(
                &grant.view(&world),
                &catalog,
                &operations[..2],
                &expected(&world, &ids[..1]),
                TransactionId(1),
                ordering
            )
            .err(),
            Some(RejectCode::PermissionDenied)
        );
    }
    let grant = actor(&ids)
        .permission(key("inventory:permission/consume"))
        .permission(key("inventory:permission/insert"))
        .permission(key("inventory:permission/extract"));
    assert_eq!(
        plan_inventory(
            &grant.view(&world),
            &catalog,
            &operations,
            &expected(&world, &ids[..1]),
            TransactionId(1),
            ordering
        )
        .err(),
        Some(RejectCode::Conflict)
    );
    let mut invalid = stack(1, 90);
    invalid.data.insert(
        key("unknown:oversized"),
        ComponentValue::Text("x".repeat(torse_inventory::MAX_INSTANCE_TEXT_BYTES + 1)),
    );
    assert_eq!(
        plan_inventory(
            &grant.view(&world),
            &catalog,
            &[Op::Insert {
                target: ids[0],
                slot: 1,
                stack: invalid
            }],
            &expected(&world, &ids[..1]),
            TransactionId(1),
            ordering
        )
        .err(),
        Some(RejectCode::QuotaExceeded)
    );
    assert_eq!(world.capture_snapshot(meta(&world)).unwrap(), before);
}

#[test]
fn same_container_swap_near_budget_does_not_charge_transient_duplicates() {
    let (mut world, _, catalog, ids) = setup();
    let mut large = ItemStack::new(key("base:stone"), 1);
    large.data = (0..3)
        .map(|index| {
            (
                key(&format!("test:text/{index}")),
                ComponentValue::Text("x".repeat(1000)),
            )
        })
        .collect();
    let mut full = Inventory::new(64).unwrap();
    let small_slot = (0..64)
        .find(|slot| full.set(*slot, Some(large.clone()), &catalog).is_err())
        .expect("container bound precedes slot bound");
    let small = ItemStack::new(key("base:stone"), 1);
    full.set(small_slot, Some(small.clone()), &catalog).unwrap();
    assert!(full.set(small_slot, Some(large.clone()), &catalog).is_err());
    initialize(&mut world, &catalog, ids[0], full.slots());
    let grant = actor(&ids).permission(key("inventory:permission/swap"));
    let plan = plan_inventory(
        &grant.view(&world),
        &catalog,
        &[Op::Swap {
            left: ids[0],
            left_slot: small_slot,
            right: ids[0],
            right_slot: 0,
        }],
        &expected(&world, &ids[..1]),
        TransactionId(1),
        request(ids[0], ids[1], 1).ordering,
    )
    .unwrap();
    world.commit(plan.transaction()).unwrap();
    let result = inventory(&world, &catalog, ids[0]);
    assert_eq!(result.slots()[0], Some(small));
    assert_eq!(result.slots()[small_slot], Some(large));
}
