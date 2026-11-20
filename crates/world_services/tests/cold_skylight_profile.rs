//! Explicit cold-column measurement, independent of host/network delivery.
#[path = "fixtures/lighting_column.rs"]
mod fixture;
use fixture::{ColumnSource, Optics, binding, limits};
use std::time::Instant;
use torse_types::SectionPos;
use torse_world_services::lighting::{self as lighting_api, LightJob};

#[test]
#[ignore = "explicit Release full-height skylight comparison"]
fn cold_skylight_profile() {
    if cfg!(debug_assertions) {
        panic!("run this profile with --release");
    }
    println!("pattern,sample,elapsed_us,max_poll_us,polls,initial_bytes,peak_bytes,hash");
    for pattern in 0..=2 {
        let source = ColumnSource {
            height: 384,
            pattern,
        };
        for sample in 0..3 {
            let start = Instant::now();
            let mut job = LightJob::prepare_sky(
                binding(),
                &source,
                &Optics,
                SectionPos::new(0, 4, 0),
                limits(),
            )
            .unwrap();
            let initial = job.allocated_bytes();
            let mut peak = initial;
            let mut polls = 0;
            let mut max_poll = 0;
            loop {
                let tick = Instant::now();
                let complete = job.poll(binding(), &source, &Optics, 100_000).unwrap();
                max_poll = max_poll.max(tick.elapsed().as_micros());
                peak = peak.max(job.allocated_bytes());
                polls += 1;
                if complete {
                    break;
                }
            }
            let output = job.finish(binding(), &source, &Optics).unwrap();
            let hash = output.levels().iter().fold(0u64, |hash, value| {
                hash.wrapping_mul(31).wrapping_add(u64::from(*value))
            });
            println!(
                "{pattern},{sample},{},{max_poll},{polls},{initial},{peak},{hash:016x}",
                start.elapsed().as_micros()
            );
        }
    }
}
