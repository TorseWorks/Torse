use torse_content::{compile, m0_fixture_pack};
use torse_types::{
    Binding, BlockPos, Compatibility, ContentKey, Digest, EvictionRank, SectionPos, WorldEpoch,
    WorldId,
};
use torse_world::{BlockStateId, BlockStateRegistry, Section, SectionStore, WorldBounds};
use torse_world_services::lighting::{LightCache, LightJob, LightLimits, LightQuery, LightSection};

fn registry(emission: f32) -> BlockStateRegistry {
    let mut pack = m0_fixture_pack();
    for block in &mut pack.blocks {
        block.value.light_transport.emission = if block.value.id.to_string() == "base:glass" {
            emission
        } else {
            0.0
        };
        block.value.light_transport.absorption = match block.value.id.to_string().as_str() {
            "base:glass" => 0.0,
            "base:glass_slab" => 0.5,
            _ => 1.0,
        };
    }
    BlockStateRegistry::build(&compile(pack).unwrap()).unwrap()
}
fn binding() -> Binding {
    Binding {
        world: WorldId(1),
        epoch: WorldEpoch(1),
        compatibility: Compatibility {
            authority_schema: 1,
            rules_digest: Digest([1; 32]),
            content_digest: registry(1.0).authority_digest(),
        },
    }
}
fn state(registry: &BlockStateRegistry, name: &str) -> BlockStateId {
    registry
        .default_state(&ContentKey::parse(name).unwrap())
        .unwrap()
}
fn load(store: &mut SectionStore, pos: SectionPos, fill: BlockStateId) {
    store.request(pos).unwrap();
    store.install(pos, Section::new(16, fill).unwrap()).unwrap();
    store.finish_load(pos).unwrap();
}
fn store(fill: BlockStateId) -> SectionStore {
    let mut store = SectionStore::new(16, WorldBounds::new(0, 16).unwrap()).unwrap();
    for z in -2..=2 {
        for x in -2..=2 {
            load(&mut store, SectionPos::new(x, 0, z), fill);
        }
    }
    store
}
fn limits() -> LightLimits {
    LightLimits {
        cells: 200_000,
        dependencies: 64,
        frontier: 200_000,
        bytes: 4_000_000,
    }
}
fn calculate(
    store: &SectionStore,
    registry: &BlockStateRegistry,
    target: SectionPos,
    work: usize,
) -> LightSection {
    let mut job = LightJob::prepare(binding(), store, registry, target, limits()).unwrap();
    for _ in 0..100_000 {
        if job.poll(binding(), store, registry, work).unwrap() {
            return job.finish(binding(), store, registry).unwrap();
        }
    }
    panic!("lighting failed to finish");
}
fn level(
    cache: &LightCache,
    store: &SectionStore,
    registry: &BlockStateRegistry,
    x: i64,
    y: i64,
    z: i64,
) -> u8 {
    match cache.sample(binding(), store, registry, BlockPos::new(x, y, z)) {
        LightQuery::Ready {
            level,
            complete: true,
        } => level,
        other => panic!("unexpected light query: {other:?}"),
    }
}
const TARGET: SectionPos = SectionPos::new(0, 0, 0);

