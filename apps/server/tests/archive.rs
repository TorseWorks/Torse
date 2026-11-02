//! Offline migration preserves unavailable content and never edits the source world.
mod support;

use support::Directory;
use torse_codec::wire;
use torse_content::{
    CompiledContent, ComponentField, ComponentFieldType, ComponentSchema, PersistencePolicy,
    SourceRef, Sourced, StateSchema, compile, m0_fixture_pack,
};
use torse_server::{archive as migration, bootstrap as base, execution::*, *};
use torse_sim::action::ActionService;
use torse_sim::durable::*;
use torse_sim::{Recovery, WorldAuthority};
use torse_types::*;
use torse_world::*;

fn key(s: &str) -> ContentKey {
    ContentKey::parse(s).unwrap()
}
fn content(version: u32, plugin: bool) -> CompiledContent {
    let mut pack = m0_fixture_pack();
    let component = &mut pack
        .state_schemas
        .iter_mut()
        .find(|s| s.value.id == key("base:state/container"))
        .unwrap()
        .value
        .component_schemas[0];
    component.schema_version = version;
    component.fields = vec![
        ComponentField {
            id: key(if version == 1 { "base:old" } else { "base:new" }),
            field_type: ComponentFieldType::Unsigned32,
            required: true,
        },
        ComponentField {
            id: key("base:payload"),
            field_type: ComponentFieldType::Record,
            required: false,
        },
    ];
    if version > 1 {
        component.migration_binding = Some(key("base:migrate/inventory"));
    }
    if plugin {
        pack.state_schemas.push(Sourced::new(
            SourceRef::new("archive-plugin", 1),
            StateSchema {
                id: key("plugin:state"),
                schema_version: 1,
                finite_fields: vec![],
                component_schemas: vec![ComponentSchema {
                    id: key("plugin:data"),
                    schema_version: 1,
                    owner_module: key("plugin:module"),
                    fields: vec![ComponentField {
                        id: key("plugin:text"),
                        field_type: ComponentFieldType::String { max_bytes: 64 },
                        required: true,
                    }],
                    persistence: PersistencePolicy::Always,
                    replication: ReplicationDeclaration::SERVER_PRIVATE,
                    migration_binding: None,
                }],
            },
        ));
        let mut block = pack
            .blocks
            .iter()
            .find(|b| b.value.id == key("base:stone"))
            .unwrap()
            .clone();
        block.value.id = key("plugin:block");
        block.value.state_schema = key("plugin:state");
        pack.blocks.push(block);
    }
    compile(pack).unwrap()
}
fn compatibility(content: &CompiledContent) -> Compatibility {
    Compatibility {
        authority_schema: 1,
        rules_digest: content.authority_digest,
        content_digest: content.authority_digest,
    }
}
fn fields(value: u32) -> ComponentFields {
    [
        (key("base:old"), ComponentValue::U32(value)),
        (
            key("base:payload"),
            ComponentValue::Record(
                [
                    (key("test:reference"), ComponentValue::Entity(EntityId(1))),
                    (
                        key("test:values"),
                        ComponentValue::List(vec![
                            ComponentValue::Bool(true),
                            ComponentValue::I64(-7),
                            ComponentValue::Content(key("plugin:block")),
                            ComponentValue::Text("preserved".into()),
                        ]),
                    ),
                ]
                .into(),
            ),
        ),
    ]
    .into()
}
fn world(content: &CompiledContent) -> (WorldAuthority, ActionService) {
    let schemas = ComponentSchemas::build(content);
    let mut world = WorldAuthority::new(
        base::WORLD,
        BlockStateRegistry::build(content).unwrap(),
        schemas.clone(),
        2,
        WorldBounds::new(0, 8).unwrap(),
        compatibility(content),
    )
    .unwrap();
    world.store_mut().request(SectionPos::ZERO).unwrap();
    world
        .store_mut()
        .install(SectionPos::ZERO, Section::empty(2).unwrap())
        .unwrap();
    world.store_mut().finish_load(SectionPos::ZERO).unwrap();
    let state = world.registry().default_state(&key("base:stone")).unwrap();
    world
        .store_mut()
        .apply_edits(&[BlockEdit {
            pos: BlockPos::new(0, 0, 0),
            state,
        }])
        .unwrap();
    if content.blocks.contains_key(&key("plugin:block")) {
        let state = world
            .registry()
            .default_state(&key("plugin:block"))
            .unwrap();
        world
            .store_mut()
            .apply_edits(&[BlockEdit {
                pos: BlockPos::new(1, 0, 0),
                state,
            }])
            .unwrap();
    }
    let handle = world
        .entities_mut()
        .spawn(Position::new(1.5, 3.5, 1.5))
        .unwrap();
    world
        .entities_mut()
        .components_mut(handle)
        .unwrap()
        .insert(&schemas, &key("base:schema/inventory_content"), fields(7))
        .unwrap();
    if schemas.get(&key("plugin:data")).is_some() {
        world
            .entities_mut()
            .components_mut(handle)
            .unwrap()
            .insert(
                &schemas,
                &key("plugin:data"),
                [(
                    key("plugin:text"),
                    ComponentValue::Text("opaque data".into()),
                )]
                .into(),
            )
            .unwrap();
    }
    let actions = ActionService::bind(vec![], vec![], &schemas).unwrap();
    (world, actions)
}
fn archive(content: &CompiledContent) -> SnapshotArchive {
    let (world, _) = world(content);
    let meta = SnapshotMeta {
        cursor: CommitCursor::initial(base::WORLD),
        compatibility: compatibility(content),
        resume_at: world.coordinator().resume_point(),
    };
    SnapshotArchive::capture(
        &world.capture_snapshot(meta).unwrap(),
        content,
        base::budget(),
    )
    .unwrap()
}
fn upgrade(source: &ComponentFields) -> Result<ComponentFields, Fault> {
    let Some(ComponentValue::U32(value)) = source.get(&key("base:old")) else {
        return Err(Fault::invalid_data("old field missing"));
    };
    let mut fields = source.clone();
    fields.remove(&key("base:old"));
    fields.insert(
        key("base:new"),
        ComponentValue::U32(
            value
                .checked_mul(2)
                .ok_or_else(|| Fault::invalid_data("migration overflow"))?,
        ),
    );
    Ok(fields)
}
fn plan(source: &SnapshotArchive, target: &CompiledContent) -> MigrationPlan {
    let mut plan = MigrationPlan::new(source.snapshot.meta.compatibility, compatibility(target));
    plan.bind_component(
        &ComponentSchemas::build(target),
        &key("base:inventory"),
        key("base:schema/inventory_content"),
        1,
        key("base:migrate/inventory"),
        upgrade,
    )
    .unwrap();
    plan
}

