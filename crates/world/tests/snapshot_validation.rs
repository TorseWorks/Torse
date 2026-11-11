use torse_content::{
    ComponentField, ComponentFieldType, ComponentSchema, PersistencePolicy, compile,
    m0_fixture_pack,
};
use torse_types::{
    CommitCursor, CommitSeq, Compatibility, ContentKey, Digest, EntityId, FaultCode, OutputBudget,
    ReplicationDeclaration, ResumePoint, SectionPos, SnapshotMeta, Tick, WorldId,
};
use torse_world::*;
fn key(id: &str) -> ContentKey {
    ContentKey::parse(id).unwrap()
}
fn fixture() -> (Snapshot, BlockStateRegistry, ComponentSchemas) {
    let mut pack = m0_fixture_pack();
    for (id, persistence) in [
        ("test:cache", PersistencePolicy::Never),
        ("test:data", PersistencePolicy::Always),
    ] {
        pack.state_schemas[0]
            .value
            .component_schemas
            .push(ComponentSchema {
                id: key(id),
                schema_version: 1,
                owner_module: key("base:core"),
                fields: vec![ComponentField {
                    id: key("test:value"),
                    field_type: ComponentFieldType::Record,
                    required: true,
                }],
                persistence,
                replication: ReplicationDeclaration::SERVER_PRIVATE,
                migration_binding: None,
            });
    }
    let content = compile(pack).unwrap();
    let registry = BlockStateRegistry::build(&content).unwrap();
    let schemas = ComponentSchemas::build(&content);
    let mut store = SectionStore::new(2, WorldBounds::new(0, 2).unwrap()).unwrap();
    for (x, name) in [(0, "base:stone"), (1, "base:glass")] {
        let pos = SectionPos::new(x, 0, 0);
        store.request(pos).unwrap();
        store
            .install(
                pos,
                Section::new(2, registry.default_state(&key(name)).unwrap()).unwrap(),
            )
            .unwrap();
        store.finish_load(pos).unwrap();
    }
    let mut entities = EntityStore::new(2).unwrap();
    for x in [0.5, 2.5] {
        let handle = entities.spawn(Position::new(x, 0.5, 0.5)).unwrap();
        for id in ["test:cache", "test:data"] {
            entities
                .components_mut(handle)
                .unwrap()
                .insert(
                    &schemas,
                    &key(id),
                    [(
                        key("test:value"),
                        ComponentValue::Record([(key("test:a"), ComponentValue::U32(1))].into()),
                    )]
                    .into(),
                )
                .unwrap();
        }
    }
    let meta = SnapshotMeta {
        cursor: CommitCursor {
            world: WorldId(1),
            seq: CommitSeq(0),
        },
        compatibility: Compatibility {
            authority_schema: 4,
            rules_digest: Digest([1; 32]),
            content_digest: content.authority_digest,
        },
        resume_at: ResumePoint {
            tick: Tick(0),
            next_phase: 0,
            next_step: 0,
        },
    };
    (
        Snapshot::capture(meta, &store, &entities, &registry).unwrap(),
        registry,
        schemas,
    )
}
fn rejected(snapshot: &Snapshot, registry: &BlockStateRegistry, schemas: &ComponentSchemas) {
    let budget = OutputBudget::new(100_000, 8_000_000);
    assert!(snapshot.validate_structure().is_err());
    assert!(snapshot.restore(registry, schemas).is_err());
    assert!(
        snapshot
            .restore_persistent_with_budget(registry, schemas, budget)
            .is_err()
    );
    assert!(snapshot.persistent_view(schemas).is_err());
    assert!(SnapshotArchive::import_legacy(snapshot, ArchiveManifest::default(), budget).is_err());
}

#[test]
fn duplicate_or_reordered_identities_are_rejected_at_all_entry_points() {
    let (base, registry, schemas) = fixture();
    let mut variants = vec![];
    let mut value = base.clone();
    value.states.push(value.states.last().unwrap().clone());
    variants.push(value);
    let mut value = base.clone();
    value.states.reverse();
    variants.push(value);
    let mut value = base.clone();
    value.sections.push(value.sections.last().unwrap().clone());
    variants.push(value);
    let mut value = base.clone();
    value.sections.reverse();
    variants.push(value);
    let mut value = base.clone();
    value.entities.push(value.entities.last().unwrap().clone());
    variants.push(value);
    let mut value = base.clone();
    value.entities.reverse();
    variants.push(value);
    let mut value = base.clone();
    let components = &mut value.entities[0].components;
    components.push(components.last().unwrap().clone());
    variants.push(value);
    let mut value = base.clone();
    value.entities[0].components.reverse();
    variants.push(value);
    let mut value = base.clone();
    let fields = &mut value.entities[0].components[0].fields;
    fields.push(fields[0].clone());
    variants.push(value);
    for value in variants {
        rejected(&value, &registry, &schemas);
    }
    assert!(base.restore(&registry, &schemas).is_ok());
}