#[test]
fn state_optics_switch_off_and_reload_invalidate_native_light() {
    use torse_content::LightTransport;
    let mut pack = m0_fixture_pack();
    let block = &mut pack
        .blocks
        .iter_mut()
        .find(|b| !b.value.state_overrides.is_empty())
        .unwrap()
        .value;
    let key = block.id.clone();
    block.light_transport = LightTransport {
        absorption: 0.0,
        emission: 1.0,
    };
    let off_values = block.state_overrides[0].values.clone();
    let on_values = block.state_overrides[1].values.clone();
    block.state_overrides[0].light_transport = Some(LightTransport {
        absorption: 0.0,
        emission: 0.0,
    });
    let registry = BlockStateRegistry::build(&compile(pack).unwrap()).unwrap();
    let off = registry.state_of(&key, &off_values).unwrap();
    let on = registry.state_of(&key, &on_values).unwrap();
    let mut store = store(BlockStateId::EMPTY);
    let pos = BlockPos::new(8, 8, 8);
    let mut cache = LightCache::new(binding(), 1, 1_000_000).unwrap();
    store.edit(pos, on).unwrap();
    cache
        .publish(
            calculate(&store, &registry, TARGET, 4096),
            &store,
            &registry,
        )
        .unwrap();
    assert_eq!(level(&cache, &store, &registry, 8, 8, 8), 15);
    assert_eq!(level(&cache, &store, &registry, 9, 8, 8), 14);
    store.edit(pos, off).unwrap();
    assert_eq!(
        cache.sample(binding(), &store, &registry, pos),
        LightQuery::Pending
    );
    cache
        .publish(
            calculate(&store, &registry, TARGET, 4096),
            &store,
            &registry,
        )
        .unwrap();
    assert_eq!(level(&cache, &store, &registry, 9, 8, 8), 0);
    store.unload(TARGET).unwrap();
    assert_eq!(
        cache.sample(binding(), &store, &registry, pos),
        LightQuery::Pending
    );
    load(&mut store, TARGET, BlockStateId::EMPTY);
    store.edit(pos, on).unwrap();
    assert_eq!(
        cache.sample(binding(), &store, &registry, pos),
        LightQuery::Pending
    );
    cache
        .publish(
            calculate(&store, &registry, TARGET, 4096),
            &store,
            &registry,
        )
        .unwrap();
    assert_eq!(level(&cache, &store, &registry, 9, 8, 8), 14);
}

fn sky(store: &SectionStore, registry: &BlockStateRegistry) -> LightSection {
    let mut job = LightJob::prepare_sky(binding(), store, registry, TARGET, limits()).unwrap();
    for _ in 0..1000 {
        if job.poll(binding(), store, registry, 4096).unwrap() {
            return job.finish(binding(), store, registry).unwrap();
        }
    }
    panic!("skylight failed to finish");
}

#[test]
fn skylight_roof_opening_and_upper_section_changes_are_observed() {
    let registry = registry(1.0);
    let mut store = SectionStore::new(16, WorldBounds::new(0, 32).unwrap()).unwrap();
    for z in -2..=2 {
        for x in -2..=2 {
            for y in 0..2 {
                load(&mut store, SectionPos::new(x, y, z), BlockStateId::EMPTY);
            }
        }
    }
    let stone = state(&registry, "base:stone");
    for z in -16..32 {
        for x in -16..32 {
            store.edit(BlockPos::new(x, 24, z), stone).unwrap();
        }
    }
    let mut cache = LightCache::new(binding(), 1, 1_000_000).unwrap();
    let stamp = cache
        .publish(sky(&store, &registry), &store, &registry)
        .unwrap();
    assert_eq!(level(&cache, &store, &registry, 8, 8, 8), 0);
    store
        .edit(BlockPos::new(8, 24, 8), BlockStateId::EMPTY)
        .unwrap();
    assert!(!cache.validate_stamp(stamp, binding(), &store, &registry));
    cache
        .publish(sky(&store, &registry), &store, &registry)
        .unwrap();
    assert_eq!(level(&cache, &store, &registry, 8, 8, 8), 15);
    assert_eq!(level(&cache, &store, &registry, 9, 8, 8), 14);
}

#[test]
fn missing_sky_columns_are_dark_incomplete_and_extreme_height_is_bounded() {
    let registry = registry(1.0);
    let mut store = SectionStore::new(16, WorldBounds::new(0, 32).unwrap()).unwrap();
    load(&mut store, TARGET, BlockStateId::EMPTY);
    let mut cache = LightCache::new(binding(), 1, 1_000_000).unwrap();
    cache
        .publish(sky(&store, &registry), &store, &registry)
        .unwrap();
    assert_eq!(
        cache.sample(binding(), &store, &registry, BlockPos::new(8, 8, 8)),
        LightQuery::Ready {
            level: 0,
            complete: false
        }
    );
    let mut huge = SectionStore::new(16, WorldBounds::new(i64::MIN, i64::MAX).unwrap()).unwrap();
    load(&mut huge, TARGET, BlockStateId::EMPTY);
    assert!(LightJob::prepare_sky(binding(), &huge, &registry, TARGET, limits()).is_err());
}