#[test]
fn missing_package_roundtrips_unchanged_and_can_be_restored_after_reinstallation() {
    let full = content(1, true);
    let missing = content(1, false);
    let original = archive(&full);
    let mut bytes = vec![];
    wire::encode(&original, &mut bytes, base::budget(), base::codec_limits()).unwrap();
    let decoded: SnapshotArchive =
        wire::decode(&bytes, base::budget(), base::codec_limits()).unwrap();
    assert_eq!(decoded, original);
    let issues = decoded.inspect(&missing, base::budget()).unwrap();
    assert!(
        issues.iter().any(
            |i| matches!(i, ArchiveIssue::Block { block, .. } if *block == key("plugin:block"))
        )
    );
    assert!(
        issues.iter().any(
            |i| matches!(i, ArchiveIssue::Component { schema } if *schema == key("plugin:data"))
        )
    );
    let transition = MigrationPlan::new(
        original.snapshot.meta.compatibility,
        compatibility(&missing),
    );
    let retained = original
        .migrate(&missing, &transition, base::budget())
        .unwrap();
    assert_eq!(retained.snapshot.sections, original.snapshot.sections);
    assert_eq!(retained.snapshot.entities, original.snapshot.entities);
    assert_eq!(
        retained.snapshot.next_entity_id,
        original.snapshot.next_entity_id
    );
    assert!(
        retained
            .validate_for_restore(&missing, base::budget())
            .is_err()
    );
    let restore = MigrationPlan::new(retained.snapshot.meta.compatibility, compatibility(&full));
    let restored = retained.migrate(&full, &restore, base::budget()).unwrap();
    restored
        .validate_for_restore(&full, base::budget())
        .unwrap();
    assert_eq!(restored, original);
    let schemas = ComponentSchemas::build(&full);
    let registry = BlockStateRegistry::build(&full).unwrap();
    let mut recovery = Recovery::new(base::WORLD, &registry, &schemas);
    recovery
        .restore_archive(&restored, &full, base::budget())
        .unwrap();
    let world = recovery.finish().unwrap();
    assert_eq!(world.entities().next_entity_id(), 2);
}

