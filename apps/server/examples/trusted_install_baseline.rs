//! File-backed generated-entity installs using the public streaming fixture.
#[allow(dead_code)]
#[path = "../tests/support/streaming.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "serial_baseline/workload.rs"]
mod state;
#[path = "../tests/support/mod.rs"]
mod support;
use std::{error::Error, fs, io::Write, time::Instant};
use torse_server::{installs::*, *};
use torse_types::*;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    if !(2..=3).contains(&args.len()) || args.get(2).is_some_and(|s| s != "--batch-first") {
        return Err("usage: trusted_install_baseline NEW_OUTPUT_DIRECTORY [--batch-first]".into());
    }
    let output = std::path::Path::new(&args[1]);
    fs::create_dir(output)?;
    let mut csv = fs::File::create(output.join("samples.csv"))?;
    writeln!(
        csv,
        "mode,pages,flush_requests,backend_sync_calls,total_ready_ns,max_call_ns"
    )?;
    let modes = if args.len() == 3 {
        ["batch", "single"]
    } else {
        ["single", "batch"]
    };
    let mut expected = None;
    let mut expected_log = None;
    for mode in modes {
        let dir = support::Directory::new();
        let pages = fixture::pages(&dir);
        let mut server = fixture::create(&dir);
        let input: Vec<_> = (2..26)
            .map(|x| {
                fixture::generated_with_entity(
                    server.authority().unwrap().world(),
                    SectionPos::new(x, 0, 0),
                )
            })
            .collect();
        let before = server.flush_metrics().calls;
        let now = Instant::now();
        let mut max_call = 0;
        if mode == "single" {
            for mut page in input {
                let call = Instant::now();
                let world = server.authority()?.world();
                page.meta = torse_sim::streaming::page_meta(world);
                let first = world.entities().next_entity_id();
                page.entities[0].id = EntityId(first);
                page.next_entity_id = first + 1;
                assert!(matches!(
                    server.install_section(&pages, page, &mut NoReplicas)?,
                    CommitStep::Durable(_)
                ));
                max_call = max_call.max(call.elapsed().as_nanos());
            }
        } else {
            // Include bounded handle construction and input validation in total.
            let mut batch = server.begin_section_installs(
                pages.clone(),
                input,
                SectionInstallLimits {
                    max_pages: 24,
                    max_bytes: 8 * 1024 * 1024,
                },
            )?;
            while batch.remaining() != 0 {
                let call = Instant::now();
                let report = server.install_sections(&mut batch, &mut NoReplicas)?;
                assert!(report.failure.is_none() && !report.installed.is_empty());
                assert!(
                    report
                        .installed
                        .iter()
                        .all(|p| report.durability.covers(p.at))
                );
                max_call = max_call.max(call.elapsed().as_nanos());
            }
        }
        let total = now.elapsed().as_nanos();
        let flushes = server.flush_metrics().calls - before;
        assert_eq!(flushes, if mode == "single" { 24 } else { 6 });
        writeln!(csv, "{mode},24,{flushes},,{total},{max_call}")?;
        let bytes = state::state_bytes(server.authority()?)?;
        if let Some(expected) = &expected {
            assert_eq!(&bytes, expected);
        } else {
            expected = Some(bytes.clone());
        }
        fs::write(output.join(format!("{mode}-state.bin")), &bytes)?;
        drop(server);
        let log = fs::read(dir.0.join("journal.log"))?;
        if let Some(expected) = &expected_log {
            assert_eq!(&log, expected);
        } else {
            expected_log = Some(log.clone());
        }
        fs::write(output.join(format!("{mode}-journal.log")), log)?;
        let recovered = fixture::open(&dir, pages)?;
        assert_eq!(state::state_bytes(recovered.authority()?)?, bytes);
    }
    Ok(())
}
