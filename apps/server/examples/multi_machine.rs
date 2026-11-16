//! Real multi-machine Wasm workload. Compilation and recovery are outside timing.
#[path = "multi_machine/workload.rs"]
mod workload;
use std::cell::RefCell;
use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;
use torse_diagnostics::{SerialProfiler, Stage};
use torse_server::{bootstrap as base, execution::*, *};
use torse_sim::action::{ActionTarget, ActorContext};
use torse_storage::*;
use torse_types::*;
use workload::Load;

struct Options {
    output: PathBuf,
    load: Load,
    subscribers: u128,
    ticks: u64,
    warmup: u64,
    workers: usize,
    file: bool,
}
fn options() -> Result<Options, Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let output = args.next().ok_or("usage: multi_machine <new-output-directory> [--machines N] [--subscribers N] [--ticks N] [--warmup N] [--workers N] [--file] [--shared]")?;
    let mut o = Options {
        output: output.into(),
        load: Load {
            machines: 16,
            shared: false,
        },
        subscribers: 0,
        ticks: 30,
        warmup: 5,
        workers: 0,
        file: false,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--machines" => o.load.machines = args.next().ok_or("missing machines")?.parse()?,
            "--subscribers" => o.subscribers = args.next().ok_or("missing subscribers")?.parse()?,
            "--ticks" => o.ticks = args.next().ok_or("missing ticks")?.parse()?,
            "--warmup" => o.warmup = args.next().ok_or("missing warmup")?.parse()?,
            "--workers" => o.workers = args.next().ok_or("missing workers")?.parse()?,
            "--file" => o.file = true,
            "--shared" => o.load.shared = true,
            _ => return Err(format!("unknown argument {arg}").into()),
        }
    }
    if !(1..=64).contains(&o.load.machines)
        || o.subscribers > 8
        || o.workers > 64
        || o.ticks == 0
        || o.ticks > 100
        || o.warmup > 20
    {
        return Err("workload limits exceeded".into());
    }
    Ok(o)
}
struct Capture(Rc<RefCell<MemoryBackend>>);
impl JournalBackend for Capture {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.0.borrow_mut().append(bytes)
    }
    fn sync(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().sync()
    }
}
fn main() -> Result<(), Box<dyn Error>> {
    let o = options()?;
    fs::create_dir(&o.output)?;
    let game = o.load.assemble()?;
    let stats = Rc::clone(&game.stats);
    let compatibility = game.world.coordinator().compatibility();
    let mut policy = base::file_policy();
    policy.journal.max_pending_records = 16;
    let path = o.output.join("world");
    fs::create_dir(&path)?;
    let directory = WorldDirectory::acquire(&path, base::WORLD, 65536)?;
    if o.file {
        let mut server = base::Files::create_file(
            directory,
            compatibility,
            base::Codec::new(base::codec_limits())?,
            policy,
            |binding| {
                ExecutionHost::new(
                    binding,
                    game.world,
                    game.actions,
                    game.rules,
                    base::execution_limits(),
                )
            },
        )?;
        measure(&mut server, &o, &stats)?;
        let expected = workload::state(server.authority()?)?;
        drop(server);
        fs::write(
            o.output.join("journal.bin"),
            fs::read(path.join("journal.log"))?,
        )?;
        let game = o.load.assemble()?;
        let stats = Rc::clone(&game.stats);
        let recovered = base::Files::open_file(
            WorldDirectory::acquire(&path, base::WORLD, 65536)?,
            compatibility,
            base::Codec::new(base::codec_limits())?,
            policy,
            |binding| {
                Ok(ExecutionRecovery::new(
                    binding,
                    game.world.registry(),
                    game.world.schemas(),
                    game.actions,
                    game.rules,
                    base::execution_limits(),
                ))
            },
        )?;
        assert_eq!(workload::state(recovered.authority()?)?, expected);
        assert_eq!(stats.borrow().guest_dispatches.values().sum::<u64>(), 0);
        o.load.verify(recovered.authority()?, o.ticks + o.warmup);
    } else {
        let binding = Binding {
            world: base::WORLD,
            epoch: directory.epoch(),
            compatibility,
        };
        let bytes = Rc::new(RefCell::new(MemoryBackend::new(128 * 1024 * 1024)));
        let journal = Journal::with_backend(
            binding,
            CommitCursor::initial(base::WORLD),
            base::Codec::new(base::codec_limits())?,
            policy.journal,
            Capture(Rc::clone(&bytes)),
        )?;
        let checkpoints = Checkpoints::open(
            binding,
            base::Codec::new(base::codec_limits())?,
            policy.checkpoint,
            MemoryCheckpointBackend::new(16_777_216),
        )?;
        let host = ExecutionHost::new(
            binding,
            game.world,
            game.actions,
            game.rules,
            base::execution_limits(),
        )?;
        let mut server = Server::initialize(directory, host, journal, checkpoints, policy.server)?;
        measure(&mut server, &o, &stats)?;
        fs::write(o.output.join("journal.bin"), bytes.borrow().bytes())?;
    }
    Ok(())
}