#[test]
fn explicit_component_upgrade_keeps_identity_position_nested_fields_and_allocator() {
    let source = archive(&content(1, true));
    let original = source.clone();
    let target = content(2, true);
    let candidate = source
        .migrate(&target, &plan(&source, &target), base::budget())
        .unwrap();
    candidate
        .validate_for_restore(&target, base::budget())
        .unwrap();
    assert_eq!(source, original);
    assert_eq!(candidate.origin, source.origin);
    assert_eq!(
        candidate.snapshot.entities[0].id,
        source.snapshot.entities[0].id
    );
    assert_eq!(
        candidate.snapshot.entities[0].position_bits,
        source.snapshot.entities[0].position_bits
    );
    assert_eq!(
        candidate.snapshot.next_entity_id,
        source.snapshot.next_entity_id
    );
    let fields = &candidate.snapshot.entities[0].components[0].fields;
    assert_eq!(fields[0], (key("base:new"), SnapshotValue::U32(14)));
    assert_eq!(
        fields[1],
        source.snapshot.entities[0].components[0].fields[1]
    );
    assert_eq!(
        candidate.snapshot.entities[0].components[1],
        source.snapshot.entities[0].components[1]
    );
}

#[test]
fn missing_handlers_owners_versions_invalid_output_and_budgets_leave_original_unchanged() {
    fn fail(_: &ComponentFields) -> Result<ComponentFields, Fault> {
        Err(Fault::invalid_data("migration failed"))
    }
    fn invalid(_: &ComponentFields) -> Result<ComponentFields, Fault> {
        Ok(Default::default())
    }
    let source = archive(&content(1, false));
    let before = source.clone();
    let target = content(2, false);
    let schemas = ComponentSchemas::build(&target);
    let mut no_handler =
        MigrationPlan::new(source.snapshot.meta.compatibility, compatibility(&target));
    assert!(
        source
            .migrate(&target, &no_handler, base::budget())
            .is_err()
    );
    assert!(
        no_handler
            .bind_component(
                &schemas,
                &key("other:module"),
                key("base:schema/inventory_content"),
                1,
                key("base:migrate/inventory"),
                upgrade
            )
            .is_err()
    );
    assert!(
        no_handler
            .bind_component(
                &schemas,
                &key("base:inventory"),
                key("base:schema/inventory_content"),
                2,
                key("base:migrate/inventory"),
                upgrade
            )
            .is_err()
    );
    for callback in [fail as MigrateFields, invalid as MigrateFields] {
        let mut bad =
            MigrationPlan::new(source.snapshot.meta.compatibility, compatibility(&target));
        bad.bind_component(
            &schemas,
            &key("base:inventory"),
            key("base:schema/inventory_content"),
            1,
            key("base:migrate/inventory"),
            callback,
        )
        .unwrap();
        assert!(source.migrate(&target, &bad, base::budget()).is_err());
    }
    for budget in [
        OutputBudget {
            max_items: 1,
            ..base::budget()
        },
        OutputBudget {
            max_bytes: 32,
            ..base::budget()
        },
    ] {
        assert!(
            source
                .migrate(&target, &plan(&source, &target), budget)
                .is_err()
        );
    }
    assert_eq!(source, before);
}

#[test]
fn legacy_data_is_preserved_without_guessing_versions_and_requires_a_trusted_manifest() {
    let old_content = content(1, false);
    let source = archive(&old_content);
    let legacy = SnapshotArchive::import_legacy(
        &source.snapshot,
        ArchiveManifest::default(),
        base::budget(),
    )
    .unwrap();
    assert_eq!(legacy.snapshot, source.snapshot);
    assert!(
        legacy
            .inspect(&old_content, base::budget())
            .unwrap()
            .iter()
            .any(|i| matches!(i, ArchiveIssue::MissingStamp { .. }))
    );
    assert!(
        legacy
            .validate_for_restore(&old_content, base::budget())
            .is_err()
    );
    let target = content(2, false);
    assert!(
        legacy
            .migrate(&target, &plan(&source, &target), base::budget())
            .is_err()
    );
    let stamped =
        SnapshotArchive::import_legacy(&source.snapshot, source.manifest.clone(), base::budget())
            .unwrap();
    stamped
        .migrate(&target, &plan(&stamped, &target), base::budget())
        .unwrap()
        .validate_for_restore(&target, base::budget())
        .unwrap();
}

