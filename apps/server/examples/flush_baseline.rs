//! Isolated serial flush experiment using the public authority and journal contracts.
#[path = "flush_baseline/meter.rs"]
mod meter;
#[allow(dead_code)]
#[path = "serial_baseline/workload.rs"]
mod reference;
use meter::*;
use std::{
    error::Error,
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::Instant,
};
use torse_server::{bootstrap as base, execution::*, *};
use torse_sim::{OrderingKey, Phase, SystemSpec, Transaction, WorldAuthority};
use torse_storage::*;
use torse_types::*;
use torse_world::Position;

struct Entities;
impl DurableRule for Entities {
    fn spec(&self) -> SystemSpec {
        SystemSpec {
            id: base::key("flush:entities"),
            phase: Phase::Simulation,
            rule_priority: 0,
            before: vec![],
            after: vec![],
        }
    }
    fn plan(&self, world: &WorldAuthority, tick: Tick) -> Result<Vec<WorkPlan>, Fault> {
        world
            .entities()
            .iter()
            .enumerate()
            .map(|(index, (handle, entity))| {
                let mut tx = Transaction::new(
                    TransactionId(u128::from(tick.0) * 32 + index as u128 + 1),
                    self.spec().id,
                    OrderingKey {
                        phase: Phase::Simulation,
                        rule_priority: 0,
                        stable_task_key: index as u64,
                        local_sequence: 0,
                    },
                );
                tx.move_entity(
                    handle,
                    entity.id,
                    entity.position,
                    [
                        if tick.0.is_multiple_of(2) {
                            0.125
                        } else {
                            -0.125
                        },
                        0.0,
                        0.0,
                    ],
                    [0.3, 0.9, 0.3],
                );
                Ok(WorkPlan::transaction(tx))
            })
            .collect()
    }
}
fn rules(case: &str) -> Vec<Box<dyn DurableRule>> {
    match case {
        "empty" => vec![],
        "light" => reference::Workload {
            tasks: 1,
            contended: false,
        }
        .rules(),
        "entities" => vec![Box::new(Entities)],
        _ => unreachable!(),
    }
}
fn initial(case: &str) -> Result<(WorldAuthority, torse_sim::action::ActionService), Fault> {
    let (mut world, actions) = base::world()?;
    if case == "entities" {
        // Replace the two bootstrap demonstration entities before initialization.
        let handles: Vec<_> = world.entities().iter().map(|(handle, _)| handle).collect();
        for handle in handles {
            world
                .entities_mut()
                .despawn(handle)
                .expect("initial entity exists");
        }
        for index in 0..16 {
            world
                .entities_mut()
                .spawn(Position::new(1.0 + index as f64, 8.0, 8.0))
                .map_err(|error| Fault::invalid_data(format!("initial entity: {error:?}")))?;
        }
    }
    Ok((world, actions))
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut output = PathBuf::from(".work/flush-baseline");
    let mut backend = String::from("file");
    let mut ticks = 30u32;
    let mut warmup = 5u32;
    let mut enabled = true;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => output = args.next().ok_or("missing output")?.into(),
            "--backend" => backend = args.next().ok_or("missing backend")?,
            "--ticks" => ticks = args.next().ok_or("missing ticks")?.parse()?,
            "--warmup" => warmup = args.next().ok_or("missing warmup")?.parse()?,
            "--unmeasured" => enabled = false,
            _ => return Err(format!("unknown option: {arg}").into()),
        }
    }
    if ticks == 0 || ticks > 1000 || warmup > 100 || !matches!(backend.as_str(), "memory" | "file")
    {
        return Err("ticks=1..1000 warmup=0..100 backend=memory|file".into());
    }
    fs::create_dir_all(output.parent().unwrap_or(Path::new(".")))?;
    fs::create_dir(&output)?;
    let mut csv = BufWriter::new(File::create(output.join("ticks.csv"))?);
    writeln!(
        csv,
        "case,tick,tick_ns,flush_requests,durable_hits,backend_sync_calls,file_sync_all_calls,append_records,append_bytes,file_write_calls,file_written_bytes,append_ns,sync_ns,flush_ns,freeze_records,advance_records,task_records,durable_seq"
    )?;
    for case in ["empty", "light", "entities"] {
        run_case(&output, &backend, case, ticks, warmup, enabled, &mut csv)?;
    }
    csv.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "torse-flush-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }
    #[test]
    fn measured_memory_and_file_and_unmeasured_execution_have_identical_states() {
        let roots = [directory(), directory(), directory(), directory()];
        for case in ["empty", "light", "entities"] {
            for (root, kind, enabled) in [
                (&roots[0], "memory", true),
                (&roots[1], "file", true),
                (&roots[2], "memory", false),
                (&roots[3], "file", false),
            ] {
                run_case(root, kind, case, 2, 1, enabled, &mut Vec::new()).unwrap();
            }
            let expected = fs::read(roots[0].join(format!("{case}-state.bin"))).unwrap();
            assert_eq!(
                fs::read(roots[1].join(case).join("journal.log")).unwrap(),
                fs::read(roots[3].join(case).join("journal.log")).unwrap()
            );
            for root in &roots[1..] {
                assert_eq!(
                    fs::read(root.join(format!("{case}-state.bin"))).unwrap(),
                    expected
                );
            }
        }
    }
    #[test]
    fn already_durable_and_invalid_targets_do_not_synchronize() {
        let (world, _) = initial("empty").unwrap();
        let binding = Binding {
            world: base::WORLD,
            epoch: WorldEpoch(1),
            compatibility: world.coordinator().compatibility(),
        };
        let meter = Meter::new(true);
        let mut journal = Observed {
            inner: Journal::with_backend(
                binding,
                CommitCursor::initial(base::WORLD),
                base::Codec::new(base::codec_limits()).unwrap(),
                base::file_policy().journal,
                Backend {
                    store: Store::Memory(MemoryBackend::new(4096)),
                    meter: meter.clone(),
                },
            )
            .unwrap(),
            meter: meter.clone(),
        };
        for _ in 0..2 {
            journal
                .flush_through(CommitCursor::initial(base::WORLD))
                .unwrap();
        }
        let mut invalid = CommitCursor::initial(base::WORLD);
        invalid.seq = CommitSeq(1);
        assert!(journal.flush_through(invalid).is_err());
        let counts = meter.counts.get();
        assert_eq!(
            (counts.requests, counts.hits, counts.syncs, counts.records),
            (3, 2, 0, 0)
        );
        assert_eq!(journal.durability().through.0, 0);
    }
}

