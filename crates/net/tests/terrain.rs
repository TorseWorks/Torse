#[path = "support/replica.rs"]
#[allow(dead_code)]
mod fixture;
use fixture::seed;
use torse_net::terrain::*;
use torse_sim::replica::*;
use torse_types::*;
use torse_world::WorldBounds;

fn setup(bytes: u64) -> (TerrainMirror, TerrainUpdate<ReplicaSnapshot>) {
    let seed = seed(1);
    let mirror = TerrainMirror::new(
        seed.binding,
        16,
        seed.state.bounds,
        TerrainLimits {
            max_sections: 4,
            max_bytes: bytes,
            metadata_bytes: 4_u64 * 1024,
        },
        None,
    )
    .unwrap();
    let pos = seed.state.sections[0].pos;
    let update = TerrainUpdate {
        binding: seed.binding,
        generation: 1,
        seq: 1,
        through: seed.baseline,
        reset: Some(TerrainReset {
            sections: vec![pos],
            retained: vec![],
        }),
        pages: vec![TerrainPage {
            light: None,
            at: seed.baseline,
            digest: Digest([1; 32]),
            terrain: seed.state,
        }],
        unavailable: vec![],
    };
    (mirror, update)
}
#[test]
fn full_height_metadata_budget_is_independent_and_rejection_preserves_sequence() {
    let (base, mut update) = setup(1_000_000);
    let mut page = update.pages[0].clone();
    drop(base);
    let scope: Vec<_> = (-32..=32)
        .flat_map(|x| (-4..20).flat_map(move |y| (-32..=32).map(move |z| SectionPos::new(x, y, z))))
        .collect();
    let mut limits = TerrainLimits {
        max_sections: 131_072,
        max_bytes: 4096,
        metadata_bytes: 512,
    };
    let mut mirror = TerrainMirror::new(
        update.binding,
        16,
        update.pages[0].terrain.bounds,
        limits,
        None,
    )
    .unwrap();
    update.pages.clear();
    update.reset = Some(TerrainReset {
        sections: scope.clone(),
        retained: Vec::new(),
    });
    assert!(mirror.apply(update.clone()).is_err());
    assert!(mirror.desired().is_empty());
    assert_eq!(mirror.accounted_bytes(), 0);
    limits.metadata_bytes = 128 * 1024 * 1024;
    let mut mirror = TerrainMirror::new(
        update.binding,
        16,
        WorldBounds::new(-64, 320).unwrap(),
        limits,
        None,
    )
    .unwrap();
    mirror.apply(update.clone()).unwrap();
    assert_eq!(mirror.desired().len(), 101_400);
    assert_eq!(mirror.cached_bytes(), 0);
    assert_eq!(mirror.metadata_bytes(), 101_400 * 512);
    page.terrain.bounds = WorldBounds::new(-64, 320).unwrap();
    let mut batch = update.clone();
    batch.reset = None;
    batch.seq = 2;
    batch.pages = vec![page];
    assert_eq!(
        mirror.apply(batch.clone()).unwrap_err().code,
        FaultCode::ResourceLimit
    );
    assert_eq!(mirror.desired().len(), 101_400);
    assert_eq!(mirror.cached_pages(), 0);
    batch.pages.clear();
    mirror.apply(batch).unwrap();
    update.generation = 2;
    update.reset.as_mut().unwrap().sections.reverse();
    assert!(mirror.apply(update.clone()).is_err());
    assert_eq!(mirror.desired().len(), 101_400);
    update.reset = Some(TerrainReset {
        sections: vec![SectionPos::ZERO],
        retained: Vec::new(),
    });
    mirror.apply(update.clone()).unwrap();
    assert_eq!(mirror.desired().len(), 1);
    update.generation = 1;
    assert!(mirror.apply(update).is_err());
    assert_eq!(mirror.desired().len(), 1);
}
fn light(pos: SectionPos, digest: Digest, generation: u64) -> TerrainLight {
    TerrainLight {
        pos,
        terrain: digest,
        generation,
        rules: 1,
        optics: seed(1).binding.compatibility.rules_digest,
        complete: true,
        emitted: vec![3; 4096],
        sky: vec![15; 4096],
    }
}
#[test]
fn light_updates_match_content_and_reject_bad_or_disordered_messages_atomically() {
    let (mut mirror, mut first) = setup(1_000_000);
    let pos = first.pages[0].terrain.sections[0].pos;
    let digest = first.pages[0].digest;
    first.pages[0].light = Some(light(pos, digest, 1));
    let binding = first.binding;
    mirror.apply(first.clone()).unwrap();
    assert_eq!(mirror.light(pos).unwrap().generation, 1);
    let update = TerrainLightUpdate {
        binding,
        interest_generation: 1,
        seq: 1,
        pages: vec![light(pos, digest, 2)],
    };
    for kind in 0..6 {
        let mut invalid = update.clone();
        match kind {
            0 => invalid.seq = 2,
            1 => invalid.interest_generation = 2,
            2 => invalid.binding.epoch = WorldEpoch(20),
            3 => invalid.pages[0].emitted[0] = 16,
            4 => invalid.pages[0].rules = 2,
            _ => {
                invalid.pages[0].sky.pop();
            }
        }
        assert!(mirror.apply_light(invalid).is_err());
        assert_eq!(mirror.light(pos).unwrap().generation, 1);
    }
    mirror.apply_light(update.clone()).unwrap();
    assert_eq!(mirror.light(pos).unwrap().generation, 2);
    assert!(mirror.apply_light(update.clone()).is_err());
    let mut mismatch = update.clone();
    mismatch.seq = 2;
    mismatch.pages[0].terrain = Digest([9; 32]);
    mismatch.pages[0].generation = 3;
    assert!(mirror.apply_light(mismatch).unwrap().changed.is_empty());
    assert_eq!(mirror.light(pos).unwrap().generation, 2);
    let mut old = update;
    old.seq = 3;
    old.pages[0].generation = 1;
    assert!(mirror.apply_light(old).unwrap().changed.is_empty());
    assert_eq!(mirror.light(pos).unwrap().generation, 2);
    let mut changed = first;
    changed.reset = None;
    changed.seq = 2;
    changed.pages[0].digest = Digest([4; 32]);
    changed.pages[0].light = None;
    mirror.apply(changed).unwrap();
    assert!(mirror.light(pos).is_none());
}

