//! Actual machine guest, independent of disk synchronization and build startup.
use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;
use torse_diagnostics::{SerialProfiler, Stage};
use torse_server::{bootstrap as base, execution::*, mods, *};
use torse_sim::action::{ActionTarget, ActorContext};
use torse_storage::*;
use torse_types::*;

struct Sink;
impl JournalBackend for Sink {
    fn append(&mut self, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    fn sync(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let output = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: mod_baseline <new-output-directory>")?,
    );
    fs::create_dir(&output)?;
    let mut raw = File::create(output.join("ticks.csv"))?;
    let mut summary = File::create(output.join("summary.csv"))?;
    writeln!(
        raw,
        "machines,subscribers,tick,drive_ns,planning_ns,preparation_ns,projection_ns,guest_dispatches"
    )?;
    writeln!(
        summary,
        "machines,subscribers,drive_mean_ns,planning_mean_ns,preparation_mean_ns,projection_mean_ns,dispatches,input_count,output_count"
    )?;
    let machines = 1u32;
    for subscribers in [0, 8] {
        let mut game = mods::mod_world(&[PathBuf::from("examples/mods/machine")], true, &[])?;
        let schemas = game.world.schemas().clone();
        let catalog = torse_inventory::ItemCatalog::new([(base::key("base:stone"), 64)]).unwrap();
        let mut stock = torse_inventory::Inventory::new(2).unwrap();
        stock
            .set(
                0,
                Some(torse_inventory::ItemStack {
                    data: Default::default(),
                    item: base::key("base:stone"),
                    count: 64,
                }),
                &catalog,
            )
            .unwrap();
        let input = game.world.entities().handle_of(game.input).unwrap();
        game.world
            .entities_mut()
            .components_mut(input)
            .unwrap()
            .insert(&schemas, &torse_inventory::schema(), stock.fields())?;
        let directory_path = output.join(format!("{machines}-{subscribers}"));
        fs::create_dir(&directory_path)?;
        let directory = WorldDirectory::acquire(directory_path, base::WORLD, 65536)?;
        let binding = Binding {
            world: base::WORLD,
            epoch: directory.epoch(),
            compatibility: game.world.coordinator().compatibility(),
        };
        let mut policy = base::file_policy();
        policy.journal.max_pending_records = 16;
        let codec = base::Codec::new(base::codec_limits())?;
        let journal = Journal::with_backend(
            binding,
            CommitCursor::initial(base::WORLD),
            codec,
            policy.journal,
            Sink,
        )?;
        let cp = Checkpoints::open(
            binding,
            codec,
            policy.checkpoint,
            MemoryCheckpointBackend::new(16_777_216),
        )?;
        let stats = game.stats.clone();
        let input_id = game.input;
        let output_id = game.output;
        let authority = ExecutionHost::new(
            binding,
            game.world,
            game.actions,
            game.rules,
            base::execution_limits(),
        )?;
        let mut server = Server::initialize(directory, authority, journal, cp, policy.server)?;
        server.set_commit_batch_limit(16)?;
        for id in 1..=subscribers {
            let actor = server.authority()?.world().entities().iter().fold(
                ActorContext::new(base::key("benchmark:operator")),
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
            server.open_replica(SessionId(id), base::scope(), base::budget())?;
        }
        let profiler = SerialProfiler::new(0);
        server.set_serial_profiler(profiler.clone());
        let mut times = vec![];
        let mut plans = vec![];
        let mut prepares = vec![];
        let mut projections = vec![];
        for tick in 0..25 {
            profiler.clear();
            let before = stats.borrow().guest_dispatches.values().sum::<u64>();
            let start = Instant::now();
            server.drive_tick(&mut NoReplicas, 256)?;
            let elapsed = start.elapsed().as_nanos() as u64;
            // Drain each authorized stream after its complete durable tick.
            for id in 1..=subscribers {
                server.take_replica(SubscriptionId(id), base::budget())?;
            }
            let totals = profiler.totals();
            let plan = totals[Stage::Planning as usize].total_ns as u64;
            let prepare = totals[Stage::Preparation as usize].total_ns as u64;
            let project = totals[Stage::Projection as usize].total_ns as u64;
            let dispatched = stats.borrow().guest_dispatches.values().sum::<u64>() - before;
            assert_eq!(dispatched, if tick == 0 { 0 } else { u64::from(machines) });
            if tick >= 5 {
                writeln!(
                    raw,
                    "{machines},{subscribers},{tick},{elapsed},{plan},{prepare},{project},{dispatched}"
                )?;
                times.push(elapsed);
                plans.push(plan);
                prepares.push(prepare);
                projections.push(project);
            }
        }
        let inventory_count = |id| {
            let world = server.authority().unwrap().world();
            let entity = world
                .entities()
                .get(world.entities().handle_of(id).unwrap())
                .unwrap();
            let inventory = torse_inventory::Inventory::from_fields(
                entity
                    .components
                    .component(&torse_inventory::schema())
                    .unwrap(),
                &catalog,
            )
            .unwrap();
            inventory
                .slots()
                .iter()
                .flatten()
                .map(|s| s.count)
                .sum::<u32>()
        };
        let input = inventory_count(input_id);
        let output = inventory_count(output_id);
        assert_eq!(output, 6 * machines);
        assert_eq!(input + output, 64);
        let stats = stats.borrow();
        assert_eq!(stats.traps.values().sum::<u64>(), 0);
        assert_eq!(stats.rejections.values().sum::<u64>(), 0);
        let average = |v: &Vec<u64>| v.iter().sum::<u64>() / v.len() as u64;
        writeln!(
            summary,
            "{machines},{subscribers},{},{},{},{},{},{input},{output}",
            average(&times),
            average(&plans),
            average(&prepares),
            average(&projections),
            stats.guest_dispatches.values().sum::<u64>()
        )?;
        println!(
            "machines={machines} subscribers={subscribers} drive_mean_us={} planning_mean_us={}",
            average(&times) / 1000,
            average(&plans) / 1000
        );
    }
    Ok(())
}