#[test]
fn record_duplicates_cannot_be_silently_collapsed_by_map_conversion() {
    let (base, registry, schemas) = fixture();
    for entries in [
        vec![
            (key("test:a"), SnapshotValue::U32(1)),
            (key("test:a"), SnapshotValue::U32(2)),
        ],
        vec![
            (key("test:z"), SnapshotValue::U32(1)),
            (key("test:a"), SnapshotValue::U32(2)),
        ],
    ] {
        let mut value = base.clone();
        value.entities[0].components[1].fields[0].1 = SnapshotValue::Record(entries);
        rejected(&value, &registry, &schemas);
    }
}

#[test]
fn invalid_extents_cells_and_reserved_identities_fail_before_restore() {
    let (base, registry, schemas) = fixture();
    let mut variants = vec![];
    let mut value = base.clone();
    value.side = 65;
    variants.push(value);
    let mut value = base.clone();
    value.bounds.min_y = value.bounds.max_y;
    variants.push(value);
    let mut value = base.clone();
    value.states[0].id = BlockStateId::EMPTY;
    variants.push(value);
    let mut value = base.clone();
    value.sections[0].cells = SnapshotCells::Uniform(BlockStateId(65535));
    variants.push(value);
    let mut value = base.clone();
    value.sections[0].cells = SnapshotCells::Dense(vec![BlockStateId::EMPTY; 7]);
    variants.push(value);
    let mut value = base.clone();
    value.sections[0].cells = SnapshotCells::Dense(vec![BlockStateId(65535); 8]);
    variants.push(value);
    let mut value = base.clone();
    value.entities[0].id = EntityId(0);
    variants.push(value);
    let mut value = base.clone();
    value.entities[0].position_bits = Position::new(f64::MAX, 0.5, 0.5).to_bits();
    variants.push(value);
    for value in variants {
        rejected(&value, &registry, &schemas);
    }
}

#[test]
fn depth_nodes_and_bytes_are_checked_before_runtime_only_projection() {
    let (base, registry, schemas) = fixture();
    fn nested(depth: usize) -> SnapshotValue {
        let mut value = SnapshotValue::Bool(true);
        for _ in 0..depth {
            value = SnapshotValue::List(vec![value]);
        }
        SnapshotValue::Record(vec![(key("test:a"), value)])
    }
    let mut deepest = base.clone();
    deepest.entities[0].components[0].fields[0].1 = nested(7);
    assert!(deepest.validate_structure().is_ok());
    assert!(deepest.restore(&registry, &schemas).is_ok());
    for oversized in [
        nested(8),
        nested(128),
        SnapshotValue::Text("x".repeat(65_536)),
        SnapshotValue::List(vec![SnapshotValue::Bool(true); 4096]),
    ] {
        let mut value = base.clone();
        value.entities[0].components[0].fields[0].1 = oversized;
        rejected(&value, &registry, &schemas);
    }
    let mut value = base.clone();
    value.entities[0].components = vec![value.entities[0].components[0].clone(); 65];
    rejected(&value, &registry, &schemas);
    let projected = base.persistent_view(&schemas).unwrap();
    assert_eq!(projected.entities[0].components.len(), 1);
    let budget = OutputBudget::new(100_000, 8_000_000);
    assert!(matches!(
        base.restore_persistent_with_budget(&registry, &schemas, budget),
        Err(error) if error.code == FaultCode::InvalidData
    ));
    assert!(matches!(
        projected.restore_persistent_with_budget(&registry, &schemas, OutputBudget::new(1, 128)),
        Err(error) if error.code == FaultCode::ResourceLimit
    ));
    let restored = projected
        .restore_persistent_with_budget(&registry, &schemas, budget)
        .unwrap();
    assert_eq!(
        Snapshot::capture(
            projected.meta,
            &restored.store,
            &restored.entities,
            &registry
        )
        .unwrap(),
        *projected
    );
}

#[test]
fn structural_validation_preserves_unknown_content_for_offline_inspection() {
    let (mut value, registry, schemas) = fixture();
    value.states[0].block = key("missing:block");
    assert!(value.validate_structure().is_ok());
    assert!(value.restore(&registry, &schemas).is_err());
    let archived = SnapshotArchive::import_legacy(
        &value,
        ArchiveManifest::default(),
        torse_types::OutputBudget::new(100_000, 8_000_000),
    )
    .unwrap();
    assert_eq!(archived.snapshot, value);
}

#[test]
fn valid_dense_uniform_and_component_payloads_restore_without_semantic_changes() {
    let (mut value, registry, schemas) = fixture();
    let fill = match value.sections[0].cells {
        SnapshotCells::Uniform(state) => state,
        _ => panic!(),
    };
    value.sections[0].cells = SnapshotCells::Dense(vec![fill; 8]);
    let restored = value.restore(&registry, &schemas).unwrap();
    let captured =
        Snapshot::capture(value.meta, &restored.store, &restored.entities, &registry).unwrap();
    assert_eq!(captured.entities, value.entities);
    assert_eq!(captured.next_entity_id, value.next_entity_id);
    assert_eq!(captured.sections[0].cells, SnapshotCells::Uniform(fill));
    assert!(captured.validate_structure().is_ok());
}
