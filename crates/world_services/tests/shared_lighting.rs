use std::sync::Arc;
use torse_content::{compile, m0_fixture_pack};
use torse_types::{
    Binding, BlockPos, Compatibility, ContentKey, Digest, SectionPos, WorldEpoch, WorldId,
};
use torse_world::{
    BlockStateId, BlockStateRegistry, Section, SnapshotCells, SnapshotSection, StateEntry,
    TerrainQuery, WorldBounds,
};
use torse_world_services::lighting::{
    LightFieldLimits, LightInputVersion, LightJob, LightLimits, LightSource, LightTerrain,
    SharedLightField,
};

fn setup() -> (LightTerrain, BlockStateRegistry, SharedLightField) {
    let mut pack = m0_fixture_pack();
    for block in &mut pack.blocks {
        block.value.light_transport.emission = if block.value.id.to_string() == "base:glass" {
            1.0
        } else {
            0.0
        };
    }
    let registry = BlockStateRegistry::build(&compile(pack).unwrap()).unwrap();
    let binding = Binding {
        world: WorldId(1),
        epoch: WorldEpoch(1),
        compatibility: Compatibility {
            authority_schema: 1,
            rules_digest: Digest([1; 32]),
            content_digest: Digest([7; 32]),
        },
    };
    let mut terrain = LightTerrain::new(
        binding,
        4,
        WorldBounds::new(0, 8).unwrap(),
        &registry,
        200,
        1_000_000,
    )
    .unwrap();
    for x in -4..=4 {
        for z in -4..=4 {
            for y in 0..2 {
                terrain
                    .insert(
                        SectionPos::new(x, y, z),
                        Section::empty(4).unwrap(),
                        &registry,
                    )
                    .unwrap();
            }
        }
    }
    let field = SharedLightField::new(
        binding,
        LightFieldLimits {
            sections: 16,
            cache_bytes: 1_000_000,
            consumers: 4,
            interests: 32,
            job: job_limits(),
        },
    )
    .unwrap();
    (terrain, registry, field)
}
fn job_limits() -> LightLimits {
    LightLimits {
        cells: 100_000,
        dependencies: 200,
        frontier: 200_000,
        bytes: 5_000_000,
    }
}
fn settle(field: &mut SharedLightField, terrain: &LightTerrain, registry: &BlockStateRegistry) {
    for _ in 0..1000 {
        field.poll(terrain, registry, 20_000).unwrap();
        if field.diagnostics().pending == 0 {
            return;
        }
    }
    panic!("field did not settle: {:?}", field.diagnostics());
}
fn emitter(registry: &BlockStateRegistry) -> BlockStateId {
    registry
        .default_state(&ContentKey::parse("base:glass").unwrap())
        .unwrap()
}

#[test]
fn two_consumers_share_one_calculation_and_content_survives_source_switches() {
    let (mut terrain, registry, mut field) = setup();
    let target = SectionPos::ZERO;
    field.subscribe(1, &[target]).unwrap();
    field.subscribe(2, &[target]).unwrap();
    settle(&mut field, &terrain, &registry);
    let first = field
        .publication(1, target, &terrain, &registry)
        .unwrap()
        .clone();
    assert!(Arc::ptr_eq(
        &first,
        field.publication(2, target, &terrain, &registry).unwrap()
    ));
    assert!(first.sky.is_complete());
    assert!(first.sky.levels().iter().all(|&n| n == 15));
    assert!(first.emitted.levels().iter().all(|&n| n == 0));
    assert_eq!(field.diagnostics().started, 1);
    assert_eq!(field.diagnostics().published, 1);
    // A freshly generated/native section and an authenticated saved-page body
    // have the same canonical identity, without borrowing storage revisions.
    assert!(
        !terrain
            .insert(target, Section::empty(4).unwrap(), &registry)
            .unwrap()
    );
    assert!(
        !terrain
            .insert_snapshot(
                &SnapshotSection {
                    pos: target,
                    cells: SnapshotCells::Uniform(BlockStateId::EMPTY)
                },
                &[],
                &registry
            )
            .unwrap()
    );
    field.inputs_changed(&terrain, &registry);
    assert_eq!(field.diagnostics().pending, 0);
    assert!(Arc::ptr_eq(
        &first,
        field.publication(2, target, &terrain, &registry).unwrap()
    ));
    field.unsubscribe(1);
    assert!(field.publication(1, target, &terrain, &registry).is_none());
    assert!(field.publication(2, target, &terrain, &registry).is_some());
    field.unsubscribe(2);
    assert_eq!(field.diagnostics().cache_bytes, 0);
}

