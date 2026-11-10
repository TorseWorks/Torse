//! Reuse public file-world fixtures to compare single-page and bounded residency.
#[allow(dead_code, unused_imports)]
#[path = "../tests/residency.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "serial_baseline/workload.rs"]
mod state;
use std::{
    error::Error,
    fs,
    io::Write,
    sync::Arc,
    time::{Duration, Instant},
};
use torse_server::{residency::*, *};
use torse_types::*;
use torse_world::*;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    if !(2..=3).contains(&args.len()) || args.get(2).is_some_and(|arg| arg != "--batch-first") {
        return Err("usage: residency_baseline NEW_OUTPUT_DIRECTORY [--batch-first]".into());
    }
    let output = std::path::Path::new(&args[1]);
    fs::create_dir(output)?;
    let mut csv = fs::File::create(output.join("samples.csv"))?;
    writeln!(
        csv,
        "mode,case,pages,flush_requests,backend_sync_calls,first_ready_ns,total_ready_ns,max_poll_ns,polls"
    )?;
    let mut expected = std::collections::BTreeMap::new();
    let modes = if args.len() == 3 { [24, 1] } else { [1, 24] };
    for batch_pages in modes {
        let dir = fixture::Directory::new();
        let pages = fixture::pages(&dir);
        let mut server = fixture::create(&dir, Vec::new());
        let desired: Vec<_> = (0..26).map(|x| SectionPos::new(x, 0, 0)).collect();
        let source = Arc::new(fixture::EmptyGenerator {
            identity: fixture::identity(),
            gate: None,
        });
        let config = ResidencyConfig {
            max_resident: 26,
            max_io_bytes: 24 * 32 * 1024 * 1024,
        };
        for case in ["install", "reload"] {
            if case == "reload" {
                for pos in desired.iter().skip(2) {
                    assert!(matches!(
                        server.unload_section(&pages, *pos, &mut NoReplicas)?,
                        CommitStep::Durable(_)
                    ));
                }
            }
            server.enable_residency(config, pages.clone(), Some(source.clone()), &desired)?;
            server.set_residency_batch_pages(batch_pages)?;
            let initial_calls = server.flush_metrics().calls;
            let started = Instant::now();
            let mut first = None;
            let mut max_call = 0;
            let mut polls = 0;
            if batch_pages == 1 {
                // Original public synchronous path, excluding fixture setup.
                for pos in desired.iter().skip(2) {
                    let call = Instant::now();
                    let step = if case == "reload" {
                        server.reload_section(&pages, *pos, &mut NoReplicas)?
                    } else {
                        let world = server.authority()?.world();
                        let page = Snapshot {
                            meta: torse_sim::streaming::page_meta(world),
                            side: world.store().side(),
                            bounds: world.store().bounds(),
                            states: Vec::new(),
                            sections: vec![SnapshotSection {
                                pos: *pos,
                                cells: SnapshotCells::Uniform(BlockStateId::EMPTY),
                            }],
                            entities: Vec::new(),
                            next_entity_id: world.entities().next_entity_id(),
                        };
                        server.install_section(&pages, page, &mut NoReplicas)?
                    };
                    assert!(matches!(step, CommitStep::Durable(_)));
                    max_call = max_call.max(call.elapsed().as_nanos());
                    first.get_or_insert_with(|| started.elapsed().as_nanos());
                    polls += 1;
                }
            } else {
                loop {
                    let call = Instant::now();
                    let progress = server.poll_residency(&mut NoReplicas)?;
                    max_call = max_call.max(call.elapsed().as_nanos());
                    polls += 1;
                    match progress {
                        ResidencyProgress::Idle => break,
                        ResidencyProgress::Pending => {}
                        ResidencyProgress::Batch(report) => {
                            assert!(report.failure.is_none(), "{report:?}");
                            if !report.committed.is_empty() {
                                first.get_or_insert_with(|| started.elapsed().as_nanos());
                            }
                        }
                        other => panic!("unexpected progress: {other:?}"),
                    }
                    if started.elapsed() > Duration::from_secs(30) {
                        return Err("residency timeout".into());
                    }
                    std::thread::yield_now();
                }
            }
            let total = started.elapsed().as_nanos();
            writeln!(
                csv,
                "{batch_pages},{case},24,{},,{},{total},{max_call},{polls}",
                server.flush_metrics().calls - initial_calls,
                first.unwrap()
            )?;
            let bytes = state::state_bytes(server.authority()?)?;
            if let Some(previous) = expected.get(case) {
                assert_eq!(&bytes, previous);
            } else {
                expected.insert(case, bytes.clone());
            }
            fs::write(
                output.join(format!("{batch_pages}-{case}-state.bin")),
                bytes,
            )?;
        }
        let before = state::state_bytes(server.authority()?)?;
        drop(server);
        let log = fs::read(dir.0.join("journal.log"))?;
        fs::write(output.join(format!("{batch_pages}-journal.log")), &log)?;
        let recovered = fixture::open(&dir, pages, Vec::new());
        assert_eq!(state::state_bytes(recovered.authority()?)?, before);
        drop(recovered);
    }
    assert_eq!(
        fs::read(output.join("1-journal.log"))?,
        fs::read(output.join("24-journal.log"))?
    );
    Ok(())
}
