#[path = "support/replica.rs"]
#[allow(dead_code)]
mod fixture;
use fixture::seed;
use torse_net::terrain::*;
use torse_sim::replica::*;
use torse_types::*;

fn setup(bytes: u64) -> (TerrainMirror, TerrainUpdate<ReplicaSnapshot>) {
    let seed = seed(1);
    let mirror = TerrainMirror::new(
        seed.binding,
        16,
        seed.state.bounds,
        TerrainLimits {
            max_sections: 4,
            max_bytes: bytes,
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
fn light(pos: SectionPos, digest: Digest, generation: u64) -> TerrainLight {
    TerrainLight {
        pos,
        terrain: digest,
        generation,
        rules: 1,
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
    mirror.apply(retained.clone()).unwrap();
    assert!(mirror.apply(retained).is_err());
    match &mirror.page(pos).unwrap().sections[0].cells {
        torse_world::SnapshotCells::Dense(v) => assert_eq!(v.as_ptr(), address),
        _ => unreachable!(),
    }
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
            max_bytes: 1_000_000,
        },
        Some(pool.clone()),
    )
    .unwrap();
    assert_eq!(pool.usage().total, 1_000_000);
    drop(held);
    assert_eq!(pool.usage().total, 0);
}
