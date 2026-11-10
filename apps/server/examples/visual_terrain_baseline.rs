//! Measure mirror availability for saved display pages without expanding simulation.
#[allow(dead_code, unused_imports)]
#[path = "../tests/support/visual.rs"]
mod fixture;
#[path = "../tests/support/mod.rs"]
mod support;
use fixture::*;
use std::{error::Error, fs, io::Write};
use torse_types::*;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 2 {
        return Err("usage: visual_terrain_baseline NEW_OUTPUT_DIRECTORY".into());
    }
    let output = std::path::Path::new(&args[1]);
    fs::create_dir(output)?;
    let mut csv = fs::File::create(output.join("samples.csv"))?;
    writeln!(
        csv,
        "step,center,available,wanted,first_ready_ns,all_ready_ns,max_poll_ns,transport_bytes,simulation_sections,server_cache_bytes,server_reserved_bytes,client_cache_bytes"
    )?;
    let dir = support::Directory::new();
    let (mut server, pages, expected) = create(&dir);
    let (mut hub, id, mut client, _, _) = connect(&mut server, scope(0, 1));
    let mut display = mirror(&server);
    hub.enable_visual_interest(
        &server,
        id,
        pages,
        scope(2, 24),
        scope(2, 8),
        SectionPos::new(2, 0, 0),
        limits(),
    )?;
    let cursor = server.authority()?.world().seq();
    for (step, center) in (2..18).chain((2..18).rev()).chain([2, 10, 2]).enumerate() {
        let desired = scope(center, 8);
        hub.set_visual_interest(id, desired.clone(), SectionPos::new(center, 0, 0))?;
        let (first, all, bytes, max_poll) = fill(&mut hub, &mut server, &mut client, &mut display);
        for pos in desired.sections {
            assert_eq!(display.page(pos), expected.get(&pos));
        }
        assert_eq!(server.authority()?.world().seq(), cursor);
        let d = hub.visual_terrain_diagnostics(id).unwrap();
        writeln!(
            csv,
            "{step},{center},{},{},{},{},{},{},{},{},{},{}",
            display.pages().count(),
            display.desired().len(),
            first.as_nanos(),
            all.as_nanos(),
            max_poll.as_nanos(),
            bytes,
            server.authority()?.world().store().len(),
            d.cache_bytes,
            d.reserved_bytes,
            display.accounted_bytes()
        )?;
    }
    writeln!(
        fs::File::create(output.join("environment.txt"))?,
        "os={} arch={} mode=release fixture=24 saved dense edited pages with entities; near=1 subscribed/2 resident; display=8; movement=16 forward,16 reverse,3 revisit; first_ready=CPU mirror availability; GPU presentation and RSS not measured; server_reserved_bytes=fixed logical I/O/cache/output reservation; caches=logical observed payload",
        std::env::consts::OS,
        std::env::consts::ARCH
    )?;
    Ok(())
}
