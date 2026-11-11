//! File-backed column installation alongside a paced kernel tick and replica loop.
mod dense_install_baseline {
    pub mod workload;
}
#[path = "../tests/support/mod.rs"]
mod support;
use dense_install_baseline::workload::*;
use std::{
    error::Error,
    fs,
    io::Write,
    time::{Duration, Instant},
};
use torse_diagnostics::{SerialProfiler, Stage};
use torse_server::{bootstrap as base, execution::*, *};
use torse_sim::{action::ActorContext, replica::*};
use torse_types::*;
use torse_world::SnapshotCells;
fn actor(scope: &ReplicaScope) -> ActorContext {
    scope
        .sections
        .iter()
        .fold(ActorContext::new(base::key("benchmark:viewer")), |a, p| {
            a.section(*p)
        })
}
fn delivered(
    mirror: &mut torse_net::ReplicaMirror,
    batch: WorldReplicaBatch,
    interest: bool,
) -> Result<(), Fault> {
    let mut bytes = Vec::new();
    torse_codec::wire::encode(&batch, &mut bytes, budget(), base::codec_limits())?;
    let batch = torse_codec::wire::decode(&bytes, budget(), base::codec_limits())?;
    if interest {
        mirror.apply_interest(batch)
    } else {
        mirror.apply(batch)
    }
}
fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 2
        || args[2..]
            .iter()
            .any(|v| v != "--unpaced" && v != "--uniform")
    {
        return Err(
            "usage: dense_install_baseline NEW_OUTPUT_DIRECTORY [--unpaced] [--uniform]".into(),
        );
    }
    let paced = !args.iter().any(|v| v == "--unpaced");
    let uniform = args.iter().any(|v| v == "--uniform");
    let out = std::path::Path::new(&args[1]);
    fs::create_dir(out)?;
    let mut csv = fs::File::create(out.join("samples.csv"))?;
    writeln!(
        csv,
        "tick,x,z,dense_pages,entities,input_prepare_ns,host_total_ns,kernel_total_ns,page_prepare_ns,validation_ns,encoding_ns,publication_ns,durable_wait_ns,flush_requests,stored_sections"
    )?;
    let dir = support::Directory::new();
    let (mut server, pages, template) = create(dir.acquire())?;
    for x in -2..=2 {
        for z in -2..=2 {
            let batch = column(server.authority()?, x, z, &template, uniform);
            assert!(matches!(
                server
                    .install_section_batch(&pages, batch, &mut NoReplicas)
                    .map_err(|e| format!("prefill {x},{z}: {e}"))?,
                CommitStep::Durable(_)
            ));
        }
    }
    let pool = MemoryPool::new(MemoryBudget::new(1024 * 1024 * 1024));
    server
        .attach_memory(pool.clone(), 0)
        .map_err(|e| format!("attach: {e}"))?;
    let viewer = Viewer {
        session: SessionId(1),
        actor: ActorContextId(1),
    };
    server.register_session(viewer, actor(&scope(0)), scope(0))?;
    let mut mirror = torse_net::ReplicaMirror::from_seed(server.open_replica(
        viewer.session,
        scope(0),
        budget(),
    )?)?;
    let profiler = SerialProfiler::new(1024);
    server.set_serial_profiler(profiler.clone());
    let start = Instant::now();
    let mut center = 0;
    let mut samples = 0;
    for tick in 1..=200 {
        if paced {
            let deadline = start + Duration::from_millis(tick * 50);
            if let Some(wait) = deadline.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
        let next = (tick * 109 / 10 / 20 / 16) as i64;
        if next > center {
            for z in -2..=2 {
                profiler.clear();
                let host = Instant::now();
                let input = column(server.authority()?, next + 2, z, &template, uniform);
                let dense = input
                    .iter()
                    .filter(|p| matches!(p.sections[0].cells, SnapshotCells::Dense(_)))
                    .count();
                let entities: usize = input.iter().map(|p| p.entities.len()).sum();
                let input_ns = host.elapsed().as_nanos();
                let before = server.flush_metrics();
                let result = server.install_section_batch(&pages, input, &mut NoReplicas)?;
                let host_ns = host.elapsed().as_nanos();
                assert!(matches!(result, CommitStep::Durable(_)));
                let d = server.section_install_diagnostics();
                let encoding = profiler.totals()[Stage::Encoding as usize].total_ns;
                let calls = server.flush_metrics().calls - before.calls;
                assert_eq!(calls, 1);
                writeln!(
                    csv,
                    "{tick},{},{z},{dense},{entities},{input_ns},{host_ns},{},{},{},{encoding},{},{},{calls},{}",
                    next + 2,
                    d.elapsed.as_nanos(),
                    d.page_preparation.as_nanos(),
                    d.validation.as_nanos(),
                    d.commit.as_nanos().saturating_sub(encoding),
                    d.durability_wait.as_nanos(),
                    server.authority()?.world().store().len()
                )?;
                samples += 1;
            }
            for batch in server.take_replica(mirror.subscription, budget())? {
                delivered(&mut mirror, batch, false)?;
            }
            for batch in server.change_session_interest(
                viewer.session,
                actor(&scope(next)),
                scope(next),
                budget(),
            )? {
                delivered(&mut mirror, batch, true)?;
            }
            for z in -2..=2 {
                for y in -4..20 {
                    server.unload_section(
                        &pages,
                        SectionPos::new(center - 2, y, z),
                        &mut NoReplicas,
                    )?;
                }
            }
            center = next;
            assert_eq!(server.authority()?.world().store().len(), 600);
        }
        server.drive_tick(&mut NoReplicas, 1024)?;
        for batch in server.take_replica(mirror.subscription, budget())? {
            delivered(&mut mirror, batch, false)?;
        }
    }
    assert_eq!(samples, 30);
    let seed = server.open_replica(viewer.session, scope(center), budget())?;
    assert_eq!(mirror.state(), &seed.state);
    let expected = server
        .authority()?
        .world()
        .capture_snapshot(torse_sim::streaming::page_meta(server.authority()?.world()))?;
    drop(server);
    let (world, actions) = base::world()?;
    let recovered = base::Files::open_file(
        dir.acquire(),
        world.coordinator().compatibility(),
        base::Codec::new(base::codec_limits())?,
        file_policy(),
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                world.registry(),
                world.schemas(),
                actions,
                Vec::new(),
                limits(),
            )
            .with_section_pages(pages.clone()))
        },
    )?;
    assert_eq!(
        expected,
        recovered
            .authority()?
            .world()
            .capture_snapshot(torse_sim::streaming::page_meta(
                recovered.authority()?.world()
            ))?
    );
    writeln!(
        fs::File::create(out.join("environment.txt"))?,
        "seed={SEED} paced={paced} uniform={uniform} initial_columns=25 sections_per_column=24 ticks=200 tick_ms=50 speed=10.9 samples=30 max_resident_sections=720 steady_resident_sections=600 memory_ledger_peak={} recovery=exact replica=exact flush_requests_per_sample=1; kernel fixture, not an external host content workload",
        pool.usage().total_peak
    )?;
    Ok(())
}
