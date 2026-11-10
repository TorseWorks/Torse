//! Full seed versus incremental interest, sharing public network/file fixtures.
#[allow(dead_code)]
#[path = "../tests/support/interest.rs"]
mod fixture;
#[path = "../tests/support/mod.rs"]
mod support;
use std::{error::Error, fs, io::Write, time::Instant};
use torse_codec::wire;
use torse_net::ReplicaMirror;
use torse_server::{bootstrap as base, network::*};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    if !(2..=3).contains(&args.len()) || args.get(2).is_some_and(|arg| arg != "--interest-first") {
        return Err("usage: interest_baseline NEW_OUTPUT_DIRECTORY [--interest-first]".into());
    }
    let output = std::path::Path::new(&args[1]);
    fs::create_dir(output)?;
    let mut csv = fs::File::create(output.join("samples.csv"))?;
    writeln!(
        csv,
        "mode,step,scope_start,message,terrain_sections,retained_terrain_sections,bytes,host_ns,initial_seed_bytes"
    )?;
    let modes = if args.len() == 3 {
        ["interest", "seed"]
    } else {
        ["seed", "interest"]
    };
    let mut expected = std::collections::BTreeMap::new();
    for mode in modes {
        let dir = support::Directory::new();
        let mut server = fixture::create(&dir);
        let (mut hub, id, mut client, mut mirror, initial) =
            fixture::connect(&mut server, fixture::scope(0));
        for (step, start) in [32, 0, 32, 0, 32, 0].into_iter().enumerate() {
            let policy = PeerPolicy {
                actor: fixture::actor(),
                scope: fixture::scope(start),
            };
            let now = Instant::now();
            if mode == "seed" {
                hub.reset_policy(&mut server, id, policy)?;
            } else {
                hub.replace_policy(&mut server, id, policy)?;
            }
            let host_ns = now.elapsed().as_nanos();
            let (messages, bytes) = fixture::drain(&mut client);
            assert_eq!(messages.len(), 1);
            let (message, sections, retained) = match messages.into_iter().next().unwrap() {
                ServerMessage::Seed(seed) => {
                    let sections = seed.state.sections.len();
                    let retained = seed
                        .state
                        .sections
                        .iter()
                        .filter(|s| (32..256).contains(&s.pos.x))
                        .count();
                    mirror = ReplicaMirror::from_seed(seed)?;
                    ("Seed", sections, retained)
                }
                ServerMessage::Interest(batch) => {
                    let sections = batch.changes.sections.len();
                    let retained = batch
                        .changes
                        .sections
                        .iter()
                        .filter(|s| (32..256).contains(&s.pos.x))
                        .count();
                    mirror.apply_interest(batch)?;
                    ("Interest", sections, retained)
                }
                other => panic!("unexpected message: {other:?}"),
            };
            assert_eq!(
                (sections, retained),
                if mode == "seed" { (256, 224) } else { (32, 0) }
            );
            assert_eq!(
                mirror.state,
                fixture::reference(&mut server, fixture::actor(), fixture::scope(start))
            );
            let mut state = Vec::new();
            wire::encode(
                &mirror.state,
                &mut state,
                base::budget(),
                base::codec_limits(),
            )?;
            if let Some(previous) = expected.get(&step) {
                assert_eq!(&state, previous);
            } else {
                expected.insert(step, state.clone());
            }
            fs::write(output.join(format!("{mode}-{step}-state.bin")), state)?;
            writeln!(
                csv,
                "{mode},{step},{start},{message},{sections},{retained},{bytes},{host_ns},{initial}"
            )?;
        }
    }
    Ok(())
}