#[test]
fn input_adapter_distinguishes_unknown_air_boundary_and_remaps_saved_symbols() {
    let (mut terrain, registry, _) = setup();
    let pos = SectionPos::ZERO;
    terrain.remove(pos);
    assert_eq!(terrain.version(pos), LightInputVersion::Unknown);
    assert_eq!(
        terrain.query(BlockPos::new(0, 0, 0)),
        TerrainQuery::NotLoaded
    );
    assert_eq!(
        terrain.version(SectionPos::new(0, -1, 0)),
        LightInputVersion::Boundary
    );
    assert_eq!(
        terrain.query(BlockPos::new(0, -1, 0)),
        TerrainQuery::OutOfBounds
    );
    let id = emitter(&registry);
    terrain
        .insert(pos, Section::new(4, id).unwrap(), &registry)
        .unwrap();
    let digest = terrain.version(pos);
    let symbols = vec![StateEntry {
        id: BlockStateId(65000),
        block: ContentKey::parse("base:glass").unwrap(),
        values: vec![],
    }];
    assert!(
        !terrain
            .insert_snapshot(
                &SnapshotSection {
                    pos,
                    cells: SnapshotCells::Uniform(BlockStateId(65000))
                },
                &symbols,
                &registry
            )
            .unwrap()
    );
    assert_eq!(terrain.version(pos), digest);
    assert!(
        terrain
            .insert_snapshot(
                &SnapshotSection {
                    pos,
                    cells: SnapshotCells::Uniform(BlockStateId(65001))
                },
                &symbols,
                &registry
            )
            .is_err()
    );
    assert_eq!(terrain.version(pos), digest);
}

#[test]
fn emitter_removal_neighbor_arrival_and_roof_changes_reject_old_publications() {
    let (mut terrain, registry, mut field) = setup();
    let target = SectionPos::ZERO;
    let neighbor = SectionPos::new(1, 0, 0);
    terrain
        .insert(
            neighbor,
            Section::new(4, emitter(&registry)).unwrap(),
            &registry,
        )
        .unwrap();
    field.subscribe(1, &[target]).unwrap();
    settle(&mut field, &terrain, &registry);
    let old = field
        .publication(1, target, &terrain, &registry)
        .unwrap()
        .clone();
    assert!(old.emitted.levels().iter().any(|&v| v > 0));
    terrain
        .insert(neighbor, Section::empty(4).unwrap(), &registry)
        .unwrap();
    assert!(!old.validate(&terrain, &registry));
    assert!(field.publication(1, target, &terrain, &registry).is_none());
    field.inputs_changed(&terrain, &registry);
    settle(&mut field, &terrain, &registry);
    assert!(
        field
            .publication(1, target, &terrain, &registry)
            .unwrap()
            .emitted
            .levels()
            .iter()
            .all(|&v| v == 0)
    );
    terrain.remove(neighbor);
    field.inputs_changed(&terrain, &registry);
    settle(&mut field, &terrain, &registry);
    let incomplete = field
        .publication(1, target, &terrain, &registry)
        .unwrap()
        .clone();
    assert!(!incomplete.emitted.is_complete());
    terrain
        .insert(neighbor, Section::empty(4).unwrap(), &registry)
        .unwrap();
    assert!(!incomplete.validate(&terrain, &registry));
    field.inputs_changed(&terrain, &registry);
    settle(&mut field, &terrain, &registry);
    assert!(
        field
            .publication(1, target, &terrain, &registry)
            .unwrap()
            .emitted
            .is_complete()
    );
    let roof = SectionPos::new(0, 1, 0);
    let solid = registry
        .default_state(&ContentKey::parse("base:stone").unwrap())
        .unwrap();
    terrain
        .insert(roof, Section::new(4, solid).unwrap(), &registry)
        .unwrap();
    field.inputs_changed(&terrain, &registry);
    settle(&mut field, &terrain, &registry);
    assert!(
        field
            .publication(1, target, &terrain, &registry)
            .unwrap()
            .sky
            .levels()
            .iter()
            .any(|&v| v < 15)
    );
    assert_eq!(field.diagnostics().pending, 0);
}

