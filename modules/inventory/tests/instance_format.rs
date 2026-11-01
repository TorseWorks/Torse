use std::collections::BTreeMap;
use torse_inventory::{Inventory, ItemCatalog, ItemPolicy, ItemStack, U32Rule};
use torse_types::{ContentKey, RejectCode};
use torse_world::{ComponentFields, ComponentValue};

fn key(value: &str) -> ContentKey {
    ContentKey::parse(value).unwrap()
}
fn catalog() -> ItemCatalog {
    ItemCatalog::new([(key("sample:part"), 16)]).unwrap()
}
fn payload(value: ComponentValue) -> ComponentFields {
    [(key("sample:value"), value)].into_iter().collect()
}

#[test]
fn typed_canonical_identity_and_policy_identity() {
    let catalog = catalog();
    let mut left = ItemStack::new(key("sample:part"), 6);
    left.data = [
        (key("sample:z"), ComponentValue::U32(1)),
        (
            key("sample:a"),
            ComponentValue::List(vec![ComponentValue::U32(2), ComponentValue::U32(3)]),
        ),
    ]
    .into_iter()
    .collect();
    let mut right = left.with_count(5);
    right.data = left
        .data
        .iter()
        .rev()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert!(catalog.can_merge(&left, &right).unwrap());
    right.data.insert(key("sample:z"), ComponentValue::I64(1));
    assert!(!catalog.can_merge(&left, &right).unwrap());
    right = left.clone();
    right.data.insert(
        key("sample:a"),
        ComponentValue::List(vec![ComponentValue::U32(3), ComponentValue::U32(2)]),
    );
    assert!(!catalog.can_merge(&left, &right).unwrap());
    let digest = catalog.rules_digest();
    let strict = catalog
        .clone()
        .with_policy(
            &left.item,
            ItemPolicy {
                mergeable: false,
                ..Default::default()
            },
        )
        .unwrap();
    assert_ne!(strict.rules_digest(), digest);
    assert!(!strict.can_merge(&left, &left).unwrap());
    assert_eq!(catalog.rules_digest(), digest);
}

#[test]
fn recognized_attributes_validate_unknown_fields_survive() {
    let item = key("sample:part");
    let catalog = catalog()
        .with_policy(
            &item,
            ItemPolicy {
                revision: 2,
                mergeable: true,
                unsigned: [(
                    key("sample:damage"),
                    U32Rule {
                        minimum: 0,
                        maximum: 9,
                        required: true,
                    },
                )]
                .into_iter()
                .collect(),
            },
        )
        .unwrap();
    let mut stack = ItemStack::new(item, 1);
    assert_eq!(catalog.validate_stack(&stack), Err(RejectCode::InvalidData));
    stack
        .data
        .insert(key("sample:damage"), ComponentValue::U32(7));
    stack.data.insert(
        key("vendor:opaque"),
        ComponentValue::Record(payload(ComponentValue::Text("keep".into()))),
    );
    let mut inventory = Inventory::new(2).unwrap();
    inventory.set(0, Some(stack.clone()), &catalog).unwrap();
    assert_eq!(
        Inventory::from_fields(&inventory.fields(), &catalog).unwrap(),
        inventory
    );
    stack
        .data
        .insert(key("sample:damage"), ComponentValue::U32(10));
    assert_eq!(
        inventory.set(0, Some(stack), &catalog),
        Err(RejectCode::InvalidData)
    );
    assert_eq!(
        inventory.slots()[0].as_ref().unwrap().data[&key("sample:damage")],
        ComponentValue::U32(7)
    );
}

#[test]
fn legacy_empty_mapping_and_unknown_format_rejection() {
    let catalog = catalog();
    let mut inventory = Inventory::new(1).unwrap();
    inventory
        .set(0, Some(ItemStack::new(key("sample:part"), 1)), &catalog)
        .unwrap();
    let mut fields = inventory.fields();
    let ComponentValue::List(slots) = fields.get_mut(&key("inventory:field/slots")).unwrap() else {
        panic!()
    };
    let ComponentValue::Record(stack) = &mut slots[0] else {
        panic!()
    };
    stack.insert(key("inventory:field/data_format"), ComponentValue::U32(2));
    assert_eq!(
        Inventory::from_fields(&fields, &catalog),
        Err(RejectCode::InvalidData)
    );
    let ComponentValue::List(slots) = fields.get_mut(&key("inventory:field/slots")).unwrap() else {
        panic!()
    };
    let ComponentValue::Record(stack) = &mut slots[0] else {
        panic!()
    };
    stack.remove(&key("inventory:field/data_format"));
    stack.remove(&key("inventory:field/data"));
    assert_eq!(
        Inventory::from_fields(&fields, &catalog).unwrap(),
        inventory
    );
    assert_ne!(fields, inventory.fields());
}