#[test]
fn emitted_light_crosses_positive_and_negative_section_boundaries() {
    let registry = registry(1.0);
    let mut store = store(BlockStateId::EMPTY);
    store
        .edit(BlockPos::new(15, 8, 8), state(&registry, "base:glass"))
        .unwrap();
    let before = store.revision(TARGET);
    let mut cache = LightCache::new(binding(), 4, 1_000_000).unwrap();
    for target in [TARGET, SectionPos::new(1, 0, 0)] {
        cache
            .publish(calculate(&store, &registry, target, 137), &store, &registry)
            .unwrap();
    }
    assert_eq!(level(&cache, &store, &registry, 15, 8, 8), 15);
    assert_eq!(level(&cache, &store, &registry, 16, 8, 8), 14);
    assert_eq!(level(&cache, &store, &registry, 29, 8, 8), 1);
    assert_eq!(level(&cache, &store, &registry, 30, 8, 8), 0);
    assert_eq!(store.revision(TARGET), before);
    store
        .edit(BlockPos::new(15, 8, 8), BlockStateId::EMPTY)
        .unwrap();
    store
        .edit(BlockPos::new(-1, 8, 8), state(&registry, "base:glass"))
        .unwrap();
    cache
        .publish(
            calculate(&store, &registry, TARGET, 100_000),
            &store,
            &registry,
        )
        .unwrap();
    assert_eq!(level(&cache, &store, &registry, 0, 8, 8), 14);
    assert_eq!(level(&cache, &store, &registry, 13, 8, 8), 1);
    assert_eq!(level(&cache, &store, &registry, 14, 8, 8), 0);
}

#[test]
fn absorption_is_independent_of_visual_shape_and_blocks_detours() {
    let registry = registry(1.0);
    let mut store = store(state(&registry, "base:stone"));
    for x in 0..16 {
        store
            .edit(BlockPos::new(x, 8, 8), BlockStateId::EMPTY)
            .unwrap();
    }
    store
        .edit(BlockPos::new(0, 8, 8), state(&registry, "base:glass"))
        .unwrap();
    store
        .edit(BlockPos::new(1, 8, 8), state(&registry, "base:glass_slab"))
        .unwrap();
    let mut cache = LightCache::new(binding(), 1, 1_000_000).unwrap();
    cache
        .publish(calculate(&store, &registry, TARGET, 79), &store, &registry)
        .unwrap();
    for (x, expected) in [(0, 15), (1, 7), (2, 6), (7, 1), (8, 0)] {
        assert_eq!(level(&cache, &store, &registry, x, 8, 8), expected);
    }
    assert_eq!(level(&cache, &store, &registry, 0, 9, 8), 0);
}

#[test]
fn edits_and_unload_invalidate_jobs_results_and_consumer_stamps() {
    let registry = registry(1.0);
    let mut store = store(BlockStateId::EMPTY);
    let lamp = BlockPos::new(15, 8, 8);
    store.edit(lamp, state(&registry, "base:glass")).unwrap();
    let mut cache = LightCache::new(binding(), 1, 1_000_000).unwrap();
    let stamp = cache
        .publish(
            calculate(&store, &registry, TARGET, 100_000),
            &store,
            &registry,
        )
        .unwrap();
    let pending = calculate(&store, &registry, TARGET, 100_000);
    let mut job = LightJob::prepare(binding(), &store, &registry, TARGET, limits()).unwrap();
    assert!(!job.poll(binding(), &store, &registry, 1).unwrap());
    store.edit(lamp, BlockStateId::EMPTY).unwrap();
    assert!(job.poll(binding(), &store, &registry, 100_000).is_err());
    assert!(job.finish(binding(), &store, &registry).is_err());
    assert!(cache.publish(pending, &store, &registry).is_err());
    assert!(!cache.validate_stamp(stamp, binding(), &store, &registry));
    assert_eq!(
        cache.sample(binding(), &store, &registry, lamp),
        LightQuery::Pending
    );
    cache
        .publish(
            calculate(&store, &registry, TARGET, 100_000),
            &store,
            &registry,
        )
        .unwrap();
    assert_eq!(level(&cache, &store, &registry, 15, 8, 8), 0);
    store.unload(SectionPos::new(1, 0, 0)).unwrap();
    assert_eq!(
        cache.sample(binding(), &store, &registry, lamp),
        LightQuery::Pending
    );
    let section = calculate(&store, &registry, TARGET, 100_000);
    assert!(!section.is_complete());
    cache.publish(section, &store, &registry).unwrap();
    assert_eq!(
        cache.sample(binding(), &store, &registry, lamp),
        LightQuery::Ready {
            level: 0,
            complete: false
        }
    );
    load(&mut store, SectionPos::new(1, 0, 0), BlockStateId::EMPTY);
    assert_eq!(
        cache.sample(binding(), &store, &registry, lamp),
        LightQuery::Pending
    );
}