#[test]
fn stale_jobs_and_over_budget_interests_leave_no_partial_publication() {
    let (mut terrain, registry, mut field) = setup();
    field.subscribe(1, &[SectionPos::ZERO]).unwrap();
    field.poll(&terrain, &registry, 1).unwrap();
    terrain
        .insert(
            SectionPos::ZERO,
            Section::new(4, emitter(&registry)).unwrap(),
            &registry,
        )
        .unwrap();
    assert!(field.poll(&terrain, &registry, 10).is_err());
    assert!(
        field
            .publication(1, SectionPos::ZERO, &terrain, &registry)
            .is_none()
    );
    settle(&mut field, &terrain, &registry);
    assert_eq!(field.diagnostics().stale, 1);
    let old = field
        .publication(1, SectionPos::ZERO, &terrain, &registry)
        .unwrap()
        .clone();
    assert!(field.subscribe(1, &[SectionPos::ZERO; 17]).is_err());
    assert!(Arc::ptr_eq(
        &old,
        field
            .publication(1, SectionPos::ZERO, &terrain, &registry)
            .unwrap()
    ));
    let mut wrong = terrain.binding();
    wrong.epoch = WorldEpoch(2);
    assert!(LightJob::prepare(wrong, &terrain, &registry, SectionPos::ZERO, job_limits()).is_err());
    assert!(
        LightJob::prepare_sky(wrong, &terrain, &registry, SectionPos::ZERO, job_limits()).is_err()
    );
}

fn cold(
    terrain: &LightTerrain,
    registry: &BlockStateRegistry,
    target: SectionPos,
    sky: bool,
) -> Vec<u8> {
    let mut job = if sky {
        LightJob::prepare_sky(terrain.binding(), terrain, registry, target, job_limits()).unwrap()
    } else {
        LightJob::prepare(terrain.binding(), terrain, registry, target, job_limits()).unwrap()
    };
    while !job
        .poll(terrain.binding(), terrain, registry, 50_000)
        .unwrap()
    {}
    job.finish(terrain.binding(), terrain, registry)
        .unwrap()
        .levels()
        .to_vec()
}
#[test]
fn incremental_increases_and_decreases_match_cold_solves_after_each_edit() {
    let (mut terrain, registry, mut field) = setup();
    let target = SectionPos::ZERO;
    field.subscribe(1, &[target]).unwrap();
    settle(&mut field, &terrain, &registry);
    let mut sections = std::collections::BTreeMap::new();
    let stone = registry
        .default_state(&ContentKey::parse("base:stone").unwrap())
        .unwrap();
    let mut random = 0x4a9817u64;
    let mut changed = 0;
    for edit in 0..40 {
        random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
        let pos = SectionPos::new(((random >> 32) & 1) as i64, ((random >> 35) & 1) as i64, 0);
        let cells = sections
            .entry(pos)
            .or_insert_with(|| vec![BlockStateId::EMPTY; 64]);
        let index = ((random >> 40) % 64) as usize;
        cells[index] = match edit % 3 {
            0 => emitter(&registry),
            1 => stone,
            _ => BlockStateId::EMPTY,
        };
        if terrain
            .insert(
                pos,
                Section::from_cells(4, cells.clone()).unwrap(),
                &registry,
            )
            .unwrap()
        {
            changed += 1;
        }
        field.inputs_changed(&terrain, &registry);
        settle(&mut field, &terrain, &registry);
        let output = field.publication(1, target, &terrain, &registry).unwrap();
        assert_eq!(
            output.emitted.levels(),
            cold(&terrain, &registry, target, false),
            "emission edit {edit}"
        );
        assert_eq!(
            output.sky.levels(),
            cold(&terrain, &registry, target, true),
            "sky edit {edit}"
        );
    }
    assert_eq!(field.diagnostics().incremental_started, changed);
    assert!(field.diagnostics().update_frontier_visits > 0);
    assert_eq!(field.diagnostics().pending, 0);
}