#[test]
fn payload_limit_boundaries() {
    use torse_inventory::*;
    assert!(
        validate_instance_data(&payload(ComponentValue::Text(
            "a".repeat(MAX_INSTANCE_TEXT_BYTES)
        )))
        .is_ok()
    );
    assert_eq!(
        validate_instance_data(&payload(ComponentValue::Text(
            "a".repeat(MAX_INSTANCE_TEXT_BYTES + 1)
        ))),
        Err(RejectCode::QuotaExceeded)
    );
    assert!(
        validate_instance_data(&payload(ComponentValue::List(vec![
            ComponentValue::Bool(
                true
            );
            MAX_INSTANCE_COLLECTION
        ])))
        .is_ok()
    );
    assert_eq!(
        validate_instance_data(&payload(ComponentValue::List(vec![
            ComponentValue::Bool(
                true
            );
            MAX_INSTANCE_COLLECTION
                + 1
        ]))),
        Err(RejectCode::QuotaExceeded)
    );
    let long = ContentKey::parse(&format!(
        "sample:{}",
        "a".repeat(MAX_INSTANCE_KEY_BYTES - 7)
    ))
    .unwrap();
    let fields = [(long, ComponentValue::Bool(true))].into_iter().collect();
    assert!(validate_instance_data(&fields).is_ok());
    let long = ContentKey::parse(&format!(
        "sample:{}",
        "a".repeat(MAX_INSTANCE_KEY_BYTES - 6)
    ))
    .unwrap();
    assert_eq!(
        validate_instance_data(&[(long, ComponentValue::Bool(true))].into_iter().collect()),
        Err(RejectCode::QuotaExceeded)
    );
    let mut nested = ComponentValue::Bool(true);
    for _ in 0..MAX_INSTANCE_DEPTH {
        nested = ComponentValue::List(vec![nested]);
    }
    assert!(validate_instance_data(&payload(nested.clone())).is_ok());
    assert_eq!(
        validate_instance_data(&payload(ComponentValue::List(vec![nested]))),
        Err(RejectCode::QuotaExceeded)
    );
    // Four root list nodes plus 252 leaves exactly exhaust the node budget.
    let mut nodes: ComponentFields = (0..4)
        .map(|i| {
            (
                key(&format!("sample:n{i}")),
                ComponentValue::List(vec![ComponentValue::Bool(true); 63]),
            )
        })
        .collect();
    assert!(validate_instance_data(&nodes).is_ok());
    let ComponentValue::List(values) = nodes.get_mut(&key("sample:n0")).unwrap() else {
        panic!()
    };
    values.push(ComponentValue::Bool(true));
    assert_eq!(
        validate_instance_data(&nodes),
        Err(RejectCode::QuotaExceeded)
    );
    // Account for each record key and tag using the official size function.
    let mut bytes = BTreeMap::new();
    for i in 0..3 {
        bytes.insert(
            key(&format!("sample:b{i}")),
            ComponentValue::Text("a".repeat(1024)),
        );
    }
    let base = validate_instance_data(&bytes).unwrap();
    let overhead = ("sample:b3".len() + 8 + 9) as u64;
    bytes.insert(
        key("sample:b3"),
        ComponentValue::Text("a".repeat((MAX_INSTANCE_BYTES - base - overhead) as usize)),
    );
    assert_eq!(validate_instance_data(&bytes), Ok(MAX_INSTANCE_BYTES));
    let ComponentValue::Text(text) = bytes.get_mut(&key("sample:b3")).unwrap() else {
        panic!()
    };
    text.push('a');
    assert_eq!(
        validate_instance_data(&bytes),
        Err(RejectCode::QuotaExceeded)
    );
}