#[test]
fn finite_state_versions_are_per_entry_and_require_each_state_to_be_explicitly_mapped() {
    let source_content = content(1, false);
    let mut source = archive(&source_content);
    source.snapshot.states = vec![
        StateEntry {
            id: BlockStateId(1),
            block: key("base:glass_slab"),
            values: vec![0],
        },
        StateEntry {
            id: BlockStateId(2),
            block: key("base:glass_slab"),
            values: vec![1],
        },
    ];
    source.manifest.blocks = [1, 2]
        .into_iter()
        .map(|id| {
            (
                BlockStateId(id),
                BlockStamp {
                    definition_version: 1,
                    schema: key("base:state/slab"),
                    schema_version: 1,
                },
            )
        })
        .collect();
    source.snapshot.sections[0].cells = SnapshotCells::Dense(vec![
        BlockStateId(1),
        BlockStateId(2),
        BlockStateId::EMPTY,
        BlockStateId::EMPTY,
        BlockStateId::EMPTY,
        BlockStateId::EMPTY,
        BlockStateId::EMPTY,
        BlockStateId::EMPTY,
    ]);
    let mut pack = m0_fixture_pack();
    pack.state_schemas
        .iter_mut()
        .find(|s| s.value.id == key("base:state/slab"))
        .unwrap()
        .value
        .schema_version = 2;
    let mut target = compile(pack).unwrap();
    // Keep the unchanged attached component declaration from the source.
    target.state_schemas.insert(
        key("base:state/container"),
        source_content.state_schemas[&key("base:state/container")].clone(),
    );
    let mut plan = MigrationPlan::new(source.snapshot.meta.compatibility, compatibility(&target));
    plan.remap_block(
        key("base:glass_slab"),
        vec![0],
        key("base:glass_slab"),
        vec![0],
    )
    .unwrap();
    let partial = source.migrate(&target, &plan, base::budget()).unwrap();
    assert!(
        partial
            .validate_for_restore(&target, base::budget())
            .is_err()
    );
    assert_eq!(partial.manifest.blocks[&BlockStateId(2)].schema_version, 1);
    plan.remap_block(
        key("base:glass_slab"),
        vec![1],
        key("base:glass_slab"),
        vec![1],
    )
    .unwrap();
    source
        .migrate(&target, &plan, base::budget())
        .unwrap()
        .validate_for_restore(&target, base::budget())
        .unwrap();
}

#[test]
fn archive_wire_roundtrip_truncation_version_and_budget_rollback() {
    let source = archive(&content(1, true));
    let mut bytes = vec![];
    wire::encode(&source, &mut bytes, base::budget(), base::codec_limits()).unwrap();
    for end in 0..bytes.len() {
        assert!(
            wire::decode::<SnapshotArchive>(&bytes[..end], base::budget(), base::codec_limits())
                .is_err()
        );
    }
    let mut bad = bytes.clone();
    bad[2] ^= 1;
    assert!(wire::decode::<SnapshotArchive>(&bad, base::budget(), base::codec_limits()).is_err());
    let mut out = vec![9];
    assert!(
        wire::encode(
            &source,
            &mut out,
            OutputBudget::new(1, 32),
            base::codec_limits()
        )
        .is_err()
    );
    assert_eq!(out, [9]);
    assert!(
        wire::decode::<SnapshotArchive>(&bytes, OutputBudget::new(1, 32), base::codec_limits())
            .is_err()
    );
}

