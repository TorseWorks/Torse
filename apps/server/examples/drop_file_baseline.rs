//! Complete item-body ticks with file durability and authorized replicas.
#[path = "drop_baseline/workload.rs"]
mod workload;

use std::{error::Error, fs, io::Write, path::PathBuf, time::Instant};
use torse_codec::wire;
use torse_diagnostics::{SerialProfiler, Stage};
use torse_server::{bootstrap as base, execution::*, *};
use torse_sim::action::{ActionTarget, ActorContext};
use torse_types::*;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let output = PathBuf::from(args.next().ok_or("missing output directory")?);
    let bodies: usize = args.next().ok_or("missing body count")?.parse()?;
    let workers: usize = args.next().ok_or("missing worker count")?.parse()?;
    let ticks: u64 = args.next().ok_or("missing measured ticks")?.parse()?;
    if !(1..=64).contains(&bodies) || ![0, 1, 4].contains(&workers) || !(1..=300).contains(&ticks) {
        return Err("expected 1..64 bodies, 0/1/4 workers and 1..300 ticks".into());
    }
    fs::create_dir(&output)?;
    let path = output.join("world");
    fs::create_dir(&path)?;
    let (world, actions, catalog) = workload::world(bodies, false);
    let compatibility = world.coordinator().compatibility();
    let mut policy = base::file_policy();
    policy.journal.max_pending_records = 16;
    let codec = base::Codec::new(base::codec_limits())?;
    let mut server = base::Files::create_file(
        WorldDirectory::acquire(&path, base::WORLD, 65_536)?,
        compatibility,
        codec,
        policy,
        |binding| {
            ExecutionHost::new(
                binding,
                world,
                actions,
                workload::rules(catalog),
                base::execution_limits(),
            )
        },
    )?;
    if workers != 0 {
        server.set_parallel_planning(Some(PlanningConfig {
            workers,
            max_in_flight: 4,
        }))?;
    }
    let mut subscriptions = Vec::new();
    for id in 1..=2 {
        let actor = server.authority()?.world().entities().iter().fold(
            ActorContext::new(base::key("benchmark:viewer")),
            |actor, (_, entity)| actor.target(ActionTarget::Entity(entity.id)),
        );
        server.register_session(
            Viewer {
                session: SessionId(id),
                actor: ActorContextId(id),
            },
            actor,
            base::scope(),
        )?;
        let seed = server.open_replica(SessionId(id), base::scope(), base::budget())?;
        let mut bytes = Vec::new();
        wire::encode(&seed, &mut bytes, base::budget(), base::codec_limits())?;
        subscriptions.push((seed.subscription, bytes));
    }
    let profiler = SerialProfiler::new(0);
    server.set_serial_profiler(profiler.clone());
    let mut csv = fs::File::create(output.join("ticks.csv"))?;
    write!(csv, "tick,tick_ns,replica_batches,native_jobs")?;
    for stage in Stage::ALL {
        write!(csv, ",{}_ns", stage.name())?;
    }
    writeln!(csv)?;
    let mut prior_jobs = 0;
    let mut replica_batches = 0;
    for tick in 0..5 + ticks {
        profiler.clear();
        let started = Instant::now();
        let mut batches = 0;
        let mut complete = false;
        for _ in 0..256 {
            let batch = server.drive_batch(&mut NoReplicas, 16)?;
            assert!(batch.committed != 0 && batch.blocked.is_none());
            for (subscription, stream) in &mut subscriptions {
                for batch in server.take_replica(*subscription, base::budget())? {
                    let mut bytes = Vec::new();
                    wire::encode(&batch, &mut bytes, base::budget(), base::codec_limits())?;
                    assert!(stream.len() + bytes.len() <= 16 * 1024 * 1024);
                    stream.extend(bytes);
                    batches += 1;
                }
            }
            if server.authority()?.at_tick_boundary() {
                assert_eq!(server.authority()?.resume_point().tick, Tick(tick + 1));
                complete = true;
                break;
            }
        }
        assert!(complete);
        replica_batches += batches;
        let elapsed = started.elapsed().as_nanos();
        let jobs = server.authority()?.native_planning_stats().jobs;
        if tick >= 5 {
            write!(csv, "{tick},{elapsed},{batches},{}", jobs - prior_jobs)?;
            for stage in profiler.totals() {
                write!(csv, ",{}", stage.total_ns)?;
            }
            writeln!(csv)?;
        }
        prior_jobs = jobs;
    }
    assert!(replica_batches > 0);
    let state = workload::snapshot(server.authority()?);
    fs::write(output.join("state.bin"), &state)?;
    for (index, (_, bytes)) in subscriptions.into_iter().enumerate() {
        fs::write(output.join(format!("replica-{index}.bin")), bytes)?;
    }
    drop(server);
    let (world, actions, catalog) = workload::world(bodies, false);
    let recovered = base::Files::open_file(
        WorldDirectory::acquire(&path, base::WORLD, 65_536)?,
        compatibility,
        codec,
        policy,
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                world.registry(),
                world.schemas(),
                actions,
                workload::rules(catalog),
                base::execution_limits(),
            ))
        },
    )?;
    assert_eq!(workload::snapshot(recovered.authority()?), state);
    Ok(())
}