#[test]
fn loading_to_ready_is_a_dependency_even_without_revision_change() {
    let registry = registry(1.0);
    let mut store = store(BlockStateId::EMPTY);
    let neighbor = SectionPos::new(1, 0, 0);
    store.unload(neighbor).unwrap();
    store.request(neighbor).unwrap();
    store
        .install(neighbor, Section::empty(16).unwrap())
        .unwrap();
    assert!(LightJob::prepare(binding(), &store, &registry, neighbor, limits()).is_err());
    let before = store.revision(neighbor);
    let mut cache = LightCache::new(binding(), 1, 1_000_000).unwrap();
    cache
        .publish(
            calculate(&store, &registry, TARGET, 100_000),
            &store,
            &registry,
        )
        .unwrap();
    store.finish_load(neighbor).unwrap();
    assert_eq!(store.revision(neighbor), before);
    assert_eq!(
        cache.sample(binding(), &store, &registry, BlockPos::new(0, 8, 8)),
        LightQuery::Pending
    );
}

#[test]
fn budgets_reject_work_and_preserve_the_last_published_entry() {
    let registry = registry(1.0);
    let mut store = store(BlockStateId::EMPTY);
    for restricted in [
        LightLimits {
            cells: 1,
            ..limits()
        },
        LightLimits {
            dependencies: 1,
            ..limits()
        },
        LightLimits {
            bytes: 1,
            ..limits()
        },
    ] {
        assert!(LightJob::prepare(binding(), &store, &registry, TARGET, restricted).is_err());
    }
    store
        .edit(BlockPos::new(1, 1, 1), state(&registry, "base:glass"))
        .unwrap();
    store
        .edit(BlockPos::new(2, 1, 1), state(&registry, "base:glass"))
        .unwrap();
    let mut job = LightJob::prepare(
        binding(),
        &store,
        &registry,
        TARGET,
        LightLimits {
            frontier: 1,
            ..limits()
        },
    )
    .unwrap();
    assert!(job.poll(binding(), &store, &registry, 100_000).is_err());
    assert!(job.finish(binding(), &store, &registry).is_err());
    let mut cache = LightCache::new(binding(), 1, 1_000_000).unwrap();
    assert!(cache.memory_resident(false).is_none());
    let stamp = cache
        .publish(
            calculate(&store, &registry, TARGET, 100_000),
            &store,
            &registry,
        )
        .unwrap();
    let bytes = cache.allocated_bytes();
    assert!(
        cache
            .publish(
                calculate(&store, &registry, SectionPos::new(1, 0, 0), 100_000),
                &store,
                &registry
            )
            .is_err()
    );
    assert_eq!(cache.allocated_bytes(), bytes);
    assert!(cache.validate_stamp(stamp, binding(), &store, &registry));
    assert_eq!(
        cache.memory_resident(true).unwrap().rank,
        EvictionRank::Pinned
    );
    assert_eq!(
        cache.memory_resident(false).unwrap().rank,
        EvictionRank::UnsubscribedDerived
    );
    let mut tiny = LightCache::new(binding(), 1, 1).unwrap();
    assert!(
        tiny.publish(
            calculate(&store, &registry, TARGET, 100_000),
            &store,
            &registry
        )
        .is_err()
    );
    assert_eq!(tiny.allocated_bytes(), 0);
    assert!(cache.remove(TARGET));
    assert_eq!(cache.allocated_bytes(), 0);
    assert!(!cache.validate_stamp(stamp, binding(), &store, &registry));
}