#[test]
fn an_edit_does_not_recompute_a_disjoint_target() {
    let (mut terrain, registry, mut field) = setup();
    let far = SectionPos::new(100, 0, 0);
    terrain
        .insert(far, Section::empty(4).unwrap(), &registry)
        .unwrap();
    field.subscribe(1, &[SectionPos::ZERO, far]).unwrap();
    settle(&mut field, &terrain, &registry);
    let far_light = field
        .publication(1, far, &terrain, &registry)
        .unwrap()
        .clone();
    terrain
        .insert(
            SectionPos::ZERO,
            Section::new(4, emitter(&registry)).unwrap(),
            &registry,
        )
        .unwrap();
    field.inputs_changed(&terrain, &registry);
    assert_eq!(field.diagnostics().pending, 1);
    settle(&mut field, &terrain, &registry);
    assert!(Arc::ptr_eq(
        &far_light,
        field.publication(1, far, &terrain, &registry).unwrap()
    ));
    assert_eq!(field.diagnostics().started, 3);
    assert_eq!(field.diagnostics().incremental_started, 1);
}

#[test]
fn cache_denial_parks_work_until_explicit_retry_instead_of_recomputing_each_poll() {
    let (terrain, registry, _) = setup();
    let mut field = SharedLightField::new(
        terrain.binding(),
        LightFieldLimits {
            sections: 1,
            cache_bytes: 1,
            consumers: 2,
            interests: 2,
            job: job_limits(),
        },
    )
    .unwrap();
    field.subscribe(1, &[SectionPos::ZERO]).unwrap();
    let mut denied = false;
    for _ in 0..1000 {
        if let Err(error) = field.poll(&terrain, &registry, 50_000) {
            assert_eq!(error.code, torse_types::FaultCode::ResourceLimit);
            denied = true;
            break;
        }
    }
    assert!(denied);
    let before = field.diagnostics();
    for _ in 0..20 {
        field.poll(&terrain, &registry, 50_000).unwrap();
    }
    assert_eq!(field.diagnostics().started, before.started);
    assert_eq!(field.diagnostics().budget_denials, 1);
    assert!(
        field
            .publication(1, SectionPos::ZERO, &terrain, &registry)
            .is_none()
    );
    field.retry();
    field.poll(&terrain, &registry, 1).unwrap();
    assert_eq!(field.diagnostics().started, before.started + 1);
}