#[test]
fn execution_archive_keeps_pending_work_and_refuses_implicit_event_or_rule_migration() {
    let old = content(1, false);
    let target = content(2, false);
    let mut source = ExecutionArchive {
        world: archive(&old),
        runtime: RuntimeState {
            rules: vec![].into(),
            next_event: 2,
            events: vec![TimedEvent {
                id: EventId(1),
                event: EventDraft {
                    tick: Tick(9),
                    rule: key("missing:rule"),
                    payload: [(key("test:old"), ComponentValue::U32(7))].into(),
                },
            }],
            windows: vec![],
            work: None,
        },
    };
    let mut bytes = vec![];
    wire::encode(&source, &mut bytes, base::budget(), base::codec_limits()).unwrap();
    assert_eq!(
        wire::decode::<ExecutionArchive>(&bytes, base::budget(), base::codec_limits()).unwrap(),
        source
    );
    assert!(
        migration::migrate(
            &source,
            &target,
            &[],
            &plan(&source.world, &target),
            base::budget(),
            base::codec_limits()
        )
        .is_err()
    );
    source.runtime.events.clear();
    source.runtime.work = Some(PhaseWork {
        event_batch: false,
        tasks: vec![FrozenTask {
            transaction: None,
            request: None,
            consume: None,
            schedule: vec![],
            cancel: vec![],
            status: ReplyStatus::ReadOnly,
            output: Default::default(),
        }]
        .into(),
        next: 0,
    });
    assert!(
        migration::migrate(
            &source,
            &target,
            &[],
            &plan(&source.world, &target),
            base::budget(),
            base::codec_limits()
        )
        .is_err()
    );
    let mut preserved = vec![];
    wire::encode(
        &source,
        &mut preserved,
        base::budget(),
        base::codec_limits(),
    )
    .unwrap();
    assert_eq!(
        wire::decode::<ExecutionArchive>(&preserved, base::budget(), base::codec_limits()).unwrap(),
        source
    );
}

fn read_files(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut files = vec![];
    for item in std::fs::read_dir(root).unwrap() {
        let path = item.unwrap().path();
        if path.is_dir() {
            files.extend(read_files(&path));
        } else {
            files.push((path.clone(), std::fs::read(path).unwrap()));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

#[test]
fn migrated_fork_imports_into_a_new_directory_and_source_files_never_change() {
    let old = content(1, false);
    let target = content(2, false);
    let dir = Directory::new();
    let destination = Directory::new();
    let (world, actions) = world(&old);
    let mut server = base::Files::create_file(
        dir.acquire(),
        compatibility(&old),
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| ExecutionHost::new(binding, world, actions, vec![], base::execution_limits()),
    )
    .unwrap();
    server.drive_tick(&mut NoReplicas, 1024).unwrap();
    let source = server.capture_archive(&old, base::budget()).unwrap();
    drop(server);
    let before = read_files(&dir.0);
    let candidate = migration::migrate(
        &source,
        &target,
        &[],
        &plan(&source.world, &target),
        base::budget(),
        base::codec_limits(),
    )
    .unwrap();
    assert!(
        migration::fork_for_import(
            &candidate,
            base::WORLD,
            &target,
            base::budget(),
            base::codec_limits()
        )
        .is_err()
    );
    let new_world = WorldId(99);
    let imported = migration::fork_for_import(
        &candidate,
        new_world,
        &target,
        base::budget(),
        base::codec_limits(),
    )
    .unwrap();
    let schemas = ComponentSchemas::build(&target);
    let registry = BlockStateRegistry::build(&target).unwrap();
    let mut created = base::Files::create_file(
        WorldDirectory::acquire(&destination.0, new_world, 8192).unwrap(),
        compatibility(&target),
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            let actions = ActionService::bind(vec![], vec![], &schemas).unwrap();
            let mut recovery = ExecutionRecovery::new(
                binding,
                &registry,
                &schemas,
                actions,
                vec![],
                base::execution_limits(),
            );
            recovery.restore_archive(&imported, &target, base::budget(), base::codec_limits())?;
            recovery.finish(imported.world.snapshot.meta.cursor)
        },
    )
    .unwrap();
    assert_eq!(
        created
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id(),
        2
    );
    created.drive_tick(&mut NoReplicas, 1024).unwrap();
    drop(created);
    let opened = base::Files::open_file(
        WorldDirectory::acquire(&destination.0, new_world, 8192).unwrap(),
        compatibility(&target),
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                &registry,
                &schemas,
                ActionService::bind(vec![], vec![], &schemas).unwrap(),
                vec![],
                base::execution_limits(),
            ))
        },
    )
    .unwrap();
    let entity = opened
        .authority()
        .unwrap()
        .world()
        .entities()
        .iter()
        .next()
        .unwrap()
        .1;
    assert_eq!(
        entity
            .components
            .component(&key("base:schema/inventory_content"))
            .unwrap()[&key("base:new")],
        ComponentValue::U32(14)
    );
    assert_eq!(read_files(&dir.0), before);
    let old_schemas = ComponentSchemas::build(&old);
    let old_registry = BlockStateRegistry::build(&old).unwrap();
    let reopened_source = base::Files::open_file(
        dir.acquire(),
        compatibility(&old),
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                &old_registry,
                &old_schemas,
                ActionService::bind(vec![], vec![], &old_schemas).unwrap(),
                vec![],
                base::execution_limits(),
            ))
        },
    )
    .unwrap();
    assert_eq!(
        reopened_source.authority().unwrap().resume_point().tick,
        Tick(1)
    );
}