#[test]
fn container_budget_rejects_before_replacing_slot() {
    let catalog = catalog();
    let mut inventory = Inventory::new(64).unwrap();
    let mut stack = ItemStack::new(key("sample:part"), 1);
    stack.data = (0..3)
        .map(|i| {
            (
                key(&format!("sample:t{i}")),
                ComponentValue::Text("a".repeat(1024)),
            )
        })
        .collect();
    let mut rejected = false;
    for slot in 0..64 {
        let before = inventory.clone();
        match inventory.set(slot, Some(stack.clone()), &catalog) {
            Ok(()) => assert!(torse_world::component_size(&inventory.fields()).is_ok()),
            Err(RejectCode::QuotaExceeded) => {
                assert_eq!(inventory, before);
                rejected = true;
                break;
            }
            other => panic!("unexpected result {other:?}"),
        }
    }
    assert!(rejected);
    inventory
        .set(63, Some(ItemStack::new(key("sample:part"), 1)), &catalog)
        .unwrap();
}

#[test]
fn validator_registration_is_owned_frozen_and_compatibility_bound() {
    use torse_content::{compile, m0_fixture_pack};
    use torse_types::Compatibility;
    use torse_world::{ComponentError, ComponentSchemas, ComponentSet};
    let mut pack = m0_fixture_pack();
    torse_inventory::register_content(&mut pack);
    let content = compile(pack).unwrap();
    let catalog = catalog();
    let mut schemas = ComponentSchemas::build(&content);
    assert!(matches!(
        schemas.register_validator(
            &key("other:module"),
            &torse_inventory::schema(),
            std::sync::Arc::new(catalog.clone())
        ),
        Err(ComponentError::InvalidConfiguration { .. })
    ));
    catalog.bind_schemas(&mut schemas).unwrap();
    assert!(catalog.bind_schemas(&mut schemas).is_err());
    let base = Compatibility {
        authority_schema: 3,
        rules_digest: content.authority_digest,
        content_digest: content.presentation_digest,
    };
    let (mut schemas, bound) = schemas.bind_compatibility(base).unwrap();
    assert_ne!(base.rules_digest, bound.rules_digest);
    assert_eq!(base.content_digest, bound.content_digest);
    assert_eq!(
        catalog.bind_schemas(&mut schemas),
        Err(ComponentError::ConfigurationFrozen)
    );
    let mut inventory = Inventory::new(1).unwrap();
    inventory
        .set(0, Some(ItemStack::new(key("sample:part"), 1)), &catalog)
        .unwrap();
    let mut fields = inventory.fields();
    let ComponentValue::List(slots) = fields.get_mut(&key("inventory:field/slots")).unwrap() else {
        panic!()
    };
    let ComponentValue::Record(stack) = &mut slots[0] else {
        panic!()
    };
    stack.insert(key("inventory:field/data_format"), ComponentValue::U32(99));
    assert!(ComponentSet::validate(&schemas, &torse_inventory::schema(), &fields).is_err());
}

#[test]
fn canonical_stack_wire_vector() {
    use sha2::{Digest as _, Sha256};
    let catalog = catalog();
    let mut stack = ItemStack::new(key("sample:part"), 2);
    stack.data = payload(ComponentValue::U32(7));
    let mut inventory = Inventory::new(1).unwrap();
    inventory.set(0, Some(stack), &catalog).unwrap();
    let value = ComponentValue::Record(inventory.fields());
    let budget = torse_types::OutputBudget {
        max_items: 256,
        max_bytes: 4096,
    };
    let limits = torse_codec::CodecLimits {
        max_frame_bytes: 4096,
        max_field_bytes: 4096,
        max_depth: 12,
    };
    let mut bytes = Vec::new();
    torse_codec::wire::encode(&value, &mut bytes, budget, limits).unwrap();
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(bytes.len(), 177);
    assert_eq!(
        digest,
        "13a3b0ec313f2a78f3a75b4dec03c9a6b9670cf31291cfc85d305a06670e600f"
    );
    assert_eq!(
        torse_codec::wire::decode::<ComponentValue>(&bytes, budget, limits).unwrap(),
        value
    );
    assert!(
        torse_codec::wire::decode::<ComponentValue>(&bytes[..bytes.len() - 1], budget, limits)
            .is_err()
    );
}