#[test]
fn content_epoch_and_cache_incarnation_reject_stale_consumers() {
    let registry = registry(1.0);
    let store = store(BlockStateId::EMPTY);
    let mut cache = LightCache::new(binding(), 1, 1_000_000).unwrap();
    let stamp = cache
        .publish(
            calculate(&store, &registry, TARGET, 100_000),
            &store,
            &registry,
        )
        .unwrap();
    let changed = self::registry(0.5);
    assert!(!cache.validate_stamp(stamp, binding(), &store, &changed));
    assert!(!cache.validate_stamp(
        stamp,
        Binding {
            epoch: WorldEpoch(2),
            ..binding()
        },
        &store,
        &registry
    ));
    let mut other = LightCache::new(binding(), 1, 1_000_000).unwrap();
    other
        .publish(
            calculate(&store, &registry, TARGET, 100_000),
            &store,
            &registry,
        )
        .unwrap();
    assert!(!other.validate_stamp(stamp, binding(), &store, &registry));
    assert_eq!(
        cache.sample(binding(), &store, &registry, BlockPos::new(0, -1, 0)),
        LightQuery::OutOfBounds
    );
}

#[test]
fn propagation_matches_independent_relaxation_and_poll_partitioning() {
    let registry = registry(1.0);
    let stone = state(&registry, "base:stone");
    let lamp = state(&registry, "base:glass");
    let filter = state(&registry, "base:glass_slab");
    let mut store = store(stone);
    let mut absorption = vec![1u8; 4096];
    let mut expected = vec![0u8; 4096];
    let mut random = 12345u32;
    for i in 0..4096 {
        random = random.wrapping_mul(1664525).wrapping_add(1013904223);
        let value = match random >> 27 {
            0 => lamp,
            1..=5 => stone,
            6..=10 => filter,
            _ => BlockStateId::EMPTY,
        };
        absorption[i] = if value == stone {
            15
        } else if value == filter {
            8
        } else {
            1
        };
        expected[i] = if value == lamp { 15 } else { 0 };
        store
            .edit(
                BlockPos::new((i % 16) as i64, (i / 256) as i64, ((i / 16) % 16) as i64),
                value,
            )
            .unwrap();
    }
    for _ in 0..15 {
        let previous = expected.clone();
        for i in 0usize..4096 {
            let x = i % 16;
            let z = i / 16 % 16;
            let y = i / 256;
            for (valid, j) in [
                (x > 0, i.wrapping_sub(1)),
                (x < 15, i + 1),
                (z > 0, i.wrapping_sub(16)),
                (z < 15, i + 16),
                (y > 0, i.wrapping_sub(256)),
                (y < 15, i + 256),
            ] {
                if valid {
                    expected[i] = expected[i].max(previous[j].saturating_sub(absorption[i]));
                }
            }
        }
    }
    let mut small = LightCache::new(binding(), 1, 1_000_000).unwrap();
    let mut large = LightCache::new(binding(), 1, 1_000_000).unwrap();
    small
        .publish(calculate(&store, &registry, TARGET, 103), &store, &registry)
        .unwrap();
    large
        .publish(
            calculate(&store, &registry, TARGET, 100_000),
            &store,
            &registry,
        )
        .unwrap();
    for (i, expected) in expected.into_iter().enumerate() {
        let (x, y, z) = ((i % 16) as i64, (i / 256) as i64, ((i / 16) % 16) as i64);
        assert_eq!(
            level(&small, &store, &registry, x, y, z),
            expected,
            "cell {i}"
        );
        assert_eq!(
            level(&large, &store, &registry, x, y, z),
            expected,
            "cell {i}"
        );
    }
}

#[test]
fn unrepresentable_halo_is_rejected_without_coordinate_overflow() {
    let registry = registry(1.0);
    let mut store = store(BlockStateId::EMPTY);
    let pos = SectionPos::new(i64::MAX / 16, 0, 0);
    load(&mut store, pos, BlockStateId::EMPTY);
    assert!(LightJob::prepare(binding(), &store, &registry, pos, limits()).is_err());
}