#[test]
#[ignore = "release mixed terrain edit measurement"]
fn shared_light_edit_measurement() {
    use std::time::Instant;
    use torse_types::LocalPos;
    let (reference, registry, _) = setup();
    let binding = reference.binding();
    let mut terrain = LightTerrain::new(
        binding,
        16,
        WorldBounds::new(0, 32).unwrap(),
        &registry,
        32,
        4 * 1024 * 1024,
    )
    .unwrap();
    let stone = registry
        .default_state(&ContentKey::parse("base:stone").unwrap())
        .unwrap();
    let mut target = Section::empty(16).unwrap();
    for x in -1..=1 {
        for z in -1..=1 {
            for y in 0..2 {
                let mut section = Section::empty(16).unwrap();
                for cy in 0..16 {
                    for cz in 0..16 {
                        for cx in 0..16 {
                            if y == 0 && cy < 6 && (cx + 3 * cz + cy) % 7 < 4 {
                                section
                                    .set(
                                        LocalPos {
                                            x: cx,
                                            y: cy,
                                            z: cz,
                                        },
                                        stone,
                                    )
                                    .unwrap();
                            }
                        }
                    }
                }
                if x == 0 && z == 0 && y == 0 {
                    target = section.clone();
                }
                terrain
                    .insert(SectionPos::new(x, y, z), section, &registry)
                    .unwrap();
            }
        }
    }
    let job = LightLimits {
        cells: 200_000,
        dependencies: 64,
        frontier: 200_000,
        bytes: 5_000_000,
    };
    let mut field = SharedLightField::new(
        binding,
        LightFieldLimits {
            sections: 1,
            cache_bytes: 16 * 1024 * 1024,
            consumers: 2,
            interests: 2,
            job,
        },
    )
    .unwrap();
    field.subscribe(1, &[SectionPos::ZERO]).unwrap();
    field.subscribe(2, &[SectionPos::ZERO]).unwrap();
    let cold = Instant::now();
    settle(&mut field, &terrain, &registry);
    let cold_ms = cold.elapsed().as_secs_f64() * 1000.;
    println!("initial_cold_ms={cold_ms:.6}");
    println!("sample,edit,incremental_ms,cold_ms,frontier_visits,pending_peak,cache_peak");
    for sample in 0..12 {
        let (position, state, edit) = match sample % 4 {
            0 => (
                LocalPos { x: 8, y: 10, z: 8 },
                emitter(&registry),
                "source_add",
            ),
            1 => (
                LocalPos { x: 8, y: 10, z: 8 },
                BlockStateId::EMPTY,
                "source_remove",
            ),
            2 => (LocalPos { x: 8, y: 15, z: 8 }, stone, "roof_close"),
            _ => (
                LocalPos { x: 8, y: 15, z: 8 },
                BlockStateId::EMPTY,
                "roof_open",
            ),
        };
        target.set(position, state).unwrap();
        terrain
            .insert(SectionPos::ZERO, target.clone(), &registry)
            .unwrap();
        let previous = field.diagnostics().update_frontier_visits;
        let start = Instant::now();
        field.inputs_changed(&terrain, &registry);
        settle(&mut field, &terrain, &registry);
        let warm_ms = start.elapsed().as_secs_f64() * 1000.;
        let start = Instant::now();
        let mut values = Vec::new();
        for sky in [false, true] {
            let mut cold = if sky {
                LightJob::prepare_sky(binding, &terrain, &registry, SectionPos::ZERO, job).unwrap()
            } else {
                LightJob::prepare(binding, &terrain, &registry, SectionPos::ZERO, job).unwrap()
            };
            while !cold.poll(binding, &terrain, &registry, 20_000).unwrap() {}
            values.push(
                cold.finish(binding, &terrain, &registry)
                    .unwrap()
                    .levels()
                    .to_vec(),
            );
        }
        let cold_ms = start.elapsed().as_secs_f64() * 1000.;
        let output = field
            .publication(1, SectionPos::ZERO, &terrain, &registry)
            .unwrap();
        assert_eq!(output.emitted.levels(), values[0]);
        assert_eq!(output.sky.levels(), values[1]);
        let d = field.diagnostics();
        println!(
            "{sample},{edit},{warm_ms:.6},{cold_ms:.6},{},{},{}",
            d.update_frontier_visits - previous,
            d.pending_peak,
            d.cache_peak
        );
    }
    assert_eq!(field.diagnostics().pending, 0);
    assert_eq!(field.diagnostics().published, 13);
    assert_eq!(field.diagnostics().incremental_started, 12);
}