#[test]
fn light_from_an_unexpected_optics_table_is_discarded_on_both_routes() {
    let (mut mirror, mut first) = setup(1_000_000);
    let expected = Digest([4; 32]);
    mirror.set_light_optics(expected).unwrap();
    let pos = first.pages[0].terrain.sections[0].pos;
    let mut page = light(pos, first.pages[0].digest, 1);
    first.pages[0].light = Some(page.clone());
    let binding = first.binding;
    mirror.apply(first).unwrap();
    assert!(mirror.page(pos).is_some());
    assert!(mirror.light(pos).is_none());
    assert!(mirror.set_light_optics(page.optics).is_err());

    let mismatched = TerrainLightUpdate {
        binding,
        interest_generation: 1,
        seq: 1,
        pages: vec![page.clone()],
    };
    assert!(mirror.apply_light(mismatched).unwrap().changed.is_empty());
    assert!(mirror.light(pos).is_none());
    page.optics = expected;
    assert_eq!(
        mirror
            .apply_light(TerrainLightUpdate {
                binding,
                interest_generation: 1,
                seq: 2,
                pages: vec![page.clone()],
            })
            .unwrap()
            .changed,
        vec![pos]
    );
    assert_eq!(mirror.light(pos), Some(&page));
}

#[test]
fn invalid_inline_light_and_over_budget_light_preserve_the_terrain_baseline() {
    let (mut mirror, mut first) = setup(9000);
    let pos = first.pages[0].terrain.sections[0].pos;
    let digest = first.pages[0].digest;
    first.pages[0].light = Some(light(pos, Digest([5; 32]), 1));
    assert!(mirror.apply(first.clone()).is_err());
    assert!(mirror.page(pos).is_none());
    first.pages[0].light = None;
    // Dense fixture itself needs more than this small budget.
    assert!(mirror.apply(first).is_err());
    assert!(mirror.page(pos).is_none());
    let (mut mirror, first) = setup(18000);
    let binding = first.binding;
    mirror.apply(first).unwrap();
    let output = mirror.apply_light(TerrainLightUpdate {
        binding,
        interest_generation: 1,
        seq: 1,
        pages: vec![light(pos, digest, 1)],
    });
    assert!(output.is_err());
    assert!(mirror.light(pos).is_none());
}
#[test]
fn ordered_resets_retain_buffers_and_reject_invalid_updates_atomically() {
    let (mut mirror, first) = setup(1_000_000);
    let pos = first.pages[0].terrain.sections[0].pos;
    mirror.apply(first.clone()).unwrap();
    let before = mirror.page(pos).unwrap().clone();
    let address = match &mirror.page(pos).unwrap().sections[0].cells {
        torse_world::SnapshotCells::Dense(v) => v.as_ptr(),
        _ => unreachable!(),
    };
    for kind in 0..8 {
        let mut bad = first.clone();
        bad.reset = None;
        bad.seq = 2;
        match kind {
            0 => bad.seq = 4,
            1 => bad.generation = 2,
            2 => bad.binding.epoch = WorldEpoch(9),
            3 => bad.pages[0].terrain.sections[0].pos.x = 9,
            4 => bad.pages[0].terrain.side = 32,
            5 => bad.pages[0].at.seq = CommitSeq(9),
            6 => bad.unavailable.push((pos, TerrainUnavailable::Missing)),
            _ => bad.pages[0].terrain.states.clear(),
        }
        assert!(mirror.apply(bad).is_err());
        assert_eq!(mirror.page(pos), Some(&before));
    }
    let mut retained = first;
    retained.generation = 2;
    retained.pages.clear();
    retained
        .reset
        .as_mut()
        .unwrap()
        .retained
        .push((pos, Digest([1; 32])));
    for kind in 0..4 {
        let mut bad = retained.clone();
        let reset = bad.reset.as_mut().unwrap();
        match kind {
            0 => reset.retained[0].1 = Digest([2; 32]),
            1 => reset.retained.push(reset.retained[0]),
            2 => reset.sections[0].x += 1,
            _ => bad.unavailable.push((pos, TerrainUnavailable::Missing)),
        }
        assert!(mirror.apply(bad).is_err());
        assert_eq!(mirror.page(pos), Some(&before));
    }
    mirror.apply(retained.clone()).unwrap();
    assert!(mirror.apply(retained).is_err());
    match &mirror.page(pos).unwrap().sections[0].cells {
        torse_world::SnapshotCells::Dense(v) => assert_eq!(v.as_ptr(), address),
        _ => unreachable!(),
    }
}

