//! Explicit clone-and-validate reference versus ordered in-place empty cuts.
#[allow(dead_code)]
#[path = "../tests/support/replica.rs"]
mod fixture;
use std::{error::Error, fs, io::Write, time::Instant};
use torse_net::ReplicaMirror;
use torse_sim::replica::WorldReplicaSeed;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    if !(2..=3).contains(&args.len()) || args.get(2).is_some_and(|s| s != "--in-place-first") {
        return Err("usage: mirror_baseline NEW_OUTPUT_DIRECTORY [--in-place-first]".into());
    }
    let output = std::path::Path::new(&args[1]);
    fs::create_dir(output)?;
    let mut csv = fs::File::create(output.join("samples.csv"))?;
    writeln!(
        csv,
        "mode,sections,iteration,apply_ns,retained_terrain_copy_bytes"
    )?;
    let modes = if args.len() == 3 {
        ["in_place", "clone_validate"]
    } else {
        ["clone_validate", "in_place"]
    };
    for sections in [64, 1024] {
        let expected = fixture::seed(sections).state;
        for mode in modes {
            let mut mirror = ReplicaMirror::from_seed(fixture::seed(sections))?;
            let buffers = fixture::pointers(&mirror);
            for iteration in 0..110 {
                let batch = fixture::batch(&mirror, fixture::changes(&mirror));
                let now = Instant::now();
                if mode == "in_place" {
                    mirror.apply_interest(batch)?;
                } else {
                    // Reproduce the former empty-cut mechanism explicitly:
                    // clone all terrain, replace the dictionary, validate all
                    // references, and swap only after successful validation.
                    let mut state = mirror.state().clone();
                    state.states = batch.changes.states;
                    let mut next = ReplicaMirror::from_seed(WorldReplicaSeed {
                        binding: batch.binding,
                        subscription: batch.subscription,
                        baseline: batch.through,
                        state,
                    })?;
                    next.seq = batch.seq;
                    mirror = next;
                }
                let elapsed = now.elapsed().as_nanos();
                assert_eq!(mirror.state(), &expected);
                if mode == "in_place" {
                    assert_eq!(fixture::pointers(&mirror), buffers);
                }
                if iteration >= 10 {
                    // Logical cell bytes copied by the explicit clone above;
                    // excludes metadata, validation reads and allocator traffic.
                    let copied = if mode == "in_place" {
                        0
                    } else {
                        sections * 4096 * 2
                    };
                    writeln!(
                        csv,
                        "{mode},{sections},{},{elapsed},{copied}",
                        iteration - 10
                    )?;
                }
            }
        }
    }
    Ok(())
}