#[test]
fn residency_archive_merges_parked_pages_without_changing_the_running_world() {
    let content = content(1, false);
    let dir = Directory::new();
    let root = dir.0.join("sections");
    std::fs::create_dir(&root).unwrap();
    let pages =
        section_pages::SectionPages::new(root, base::budget(), base::codec_limits(), 64).unwrap();
    let (mut world, actions) = world(&content);
    let parked = SectionPos::new(0, 1, 0);
    world.store_mut().request(parked).unwrap();
    world
        .store_mut()
        .install(parked, Section::empty(2).unwrap())
        .unwrap();
    world.store_mut().finish_load(parked).unwrap();
    let expected = world
        .capture_snapshot(torse_sim::streaming::page_meta(&world))
        .unwrap();
    let mut server = base::Files::create_file(
        dir.acquire(),
        compatibility(&content),
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            ExecutionHost::new(
                binding,
                world,
                actions,
                Vec::new(),
                base::execution_limits(),
            )
        },
    )
    .unwrap();
    assert!(matches!(
        server
            .unload_section(&pages, parked, &mut NoReplicas)
            .unwrap(),
        CommitStep::Durable(_)
    ));
    assert!(server.capture_archive(&content, base::budget()).is_err());
    struct Source;
    impl residency::GenerationSource for Source {
        fn identity(&self) -> GenerationIdentity {
            GenerationIdentity {
                id: key("test:archive-generator"),
                version: 1,
                configuration: Digest([9; 32]),
            }
        }
        fn generate(
            &self,
            _: residency::GenerationContext,
            _: SectionPos,
        ) -> Result<Snapshot, Fault> {
            Err(Fault::invalid_data("archive test does not run generation"))
        }
    }
    server
        .enable_residency(
            residency::ResidencyConfig {
                max_resident: 4,
                max_io_bytes: 32 * 1024 * 1024,
            },
            pages.clone(),
            Some(std::sync::Arc::new(Source)),
            &[SectionPos::ZERO, parked],
        )
        .unwrap();
    let before = read_files(&dir.0.join("sections"));
    let cursor = server.authority().unwrap().cursor();
    let archive = server
        .capture_residency_archive(&content, &pages, base::budget(), base::codec_limits())
        .unwrap();
    assert_eq!(archive.execution.world.snapshot.sections, expected.sections);
    assert_eq!(archive.execution.world.snapshot.entities, expected.entities);
    assert_eq!(
        archive.execution.world.snapshot.next_entity_id,
        expected.next_entity_id
    );
    assert_eq!(server.authority().unwrap().cursor(), cursor);
    assert!(
        server
            .authority()
            .unwrap()
            .parked_sections()
            .contains_key(&parked)
    );
    assert_eq!(read_files(&dir.0.join("sections")), before);
    let mut bytes = Vec::new();
    wire::encode(&archive, &mut bytes, base::budget(), base::codec_limits()).unwrap();
    let restored: migration::ResidencyArchive =
        wire::decode(&bytes, base::budget(), base::codec_limits()).unwrap();
    assert_eq!(restored, archive);
    let (template, actions) = self::world(&content);
    let mut recovery = ExecutionRecovery::new(
        server.binding(),
        template.registry(),
        template.schemas(),
        actions,
        Vec::new(),
        base::execution_limits(),
    );
    assert!(
        recovery
            .restore_residency_archive(
                &restored,
                None,
                &content,
                base::budget(),
                base::codec_limits()
            )
            .is_err()
    );
    recovery
        .restore_residency_archive(
            &restored,
            restored.generation.as_ref(),
            &content,
            base::budget(),
            base::codec_limits(),
        )
        .unwrap();
    let recovered = recovery.finish(cursor).unwrap();
    assert!(recovered.parked_sections().is_empty());
    assert_eq!(recovered.world().store().len(), expected.sections.len());
    assert!(
        server
            .capture_residency_archive(
                &content,
                &pages,
                OutputBudget::new(1, 64),
                base::codec_limits()
            )
            .is_err()
    );
    assert_eq!(read_files(&dir.0.join("sections")), before);
}
