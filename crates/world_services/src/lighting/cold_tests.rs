use super::*;
use crate::lighting as lighting_api;
#[path = "../../tests/fixtures/lighting_column.rs"]
mod fixture;
use fixture::{ColumnSource, Optics, binding, limits};

#[test]
fn sparse_sky_frontier_matches_full_seed_heap_for_every_resolved_cell() {
    for height in [1, 16, 48] {
        for pattern in 0..=2 {
            let source = ColumnSource { height, pattern };
            let mut sparse =
                LightJob::prepare_sky(binding(), &source, &Optics, SectionPos::ZERO, limits())
                    .unwrap();
            let mut full =
                LightJob::prepare_sky(binding(), &source, &Optics, SectionPos::ZERO, limits())
                    .unwrap();
            let cells = sparse.levels.len();
            // No propagation occurs until all inputs have been seeded. Rebuild
            // the old frontier from every direct source as an independent oracle
            // for the new source-admission optimization.
            assert!(!sparse.poll(binding(), &source, &Optics, cells).unwrap());
            full.poll(binding(), &source, &Optics, cells).unwrap();
            full.queue.clear();
            full.frontier_seeded = cells;
            for index in 0..cells {
                if full.levels[index] > 0 {
                    full.push(full.levels[index], index).unwrap();
                }
            }
            for job in [&mut sparse, &mut full] {
                while !job.poll(binding(), &source, &Optics, 4096).unwrap() {}
            }
            assert_eq!(
                sparse.levels, full.levels,
                "height={height} pattern={pattern}"
            );
            assert_eq!(sparse.complete, full.complete);
            assert!(sparse.queue.capacity() <= sparse.frontier_limit);
            assert!(sparse.allocated_bytes() <= limits().bytes as usize);
        }
    }
}

#[test]
fn final_sky_layer_is_budgeted_and_cannot_be_captured_or_rebound_early() {
    let source = ColumnSource {
        height: 1,
        pattern: 0,
    };
    let mut job = LightJob::prepare_sky(binding(), &source, &Optics, SectionPos::ZERO, limits())
        .unwrap()
        .reusable(limits().bytes)
        .unwrap();
    let cells = job.levels.len();
    assert_eq!(job.queue.capacity(), 0);
    assert!(!job.poll(binding(), &source, &Optics, cells).unwrap());
    assert!(
        job.capture_at(SectionPos::ZERO, binding(), &source, &Optics)
            .is_err()
    );
    assert!(job.update(binding(), &source, &Optics).is_err());
    assert!(!job.poll(binding(), &source, &Optics, 0).unwrap());
    assert!(job.poll(binding(), &source, &Optics, cells).unwrap());
    assert_eq!(
        job.queue.capacity(),
        0,
        "uniform skylight needs no propagation frontier"
    );
    let output = job.finish(binding(), &source, &Optics).unwrap();
    assert!(output.levels()[..256].iter().all(|level| *level == 15));
    assert!(output.levels()[256..].iter().all(|level| *level == 0));
}

#[test]
fn retained_job_admission_accounts_for_future_frontier_growth() {
    let source = ColumnSource {
        height: 16,
        pattern: 0,
    };
    let job =
        LightJob::prepare_sky(binding(), &source, &Optics, SectionPos::ZERO, limits()).unwrap();
    let too_small = job.allocated_bytes() as u64 + (job.levels.len() * 4) as u64;
    assert!(job.reusable(too_small).is_err());
}