fn measure<B: JournalBackend, S: HostCheckpointStore<ExecutionModel>>(
    server: &mut Server<ExecutionHost, Journal<ExecutionModel, base::Codec, B>, S>,
    o: &Options,
    stats: &Rc<RefCell<torse_server::mods::ModStats>>,
) -> Result<(), Box<dyn Error>> {
    server.set_commit_batch_limit(16)?;
    if o.workers != 0 {
        server.set_parallel_planning(Some(PlanningConfig {
            workers: o.workers,
            max_in_flight: o.workers,
        }))?;
        server.set_prevalidation(Some(PrevalidationConfig {
            workers: o.workers,
            components: true,
            max_job_bytes: 64 * 1024 * 1024,
            ..Default::default()
        }))?;
    }
    let mut streams = Vec::new();
    for id in 1..=o.subscribers {
        let actor = server.authority()?.world().entities().iter().fold(
            ActorContext::new(base::key("benchmark:operator")),
            |actor, (_, e)| actor.target(ActionTarget::Entity(e.id)),
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
        streams.push((seed.subscription, workload::encoded(&seed)?));
    }
    let profiler = SerialProfiler::new(0);
    server.set_serial_profiler(profiler.clone());
    let mut raw = File::create(o.output.join("ticks.csv"))?;
    writeln!(
        raw,
        "machines,shared,subscribers,workers,file,tick,measured,drive_ns,take_ns,planning_ns,preparation_ns,encoding_ns,publication_ns,projection_ns,flush_ns,encoded_bytes,dispatches,events,prevalidated,hits"
    )?;
    for tick in 0..o.warmup + o.ticks {
        profiler.clear();
        let before = stats.borrow().guest_dispatches.values().sum::<u64>();
        let mut drive_ns = 0u128;
        let mut take_ns = 0u128;
        let mut finished = false;
        for _ in 0..256 {
            let start = Instant::now();
            let batch = server.drive_batch(&mut NoReplicas, 16)?;
            drive_ns += start.elapsed().as_nanos();
            if batch.committed == 0 {
                return Err("machine workload stalled".into());
            }
            for (id, stream) in &mut streams {
                let start = Instant::now();
                let batches = server.take_replica(*id, base::budget())?;
                take_ns += start.elapsed().as_nanos();
                for batch in batches {
                    let encoded = workload::encoded(&batch)?;
                    if stream.len() + encoded.len() > 16 * 1024 * 1024 {
                        return Err("replica capture limit".into());
                    }
                    stream.extend(encoded);
                }
            }
            if server.authority()?.resume_point().tick.0 == tick + 1 {
                finished = true;
                break;
            }
        }
        assert!(finished);
        let totals = profiler.totals();
        let dispatched = stats.borrow().guest_dispatches.values().sum::<u64>() - before;
        let p = server.authority()?.prevalidation_stats();
        write!(
            raw,
            "{},{},{},{},{},{},{},{drive_ns},{take_ns}",
            o.load.machines,
            o.load.shared,
            o.subscribers,
            o.workers,
            o.file,
            tick,
            tick >= o.warmup
        )?;
        for stage in [
            Stage::Planning,
            Stage::Preparation,
            Stage::Encoding,
            Stage::Publication,
            Stage::Projection,
        ] {
            write!(raw, ",{}", totals[stage as usize].total_ns)?;
        }
        let flush_ns = [Stage::Flush, Stage::JournalAppend, Stage::JournalSync]
            .into_iter()
            .map(|stage| totals[stage as usize].total_ns)
            .sum::<u128>();
        write!(raw, ",{flush_ns}")?;
        writeln!(
            raw,
            ",{},{dispatched},{},{},{}",
            totals[Stage::Encoding as usize].bytes,
            server.authority()?.runtime().events.len(),
            p.prepared,
            p.hits
        )?;
    }
    let host = server.authority()?;
    let (input, output) = o.load.verify(host, o.warmup + o.ticks);
    let stats = stats.borrow();
    assert_eq!(stats.traps.values().sum::<u64>(), 0);
    assert_eq!(stats.rejections.values().sum::<u64>(), 0);
    assert_eq!(stats.transfer_rejections.values().sum::<u64>(), 0);
    let active = o.warmup + o.ticks - 1;
    let dispatches = if o.load.shared {
        o.load.machines as u64 * active.min(4) + active.saturating_sub(4)
    } else {
        o.load.machines as u64 * active
    };
    assert_eq!(stats.guest_dispatches.values().sum::<u64>(), dispatches);
    fs::write(o.output.join("snapshot.bin"), workload::state(host)?)?;
    for (id, stream) in streams {
        fs::write(o.output.join(format!("replica-{}.bin", id.0)), stream)?;
    }
    fs::write(
        o.output.join("validation.txt"),
        format!(
            "dispatches={dispatches}\ninput={input}\noutput={output}\nevents={}\nprevalidated={}\n",
            host.runtime().events.len(),
            host.prevalidation_stats().prepared
        ),
    )?;
    println!(
        "machines={} shared={} subscribers={} workers={} file={} input={input} output={output} dispatches={dispatches}",
        o.load.machines, o.load.shared, o.subscribers, o.workers, o.file
    );
    Ok(())
}
