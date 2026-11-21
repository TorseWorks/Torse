use std::hint::black_box;
use std::time::Instant;
use torse_types::{Revision, SectionPos};
use torse_world::{Section, SectionPhase, SectionStore, WorldBounds};

fn checked_revision(store: &SectionStore, pos: SectionPos) -> Option<Revision> {
    matches!(
        store.phase(pos),
        Some(SectionPhase::Ready | SectionPhase::Active)
    )
    .then(|| store.revision(pos))
    .flatten()
}

fn borrowed_revision(store: &SectionStore, pos: SectionPos) -> Option<Revision> {
    store.readable_section(pos).map(Section::revision)
}

#[test]
#[ignore = "paired release lookup measurement; run explicitly with --release --ignored --nocapture"]
fn readable_lookup_profile() {
    for count in [64, 600, 4096] {
        let mut store = SectionStore::new(16, WorldBounds::new(0, 256).unwrap()).unwrap();
        let positions: Vec<_> = (0..count).map(|x| SectionPos::new(x, 0, -x)).collect();
        for (i, &pos) in positions.iter().enumerate() {
            store.request(pos).unwrap();
            // Include unfinished sections as well as readable ones.
            if i % 8 != 0 {
                store.install(pos, Section::empty(16).unwrap()).unwrap();
                if i % 8 != 1 {
                    store.finish_load(pos).unwrap();
                }
            }
        }
        let probes: Vec<_> = positions
            .iter()
            .copied()
            .chain([SectionPos::new(-1, 0, 1)])
            .collect();
        for &pos in &probes {
            assert_eq!(
                checked_revision(&store, pos),
                borrowed_revision(&store, pos)
            );
        }
        let measure = |lookup: fn(&SectionStore, SectionPos) -> Option<Revision>| {
            let start = Instant::now();
            for _ in 0..1000 {
                for &pos in &probes {
                    black_box(lookup(black_box(&store), black_box(pos)));
                }
            }
            start.elapsed().as_nanos()
        };
        // Warm both paths, then alternate their measurement order.
        black_box(measure(checked_revision));
        black_box(measure(borrowed_revision));
        for pair in 0..6 {
            let (old, new) = if pair % 2 == 0 {
                (measure(checked_revision), measure(borrowed_revision))
            } else {
                let new = measure(borrowed_revision);
                (measure(checked_revision), new)
            };
            println!(
                "count={count} probes={} pair={pair} checked_ns={old} borrowed_ns={new}",
                probes.len() * 1000
            );
        }
    }
}