fn run_case(
    root: &Path,
    kind: &str,
    case: &str,
    ticks: u32,
    warmup: u32,
    enabled: bool,
    csv: &mut impl Write,
) -> Result<(), Box<dyn Error>> {
    let path = root.join(case);
    fs::create_dir(&path)?;
    fs::create_dir(path.join("checkpoints"))?;
    let directory = WorldDirectory::acquire(&path, base::WORLD, 1_048_576)?;
    let (world, actions) = initial(case)?;
    let initial_positions: Vec<_> = world
        .entities()
        .iter()
        .map(|(_, entity)| entity.position)
        .collect();
    let binding = Binding {
        world: base::WORLD,
        epoch: directory.epoch(),
        compatibility: world.coordinator().compatibility(),
    };
    let config = base::file_policy();
    let meter = Meter::new(enabled);
    let store = if kind == "file" {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path.join("journal.log"))?;
        file.try_lock()?;
        file.sync_all()?;
        #[cfg(unix)]
        File::open(&path)?.sync_all()?;
        Store::File(file)
    } else {
        Store::Memory(MemoryBackend::new(134_217_728))
    };
    let codec = base::Codec::new(base::codec_limits())?;
    let journal = Observed {
        inner: Journal::with_backend(
            binding,
            CommitCursor::initial(base::WORLD),
            codec,
            config.journal,
            Backend {
                store,
                meter: meter.clone(),
            },
        )?,
        meter: meter.clone(),
    };
    // Initial checkpoint is real on both paths, outside all sample windows.
    let checkpoints = Checkpoints::open(
        binding,
        codec,
        config.checkpoint,
        FileCheckpointBackend::open(path.join("checkpoints"))?,
    )?;
    let host = ExecutionHost::new(
        binding,
        world,
        actions,
        rules(case),
        base::execution_limits(),
    )?;
    let mut server = Server::initialize(directory, host, journal, checkpoints, config.server)?;
    let tasks = match case {
        "empty" => 0,
        "light" => 1,
        _ => 16,
    };
    let mut samples = Vec::with_capacity(ticks as usize);
    for tick in 0..warmup + ticks {
        meter.reset();
        let before = server.flush_metrics();
        let start = Instant::now();
        let steps = server.drive_tick(&mut NoReplicas, 128)?;
        let tick_ns = start.elapsed().as_nanos() as u64;
        let c = meter.counts.get();
        let coordinator = server.flush_metrics();
        let host = server.authority()?;
        assert_eq!(steps, tasks + 12);
        assert!(host.at_tick_boundary());
        assert_eq!(host.resume_point().tick, Tick(u64::from(tick + 1)));
        assert!(server.durability().covers(host.cursor()));
        if case == "entities" {
            assert_eq!(initial_positions.len(), 16);
            for (index, (_, entity)) in host.world().entities().iter().enumerate() {
                let initial = initial_positions[index];
                assert_eq!(
                    entity.position,
                    Position::new(
                        initial.x + if tick.is_multiple_of(2) { 0.125 } else { 0.0 },
                        initial.y,
                        initial.z
                    )
                );
            }
        }
        if enabled {
            assert_eq!(c.requests, coordinator.calls - before.calls);
            assert_eq!(c.requests, c.hits + c.syncs);
            assert_eq!(c.records, u64::from(steps));
            assert_eq!((c.freezes, c.advances, c.tasks), (6, 6, u64::from(tasks)));
            assert!(c.sync_ns + c.append_ns <= c.flush_ns && c.flush_ns <= tick_ns);
            if kind == "file" {
                assert_eq!(c.bytes, c.written);
                assert_eq!(c.file_syncs, c.syncs);
            }
        }
        if tick >= warmup {
            samples.push((tick - warmup, tick_ns, c, server.durability().through.0));
        }
    }
    for (tick, tick_ns, c, durable) in samples {
        if !enabled {
            writeln!(csv, "{case},{tick},{tick_ns},{}{durable}", ",".repeat(14))?;
        } else {
            writeln!(
                csv,
                "{case},{},{tick_ns},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                tick,
                c.requests,
                c.hits,
                c.syncs,
                c.file_syncs,
                c.records,
                c.bytes,
                c.writes,
                c.written,
                c.append_ns,
                c.sync_ns,
                c.flush_ns,
                c.freezes,
                c.advances,
                c.tasks,
                durable
            )?;
        }
    }
    let expected = reference::state_bytes(server.authority()?)?;
    fs::write(root.join(format!("{case}-state.bin")), &expected)?;
    // Verify actual effects as well as backend equality; empty work still has phase records.
    if case == "light" {
        reference::Workload {
            tasks: 1,
            contended: false,
        }
        .verify(server.authority()?.world(), u64::from(warmup + ticks));
    }
    if case == "entities" {
        for (index, (_, entity)) in server.authority()?.world().entities().iter().enumerate() {
            assert_eq!(
                entity.position,
                Position::new(
                    initial_positions[index].x
                        + if (warmup + ticks).is_multiple_of(2) {
                            0.0
                        } else {
                            0.125
                        },
                    initial_positions[index].y,
                    initial_positions[index].z
                )
            );
        }
    }
    drop(server);
    if kind == "file" {
        let (world, actions) = initial(case)?;
        let recovered = base::Files::open_file(
            WorldDirectory::acquire(&path, base::WORLD, 1_048_576)?,
            binding.compatibility,
            codec,
            config,
            |binding| {
                Ok(ExecutionRecovery::new(
                    binding,
                    world.registry(),
                    world.schemas(),
                    actions,
                    rules(case),
                    base::execution_limits(),
                ))
            },
        )?;
        assert_eq!(reference::state_bytes(recovered.authority()?)?, expected);
    }
    println!(
        "case={case} backend={kind} state_crc32c={:08x} recovery={}",
        crc32c::crc32c(&expected),
        if kind == "file" {
            "verified"
        } else {
            "not_persistent"
        }
    );
    Ok(())
}
