//! Real item-body physics through complete durable ticks, with no synthetic CPU work.
#[path = "drop_baseline/workload.rs"]
mod workload;

use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use torse_diagnostics::{SerialProfiler, Stage};
use torse_server::{bootstrap as base, execution::*, *};
use torse_storage::*;
use torse_types::*;

struct Sink(Arc<Mutex<Vec<u8>>>);
impl JournalBackend for Sink {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
    fn sync(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn percentile(values: &[u64], percent: usize) -> u64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[(sorted.len() * percent).div_ceil(100) - 1]
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut output = PathBuf::from(".work/drop-baseline");
    let mut ticks = 30u32;
    let mut warmup = 5u32;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => output = args.next().ok_or("missing output")?.into(),
            "--ticks" => ticks = args.next().ok_or("missing ticks")?.parse()?,
            "--warmup" => warmup = args.next().ok_or("missing warmup")?.parse()?,
            _ => return Err(format!("unknown option: {arg}").into()),
        }
    }
    if ticks == 0 || ticks > 1000 || warmup > 100 {
        return Err("ticks must be 1..1000, warmup 0..100".into());
    }
    fs::create_dir_all(output.parent().unwrap_or(std::path::Path::new(".")))?;
    fs::create_dir(&output)?;
    let mut raw = File::create(output.join("ticks.csv"))?;
    let mut summary = File::create(output.join("summary.csv"))?;
    writeln!(raw, "bodies,clustered,workers,tick,planning_ns,tick_ns")?;
    writeln!(
        summary,
        "bodies,clustered,workers,planning_p50_ns,tick_p50_ns,tick_p95_ns,native_jobs,peak_lanes,state_crc32c,journal_crc32c"
    )?;
    for (bodies, clustered) in [(4, false), (32, false), (64, false), (64, true)] {
        let mut reference = None;
        for workers in [0, 1, 4] {
            let path = output.join(format!("{bodies}-{clustered}-{workers}"));
            fs::create_dir(&path)?;
            let (world, actions, catalog) = workload::world(bodies, clustered);
            let directory = WorldDirectory::acquire(&path, base::WORLD, 65_536)?;
            let binding = Binding {
                world: base::WORLD,
                epoch: directory.epoch(),
                compatibility: world.coordinator().compatibility(),
            };
            let host = ExecutionHost::new(
                binding,
                world,
                actions,
                workload::rules(catalog),
                base::execution_limits(),
            )?;
            let config = base::file_policy();
            let codec = base::Codec::new(base::codec_limits())?;
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let journal = Journal::with_backend(
                binding,
                CommitCursor::initial(base::WORLD),
                codec,
                config.journal,
                Sink(bytes.clone()),
            )?;
            let checkpoints = Checkpoints::open(
                binding,
                codec,
                config.checkpoint,
                MemoryCheckpointBackend::new(16_777_216),
            )?;
            let mut server =
                Server::initialize(directory, host, journal, checkpoints, config.server)?;
            server.set_commit_batch_limit(16)?;
            if workers != 0 {
                server.set_parallel_planning(Some(PlanningConfig {
                    workers,
                    max_in_flight: 4,
                }))?;
            }
            let profiler = SerialProfiler::new(0);
            server.set_serial_profiler(profiler.clone());
            let mut planning = Vec::new();
            let mut elapsed = Vec::new();
            for tick in 0..warmup + ticks {
                profiler.clear();
                let start = Instant::now();
                server.drive_tick(&mut NoReplicas, 4096)?;
                let time = start.elapsed().as_nanos() as u64;
                let plan = profiler.totals()[Stage::Planning as usize].total_ns as u64;
                if tick >= warmup {
                    writeln!(raw, "{bodies},{clustered},{workers},{tick},{plan},{time}")?;
                    elapsed.push(time);
                    planning.push(plan);
                }
            }
            let state = workload::snapshot(server.authority()?);
            let journal = bytes.lock().unwrap().clone();
            let stats = server.authority()?.native_planning_stats();
            if let Some((previous_state, previous_journal)) = &reference {
                assert_eq!(&state, previous_state);
                assert_eq!(&journal, previous_journal);
            }
            let state_crc = crc32c::crc32c(&state);
            let journal_crc = crc32c::crc32c(&journal);
            fs::write(path.join("state.bin"), &state)?;
            fs::write(path.join("journal.bin"), &journal)?;
            let p50 = percentile(&elapsed, 50);
            let p95 = percentile(&elapsed, 95);
            let plan = percentile(&planning, 50);
            writeln!(
                summary,
                "{bodies},{clustered},{workers},{plan},{p50},{p95},{},{},{state_crc:08x},{journal_crc:08x}",
                stats.jobs, stats.peak_lanes
            )?;
            println!(
                "bodies={bodies} clustered={clustered} workers={workers} planning_us={} tick_p50_us={} tick_p95_us={} native_jobs={} lanes={}",
                plan / 1000,
                p50 / 1000,
                p95 / 1000,
                stats.jobs,
                stats.peak_lanes
            );
            reference = Some((state, journal));
        }
    }
    let toolchain = std::process::Command::new("rustc")
        .arg("--version")
        .output()?;
    fs::write(
        output.join("run.txt"),
        format!(
            "ticks={ticks}\nwarmup={warmup}\nbackend=memory-journal-with-complete-byte-comparison\nsubscribers=0\ncommit_batch_limit=16\nmax_in_flight=4\nlogical_cpus={}\n{}",
            std::thread::available_parallelism()?.get(),
            String::from_utf8(toolchain.stdout)?
        ),
    )?;
    Ok(())
}