#[test]
#[ignore = "fixed workload for release timing"]
fn overlapping_full_height_interest_reset_measurement() {
    use std::time::Instant;
    use torse_world::{BlockStateId, SnapshotCells};

    let (_, mut initial) = setup(1_000_000);
    let bounds = WorldBounds::new(-64, 320).unwrap();
    let column_window = |left| {
        (left..left + 33)
            .flat_map(|x| {
                (-4..20).flat_map(move |y| (-16..17).map(move |z| SectionPos::new(x, y, z)))
            })
            .collect::<Vec<_>>()
    };
    let original = column_window(-16);
    let shifted = column_window(-15);
    let retained = &original[24 * 33..];
    let digest = Digest([1; 32]);
    let mut page = initial.pages.pop().unwrap();
    page.terrain.bounds = bounds;
    page.terrain.states.clear();
    page.terrain.sections[0].cells = SnapshotCells::Uniform(BlockStateId::EMPTY);
    initial.pages = original
        .iter()
        .map(|&pos| {
            let mut page = page.clone();
            page.terrain.sections[0].pos = pos;
            page
        })
        .collect();
    initial
        .reset
        .as_mut()
        .unwrap()
        .sections
        .clone_from(&original);
    let mut mirror = TerrainMirror::new(
        initial.binding,
        16,
        bounds,
        TerrainLimits {
            max_sections: original.len(),
            max_bytes: 256 * 1024 * 1024,
            metadata_bytes: 64 * 1024 * 1024,
        },
        None,
    )
    .unwrap();
    let mut reset = TerrainUpdate {
        binding: initial.binding,
        through: initial.through,
        generation: 2,
        seq: 1,
        reset: Some(TerrainReset {
            sections: shifted.clone(),
            retained: retained.iter().map(|&pos| (pos, digest)).collect(),
        }),
        pages: Vec::new(),
        unavailable: Vec::new(),
    };
    mirror.apply(initial).unwrap();
    let mut samples = Vec::new();
    for generation in 2..34 {
        reset.generation = generation;
        let scope = if generation % 2 == 0 {
            &shifted
        } else {
            &original
        };
        reset.reset.as_mut().unwrap().sections.clone_from(scope);
        let message = reset.clone();
        let started = Instant::now();
        let changes = mirror.apply(message).unwrap();
        samples.push(started.elapsed().as_micros());
        assert!(changes.changed.is_empty());
        assert_eq!(changes.removed.len(), if generation == 2 { 792 } else { 0 });
        assert_eq!(mirror.cached_pages(), retained.len());
        assert_eq!(mirror.desired().len(), scope.len());
    }
    let first = samples.remove(0);
    samples.sort_unstable();
    println!(
        "retention first_us={first} median_us={} max_us={}",
        samples[samples.len() / 2],
        samples.last().unwrap()
    );
}
#[test]
fn near_edits_hide_old_pages_until_a_fresh_page_or_retained_proof() {
    let (mut mirror, mut update) = setup(1_000_000);
    let pos = update.pages[0].terrain.sections[0].pos;
    mirror.apply(update.clone()).unwrap();
    let mut changes = fixture::changes(&torse_net::ReplicaMirror::from_seed(seed(1)).unwrap());
    changes.removed_sections.push(pos);
    mirror.observe_authority(
        &changes,
        CommitCursor {
            world: update.binding.world,
            seq: CommitSeq(2),
        },
    );
    assert!(mirror.page(pos).is_none());
    update.reset = None;
    update.seq = 2;
    mirror.apply(update.clone()).unwrap();
    assert!(mirror.page(pos).is_none());
    update.generation = 2;
    update.seq = 1;
    update.pages.clear();
    update.through.seq = CommitSeq(2);
    update.reset = Some(TerrainReset {
        sections: vec![pos],
        retained: vec![(pos, Digest([1; 32]))],
    });
    assert_eq!(mirror.apply(update).unwrap().changed, vec![pos]);
    assert!(mirror.page(pos).is_some());
}
#[test]
fn cache_denial_is_atomic_and_pool_reservation_releases_on_drop() {
    let (mut mirror, update) = setup(4096);
    assert!(mirror.apply(update.clone()).is_err());
    assert!(mirror.desired().is_empty());
    assert_eq!(mirror.accounted_bytes(), 0);
    let pool = MemoryPool::new(MemoryBudget::new(1_000_000));
    let held = TerrainMirror::new(
        update.binding,
        16,
        update.pages[0].terrain.bounds,
        TerrainLimits {
            max_sections: 4,
            max_bytes: 1_000_000 - 4 * 1024,
            metadata_bytes: 4_u64 * 1024,
        },
        Some(pool.clone()),
    )
    .unwrap();
    assert_eq!(pool.usage().total, 1_000_000);
    drop(held);
    assert_eq!(pool.usage().total, 0);
}
