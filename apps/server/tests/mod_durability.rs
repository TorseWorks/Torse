//! Durable Mod assembly acceptance: the machine survives restarts, settled
//! work never re-runs the guest, failures leave no partial changes, and one
//! package failing does not stop the others. Requires the `mods` feature and
//! builds the machine guest through cargo.

#![cfg(feature = "mods")]

mod support;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use support::Directory;
use torse_inventory::{Inventory, ItemCatalog, ItemStack};
use torse_server::execution::{
    DurableRule, ExecutionHost, ExecutionModel, ExecutionPrepared, ExecutionRecovery, WorkPlan,
};
use torse_server::{
    CommitStep, FileServer, HostAuthority, NoReplicas, PreparedChange, Server, ServerState,
    WorldDirectory, bootstrap as base, mods,
};
use torse_sim::{SystemSpec, WorldAuthority};
use torse_types::*;
use torse_world::ComponentValue;

fn machine_dir() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.pop();
    dir.pop();
    dir.join("examples").join("mods").join("machine")
}

fn workspace_root() -> PathBuf {
    let mut dir = machine_dir();
    dir.pop();
    dir.pop();
    dir.pop();
    dir
}

fn catalog() -> ItemCatalog {
    ItemCatalog::new([(base::key("base:stone"), 64)]).expect("catalog")
}

/// A second package cloning the machine content under another namespace; its
/// guest binary resolves foreign schemas, so every plan settles as a
/// rejection while the primary machine keeps running.
fn clone_package(id: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = workspace_root()
        .join(".work/mod-packages")
        .join(format!("{id}-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(dir.join("content")).expect("clone content dir");
    let manifest = format!(
        "id = \"{id}\"\nversion = \"0.1.0\"\napi = \"0.3\"\nside = \"server\"\npermissions = [\"inventory:permission/transfer\", \"inventory:permission/read\"]\n\n[server]\nentry = \"logic.wasm\"\n\n[build]\ncrate = \"torse-mod-machine\"\nworkspace = \"{}\"\n",
        workspace_root()
            .display()
            .to_string()
            .replace(std::path::MAIN_SEPARATOR, "/")
    );
    std::fs::write(dir.join("mod.toml"), manifest).expect("clone manifest");
    let content = std::fs::read_to_string(machine_dir().join("content").join("machine.toml"))
        .expect("machine content");
    std::fs::write(
        dir.join("content").join("machine.toml"),
        content.replace("machine:", &format!("{id}:")),
    )
    .expect("clone content");
    dir
}

fn assemble(
    dirs: &[PathBuf],
    stock_input: bool,
    spin: &[String],
) -> (mods::ModWorld, Rc<RefCell<mods::ModStats>>) {
    let world = mods::mod_world(dirs, stock_input, spin).expect("mod world assembles");
    let stats = Rc::clone(&world.stats);
    (world, stats)
}

fn create(dir: &Directory, world: mods::ModWorld) -> FileServer<ExecutionHost, base::Codec> {
    let compatibility = world.world.coordinator().compatibility();
    let config = base::file_policy();
    FileServer::create_file(
        dir.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        config,
        |binding| {
            ExecutionHost::new(
                binding,
                world.world,
                world.actions,
                world.rules,
                base::execution_limits(),
            )
        },
    )
    .unwrap()
}

fn reopen(
    dir: &Directory,
    dirs: &[PathBuf],
    stock: bool,
) -> (
    FileServer<ExecutionHost, base::Codec>,
    Rc<RefCell<mods::ModStats>>,
) {
    reopen_with_spin(dir, dirs, stock, &[])
}

fn reopen_with_spin(
    dir: &Directory,
    dirs: &[PathBuf],
    stock: bool,
    spin: &[String],
) -> (
    FileServer<ExecutionHost, base::Codec>,
    Rc<RefCell<mods::ModStats>>,
) {
    let (world, stats) = assemble(dirs, stock, spin);
    let compatibility = world.world.coordinator().compatibility();
    let config = base::file_policy();
    let server = FileServer::open_file(
        dir.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        config,
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                world.world.registry(),
                world.world.schemas(),
                world.actions,
                world.rules,
                base::execution_limits(),
            ))
        },
    )
    .unwrap();
    (server, stats)
}

fn tick(server: &mut FileServer<ExecutionHost, base::Codec>) {
    server.drive_tick(&mut NoReplicas, 1024).unwrap();
}

fn world_of(server: &FileServer<ExecutionHost, base::Codec>) -> &WorldAuthority {
    server.authority().unwrap().world()
}

fn progress(world: &WorldAuthority, entity: EntityId, schema: &str) -> u32 {
    let handle = world.entities().handle_of(entity).expect("entity");
    match world
        .entities()
        .get(handle)
        .expect("entity")
        .components
        .component(&base::key(schema))
        .expect("machine component")
        .get(&base::key(&format!(
            "{}:field/progress",
            schema.split(':').next().unwrap()
        ))) {
        Some(ComponentValue::U32(value)) => *value,
        other => panic!("unexpected progress: {other:?}"),
    }
}

fn slot0(world: &WorldAuthority, entity: EntityId) -> Option<ItemStack> {
    let handle = world.entities().handle_of(entity).expect("entity");
    let fields = world
        .entities()
        .get(handle)
        .expect("entity")
        .components
        .component(&torse_inventory::schema())
        .expect("inventory");
    let inventory = Inventory::from_fields(fields, &catalog()).expect("inventory decodes");
    inventory.slots()[0].clone()
}

#[test]
fn machine_survives_restart_and_keeps_counts_correct() {
    let dir = Directory::new();
    let (mut world, _stats) = assemble(&[machine_dir()], true, &[]);
    let machine = world.machines[0].1;
    let input = world.input;
    let output = world.output;
    let stack = ItemStack {
        item: base::key("base:stone"),
        count: 1,
        data: [
            (base::key("test:durability"), ComponentValue::U32(23)),
            (
                base::key("extension:unknown"),
                ComponentValue::List(vec![
                    ComponentValue::Text("retained after reopen".into()),
                    ComponentValue::U32(7),
                ]),
            ),
        ]
        .into(),
    };
    let mut stock = Inventory::new(2).unwrap();
    stock.set(0, Some(stack.clone()), &catalog()).unwrap();
    let schemas = world.world.schemas().clone();
    let handle = world.world.entities().handle_of(input).unwrap();
    world
        .world
        .entities_mut()
        .components_mut(handle)
        .unwrap()
        .insert(&schemas, &torse_inventory::schema(), stock.fields())
        .unwrap();
    let mut server = create(&dir, world);

    // Stop with progress three. Completion must reset progress, transfer the
    // item, and register the next wakeup in one durable settlement.
    for _ in 0..4 {
        tick(&mut server);
    }
    assert_eq!(
        progress(world_of(&server), machine, "machine:schema/state"),
        3
    );
    assert_eq!(
        slot0(world_of(&server), input),
        Some(stack.clone()),
        "input stays intact until the completion task"
    );
    assert_eq!(slot0(world_of(&server), output), None);

    // Reopen in the middle of processing, then complete the atomic task.
    drop(server);
    let (mut server, _stats) = reopen(&dir, &[machine_dir()], true);
    tick(&mut server);
    assert_eq!(
        slot0(world_of(&server), input),
        None,
        "input emptied by resumed transfer"
    );
    assert_eq!(
        slot0(world_of(&server), output),
        Some(stack.clone()),
        "output received the resumed transfer"
    );
    assert_eq!(
        progress(world_of(&server), machine, "machine:schema/state"),
        0
    );

    // A full second cycle runs against an emptied input: the world holds one
    // stone, so the second transfer rejects and counts stay conserved.
    for _ in 0..4 {
        tick(&mut server);
    }
    assert_eq!(
        slot0(world_of(&server), output),
        Some(stack.clone()),
        "no duplication across restart and two cycles"
    );
    assert_eq!(slot0(world_of(&server), input), None);
    assert!(
        progress(world_of(&server), machine, "machine:schema/state") == 3,
        "a rejected transfer preserves progress and schedules no new wakeup"
    );
    drop(server);
    let (server, _) = reopen(&dir, &[machine_dir()], true);
    assert_eq!(slot0(world_of(&server), output), Some(stack));
    assert_eq!(slot0(world_of(&server), input), None);
}

#[test]
fn settled_tasks_do_not_rerun_the_guest_after_recovery() {
    let dir = Directory::new();
    let (world, stats) = assemble(&[machine_dir()], true, &[]);
    let mut server = create(&dir, world);
    for _ in 0..5 {
        tick(&mut server);
    }
    assert_eq!(
        stats.borrow().guest_dispatches.get("machine"),
        Some(&4),
        "four wakeups dispatched before the exit"
    );
    drop(server);

    // Recovery applies the complete group without calling the guest. Only
    // the next new wakeup dispatches it.
    let (mut server, stats) = reopen(&dir, &[machine_dir()], true);
    assert!(stats.borrow().guest_dispatches.is_empty());
    tick(&mut server);
    assert_eq!(
        stats.borrow().guest_dispatches.get("machine"),
        Some(&1),
        "recovery replays settled work without dispatching the guest"
    );
}

#[test]
fn transfer_rejection_leaves_no_partial_changes() {
    let dir = Directory::new();
    let (world, stats) = assemble(&[machine_dir()], false, &[]);
    let machine = world.machines[0].1;
    let input = world.input;
    let output = world.output;
    let mut server = create(&dir, world);

    for _ in 0..7 {
        tick(&mut server);
    }
    assert!(
        *stats
            .borrow()
            .transfer_rejections
            .get("machine")
            .unwrap_or(&0)
            >= 1,
        "empty input must reject the transfer plan"
    );
    assert_eq!(slot0(world_of(&server), input), None);
    assert_eq!(slot0(world_of(&server), output), None);
    assert!(
        progress(world_of(&server), machine, "machine:schema/state") == 3,
        "the rejected completion cannot reset progress"
    );
    assert!(
        server.authority().unwrap().runtime().events.is_empty(),
        "rejected plans do not schedule their wakeup"
    );
}

#[test]
fn trapped_guest_parks_bounded_without_side_effects() {
    let dir = Directory::new();
    let (world, stats) = assemble(&[machine_dir()], true, &[String::from("machine")]);
    let machine = world.machines[0].1;
    let input = world.input;
    let output = world.output;
    let mut server = create(&dir, world);

    for _ in 0..18 {
        tick(&mut server);
    }
    assert_eq!(stats.borrow().traps.get("machine"), Some(&3));
    drop(server);
    let (mut server, stats) =
        reopen_with_spin(&dir, &[machine_dir()], true, &[String::from("machine")]);
    for _ in 0..64 {
        tick(&mut server);
    }
    let stats = stats.borrow();
    assert_eq!(
        stats.traps.get("machine"),
        Some(&5),
        "the persisted attempt count limits total calls to eight"
    );
    assert_eq!(stats.parks.get("machine"), Some(&1), "parked after budget");
    assert_eq!(stats.guest_dispatches.get("machine"), Some(&5));
    drop(stats);
    assert_eq!(
        progress(world_of(&server), machine, "machine:schema/state"),
        0
    );
    assert_eq!(
        slot0(world_of(&server), input),
        Some(ItemStack {
            data: Default::default(),
            item: base::key("base:stone"),
            count: 1,
        }),
        "a trapped guest never moves items"
    );
    assert_eq!(slot0(world_of(&server), output), None);
    assert!(
        world_of(&server).seq().0 > 0,
        "the server keeps committing barrier records while the package is parked"
    );
    drop(server);
    let (mut server, stats) = reopen(&dir, &[machine_dir()], true);
    for _ in 0..3 {
        tick(&mut server);
    }
    assert!(
        stats.borrow().guest_dispatches.is_empty(),
        "a parked package stays parked across restart"
    );
}

#[test]
fn failing_package_does_not_stop_the_others() {
    let dir = Directory::new();
    let clone = clone_package("machine2");
    let dirs = vec![machine_dir(), clone.clone()];
    let (world, stats) = assemble(&dirs, true, &[String::from("machine2")]);
    let primary = world.machines[0].1;
    let input = world.input;
    let output = world.output;
    let mut server = create(&dir, world);

    for _ in 0..7 {
        tick(&mut server);
    }
    let stats = stats.borrow();
    assert!(
        *stats.traps.get("machine2").unwrap_or(&0) >= 1,
        "the spinning clone traps without disturbing the phase"
    );
    drop(stats);

    // The primary machine completes its cycle with correct counts.
    assert_eq!(slot0(world_of(&server), input), None);
    assert_eq!(
        slot0(world_of(&server), output),
        Some(ItemStack {
            data: Default::default(),
            item: base::key("base:stone"),
            count: 1,
        })
    );
    assert!(
        progress(world_of(&server), primary, "machine:schema/state") >= 1,
        "the primary machine keeps cycling beside a trapped package"
    );
    let _ = std::fs::remove_dir_all(clone);
}

#[test]
fn task_budget_shortfall_commits_nothing_and_recovers() {
    let dir = Directory::new();
    let clone = clone_package("budgetmachine");
    let dirs = vec![machine_dir(), clone.clone()];
    let (world, _stats) = assemble(&dirs, true, &[]);
    let machine = world
        .machines
        .iter()
        .find(|(name, _)| name == "machine")
        .unwrap()
        .1;
    let compatibility = world.world.coordinator().compatibility();
    let config = base::file_policy();
    let mut limits = base::execution_limits();
    limits.phase_tasks = 1;
    let mut server = FileServer::create_file(
        dir.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        config,
        |binding| ExecutionHost::new(binding, world.world, world.actions, world.rules, limits),
    )
    .unwrap();

    // Both packages arm in the same phase; collecting their two tasks must
    // fail before publishing either task when capacity is one.
    assert!(
        server.drive_tick(&mut NoReplicas, 1024).is_err(),
        "budget shortfall must fault the tick"
    );
    drop(server);

    // Entity identities are deterministic: input 1, output 2, machine 3.
    let (mut server, _stats) = reopen(&dir, &dirs, true);
    let (input, output) = (EntityId(1), EntityId(2));
    assert_eq!(
        progress(world_of(&server), machine, "machine:schema/state"),
        0,
        "no partial progress survived the shortfall"
    );
    assert_eq!(
        slot0(world_of(&server), input),
        Some(ItemStack {
            data: Default::default(),
            item: base::key("base:stone"),
            count: 1,
        }),
        "the stone was not moved or duplicated"
    );
    assert_eq!(slot0(world_of(&server), output), None);

    for _ in 0..5 {
        tick(&mut server);
    }
    assert_eq!(
        slot0(world_of(&server), output),
        Some(ItemStack {
            data: Default::default(),
            item: base::key("base:stone"),
            count: 1,
        }),
        "recovered execution completes the cycle"
    );
    drop(server);
    let _ = std::fs::remove_dir_all(clone);
}

fn completion_pending(host: &ExecutionHost) -> bool {
    host.runtime().work.as_ref().is_some_and(|work| {
        work.tasks
            .get(work.next as usize)
            .and_then(|task| task.transaction.as_ref())
            .is_some_and(|group| group.members.len() == 2)
    })
}

#[test]
fn competing_machines_reject_the_whole_losing_completion() {
    let dir = Directory::new();
    let (mut world, _) = assemble(&[machine_dir()], true, &[]);
    let first = world.machines[0].1;
    let schema = base::key("machine:schema/state");
    let original = world.world.entities().handle_of(first).unwrap();
    let fields = world
        .world
        .entities()
        .get(original)
        .unwrap()
        .components
        .component(&schema)
        .unwrap()
        .clone();
    let schemas = world.world.schemas().clone();
    let other = world
        .world
        .entities_mut()
        .spawn(torse_world::Position::new(5.0, 2.0, 2.0))
        .unwrap();
    world
        .world
        .entities_mut()
        .components_mut(other)
        .unwrap()
        .insert(&schemas, &schema, fields)
        .unwrap();
    let second = world.world.entities().get(other).unwrap().id;
    let (input, output) = (world.input, world.output);
    let mut server = create(&dir, world);
    for _ in 0..5 {
        tick(&mut server);
    }
    assert_eq!(
        progress(world_of(&server), first, "machine:schema/state"),
        0
    );
    assert_eq!(
        progress(world_of(&server), second, "machine:schema/state"),
        3
    );
    assert_eq!(slot0(world_of(&server), input), None);
    assert_eq!(slot0(world_of(&server), output).unwrap().count, 1);
    assert_eq!(
        server.authority().unwrap().runtime().events.len(),
        1,
        "only the winner schedules a new wakeup"
    );
    drop(server);
    let (server, _) = reopen(&dir, &[machine_dir()], true);
    assert_eq!(
        progress(world_of(&server), second, "machine:schema/state"),
        3
    );
    assert_eq!(slot0(world_of(&server), output).unwrap().count, 1);
}

struct InvalidCompletion(Box<dyn DurableRule>);
impl DurableRule for InvalidCompletion {
    fn spec(&self) -> SystemSpec {
        self.0.spec()
    }
    fn plan(&self, world: &WorldAuthority, tick: Tick) -> Result<Vec<WorkPlan>, Fault> {
        self.0.plan(world, tick)
    }
    fn event(
        &self,
        world: &WorldAuthority,
        tick: Tick,
        event: &torse_sim::durable::TimedEvent,
    ) -> Result<WorkPlan, Fault> {
        let mut plan = self.0.event(world, tick, event)?;
        if let Some(group) = &mut plan.transaction
            && group.members.len() == 2
        {
            let machine = group
                .members
                .iter_mut()
                .find(|m| m.source == base::key("machine:module"))
                .unwrap();
            machine.components[0].fields.as_mut().unwrap().clear();
        }
        Ok(plan)
    }
}

#[test]
fn invalid_machine_component_rejects_inventory_and_wakeup_together() {
    let dir = Directory::new();
    let (mut world, _) = assemble(&[machine_dir()], true, &[]);
    let (machine, input, output) = (world.machines[0].1, world.input, world.output);
    world.rules = world
        .rules
        .into_iter()
        .map(|r| Box::new(InvalidCompletion(r)) as Box<dyn DurableRule>)
        .collect();
    let mut server = create(&dir, world);
    for _ in 0..5 {
        tick(&mut server);
    }
    assert_eq!(
        progress(world_of(&server), machine, "machine:schema/state"),
        3
    );
    assert_eq!(slot0(world_of(&server), input).unwrap().count, 1);
    assert_eq!(slot0(world_of(&server), output), None);
    assert!(server.authority().unwrap().runtime().events.is_empty());
    drop(server);
    let (server, _) = reopen(&dir, &[machine_dir()], true);
    assert_eq!(
        progress(world_of(&server), machine, "machine:schema/state"),
        3
    );
    assert_eq!(slot0(world_of(&server), input).unwrap().count, 1);
}

struct CrashHost(ExecutionHost, bool);
impl HostAuthority for CrashHost {
    type Model = ExecutionModel;
    type Prepared = ExecutionPrepared;
    fn binding(&self) -> Binding {
        self.0.binding()
    }
    fn cursor(&self) -> CommitCursor {
        self.0.cursor()
    }
    fn resume_point(&self) -> ResumePoint {
        self.0.resume_point()
    }
    fn is_tick_start(&self, p: ResumePoint) -> bool {
        self.0.is_tick_start(p)
    }
    fn at_tick_boundary(&self) -> bool {
        self.0.at_tick_boundary()
    }
    fn submit(&mut self, r: WorldRequest<torse_sim::durable::Call>) -> Admission {
        self.0.submit(r)
    }
    fn prepare_next(&mut self) -> Result<Option<ExecutionPrepared>, Fault> {
        self.0.prepare_next()
    }
    fn publish(&mut self, p: ExecutionPrepared) -> Result<Publication, Fault> {
        let completion = p.record().changes.world.component_edits.len() == 3;
        let publication = self.0.publish(p)?;
        if self.1 && completion {
            std::process::exit(73);
        }
        Ok(publication)
    }
    fn capture_snapshot(
        &mut self,
        b: OutputBudget,
    ) -> Result<WorldSnapshot<torse_sim::durable::ExecutionSnapshot>, Fault> {
        self.0.capture_snapshot(b)
    }
    fn stop_admission(&mut self, t: Tick) -> Result<(), Fault> {
        self.0.stop_admission(t)
    }
    fn shutdown_ready(&self) -> bool {
        self.0.shutdown_ready()
    }
}

#[test]
fn mod_crash_child() {
    let Ok(path) = std::env::var("TORSE_B9_PATH") else {
        return;
    };
    let mode = std::env::var("TORSE_B9_MODE").unwrap();
    let (world, _) = assemble(&[machine_dir()], true, &[]);
    let output = world.output;
    let compatibility = world.world.coordinator().compatibility();
    let mut server = FileServer::<CrashHost, base::Codec>::create_file(
        WorldDirectory::acquire(path, base::WORLD, 8192).unwrap(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            Ok(CrashHost(
                ExecutionHost::new(
                    binding,
                    world.world,
                    world.actions,
                    world.rules,
                    base::execution_limits(),
                )?,
                mode == "publish",
            ))
        },
    )
    .unwrap();
    for _ in 0..200 {
        server.drive_one(&mut NoReplicas).unwrap();
        let host = &server.authority().unwrap().0;
        if mode == "mid" && host.resume_point().tick == Tick(4) && host.at_tick_boundary() {
            std::process::exit(76);
        }
        if mode == "frozen" && completion_pending(host) {
            std::process::exit(75);
        }
        if mode == "durable" && slot0(host.world(), output).is_some() {
            std::process::exit(74);
        }
    }
    panic!("crash point not reached");
}

#[test]
fn process_exit_recovers_atomic_completion_and_frozen_members_without_guest_replay() {
    for (mode, code) in [
        ("mid", 76),
        ("frozen", 75),
        ("publish", 73),
        ("durable", 74),
    ] {
        let dir = Directory::new();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "mod_crash_child", "--nocapture"])
            .env("TORSE_B9_PATH", &dir.0)
            .env("TORSE_B9_MODE", mode)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(code), "{mode}");
        let (mut server, stats) = reopen(&dir, &[machine_dir()], true);
        assert!(
            stats.borrow().guest_dispatches.is_empty(),
            "replay must not call the guest"
        );
        let completed = mode == "durable";
        assert_eq!(
            progress(world_of(&server), EntityId(3), "machine:schema/state"),
            if completed { 0 } else { 3 }
        );
        assert_eq!(slot0(world_of(&server), EntityId(1)).is_none(), completed);
        assert_eq!(slot0(world_of(&server), EntityId(2)).is_some(), completed);
        if mode == "frozen" || mode == "publish" {
            assert!(completion_pending(server.authority().unwrap()));
            server.drive_one(&mut NoReplicas).unwrap();
            assert!(
                stats.borrow().guest_dispatches.is_empty(),
                "frozen plans restore without guest dispatch"
            );
        }
        tick(&mut server);
        assert_eq!(slot0(world_of(&server), EntityId(1)), None);
        assert_eq!(slot0(world_of(&server), EntityId(2)).unwrap().count, 1);
        drop(server);
        let (server, stats) = reopen(&dir, &[machine_dir()], true);
        assert_eq!(slot0(world_of(&server), EntityId(2)).unwrap().count, 1);
        assert!(stats.borrow().guest_dispatches.is_empty());
    }
}

struct Controlled<J> {
    inner: J,
    blocked: Rc<Cell<u8>>,
    fail: Rc<Cell<bool>>,
    accepted: Rc<Cell<usize>>,
}
impl<J: torse_storage::CommitJournal<ExecutionModel>> torse_storage::CommitJournal<ExecutionModel>
    for Controlled<J>
{
    type Staged = J::Staged;
    fn try_stage(
        &mut self,
        r: &CommitRecord<torse_sim::durable::ExecutionChanges>,
    ) -> Result<Self::Staged, StageError> {
        match self.blocked.get() {
            1 => Err(StageError::Busy),
            2 => Err(StageError::TooLarge),
            _ => self.inner.try_stage(r),
        }
    }
    fn accept(&mut self, s: Self::Staged) -> Result<(), Fault> {
        self.accepted.set(self.accepted.get() + 1);
        self.inner.accept(s)
    }
    fn flush_through(&mut self, c: CommitCursor) -> Result<Durability, Fault> {
        if self.fail.get() {
            Err(Fault::new(
                FaultCode::StorageUnavailable,
                "injected group flush failure",
            ))
        } else {
            self.inner.flush_through(c)
        }
    }
    fn durability(&self) -> Durability {
        self.inner.durability()
    }
}

#[test]
fn journal_rejection_preserves_the_group_and_flush_failure_stops_visibility() {
    for fail_completion in [false, true] {
        let dir = Directory::new();
        let directory = dir.acquire();
        let (world, _) = assemble(&[machine_dir()], true, &[]);
        let binding = Binding {
            world: base::WORLD,
            epoch: directory.epoch(),
            compatibility: world.world.coordinator().compatibility(),
        };
        let policy = base::file_policy();
        let codec = base::Codec::new(base::codec_limits()).unwrap();
        let blocked = Rc::new(Cell::new(0));
        let fail = Rc::new(Cell::new(false));
        let accepted = Rc::new(Cell::new(0));
        let journal = Controlled {
            inner: torse_storage::FileJournal::create(
                directory.root().join("journal.log"),
                binding,
                CommitCursor::initial(base::WORLD),
                codec,
                policy.journal,
            )
            .unwrap(),
            blocked: blocked.clone(),
            fail: fail.clone(),
            accepted: accepted.clone(),
        };
        std::fs::create_dir(directory.root().join("checkpoints")).unwrap();
        let checkpoints = torse_storage::Checkpoints::open(
            binding,
            codec,
            policy.checkpoint,
            torse_storage::FileCheckpointBackend::open(directory.root().join("checkpoints"))
                .unwrap(),
        )
        .unwrap();
        let host = ExecutionHost::new(
            binding,
            world.world,
            world.actions,
            world.rules,
            base::execution_limits(),
        )
        .unwrap();
        let mut server =
            Server::initialize(directory, host, journal, checkpoints, policy.server).unwrap();
        for _ in 0..200 {
            if completion_pending(server.authority().unwrap()) {
                break;
            }
            server.drive_one(&mut NoReplicas).unwrap();
        }
        assert!(completion_pending(server.authority().unwrap()));
        let before_seq = server.authority().unwrap().cursor();
        let before_runtime = server.authority().unwrap().runtime().clone();
        for mode in [1, 2] {
            blocked.set(mode);
            assert!(matches!(
                server.drive_one(&mut NoReplicas).unwrap(),
                CommitStep::Blocked(_)
            ));
            let host = server.authority().unwrap();
            assert_eq!(host.cursor(), before_seq);
            assert_eq!(host.runtime(), &before_runtime);
            assert_eq!(
                progress(host.world(), EntityId(3), "machine:schema/state"),
                3
            );
            assert_eq!(slot0(host.world(), EntityId(1)).unwrap().count, 1);
            assert_eq!(slot0(host.world(), EntityId(2)), None);
        }
        blocked.set(0);
        let previous = accepted.get();
        fail.set(fail_completion);
        if fail_completion {
            assert!(server.drive_one(&mut NoReplicas).is_err());
            assert_eq!(server.state(), ServerState::Faulted);
            assert!(server.authority().is_err());
            drop(server);
            let (mut server, _) = reopen(&dir, &[machine_dir()], true);
            assert!(completion_pending(server.authority().unwrap()));
            assert_eq!(
                progress(world_of(&server), EntityId(3), "machine:schema/state"),
                3
            );
            assert_eq!(slot0(world_of(&server), EntityId(1)).unwrap().count, 1);
            assert_eq!(slot0(world_of(&server), EntityId(2)), None);
            tick(&mut server);
            assert_eq!(slot0(world_of(&server), EntityId(2)).unwrap().count, 1);
            continue;
        }
        // Exactly one accepted record settles both component sources and the
        // new event; no independently visible inventory settlement remains.
        server.drive_one(&mut NoReplicas).unwrap();
        assert_eq!(accepted.get(), previous + 1);
        assert_eq!(
            server.authority().unwrap().cursor().seq.0,
            before_seq.seq.0 + 1
        );
        assert_eq!(
            slot0(server.authority().unwrap().world(), EntityId(2))
                .unwrap()
                .count,
            1
        );
        assert_eq!(
            progress(
                server.authority().unwrap().world(),
                EntityId(3),
                "machine:schema/state"
            ),
            0
        );
        assert_eq!(
            server.authority().unwrap().runtime().next_event,
            before_runtime.next_event + 1
        );
    }
}

// B10: consume the public UI contract through the real message and replica path.
use torse_codec::wire;
use torse_mod_api::ui::{MachineView, machine as machine_ui};
use torse_net::{MemoryTransport, ReplicaMirror, Transport, TransportLimits};
use torse_server::network::{ClientMessage, NetworkHub, PeerPolicy, ServerMessage};
use torse_sim::action::{ActionTarget, ActorContext};
use torse_sim::durable::{Call, ReplyStatus};

fn ui_actor(machine: EntityId, input: EntityId, output: EntityId) -> ActorContext {
    ActorContext::new(base::key("test:operator"))
        .permission(base::key("inventory:permission/read"))
        .permission(base::key("inventory:permission/transfer"))
        .permission(base::key("world:permission/edit"))
        .target(ActionTarget::Entity(machine))
        .target(ActionTarget::Entity(input))
        .target(ActionTarget::Entity(output))
        .section(SectionPos { x: 0, y: 0, z: 0 })
}
fn ui_send(client: &mut MemoryTransport, message: ClientMessage) {
    let mut bytes = Vec::new();
    wire::encode(&message, &mut bytes, base::budget(), base::codec_limits()).unwrap();
    client.send(bytes).unwrap();
}
fn ui_receive(client: &mut MemoryTransport) -> Vec<ServerMessage> {
    let mut messages = Vec::new();
    while let Some(bytes) = client.receive().unwrap() {
        messages.push(wire::decode(&bytes, base::budget(), base::codec_limits()).unwrap());
    }
    messages
}
fn ui_connect(
    server: &mut base::Files,
    hub: &mut NetworkHub<MemoryTransport>,
    actor: ActorContext,
) -> (u128, MemoryTransport, ReplicaMirror) {
    let (host, mut client) = MemoryTransport::pair(TransportLimits {
        frame: 8_388_608,
        queued_frames: 32,
        queued_bytes: 16_777_216,
        poll_bytes: 262_144,
    })
    .unwrap();
    let id = hub
        .add(
            host,
            PeerPolicy {
                actor,
                scope: base::scope(),
            },
            server.authority().unwrap().resume_point().tick,
        )
        .unwrap();
    ui_send(
        &mut client,
        ClientMessage::Hello {
            version: torse_protocol::PROTOCOL_VERSION,
            compatibility: server.binding().compatibility,
        },
    );
    hub.poll(server).unwrap();
    ui_receive(&mut client);
    ui_send(
        &mut client,
        ClientMessage::Subscribe {
            sections: base::scope().sections.into_iter().collect(),
        },
    );
    hub.poll(server).unwrap();
    let mirror = ui_receive(&mut client)
        .into_iter()
        .find_map(|message| match message {
            ServerMessage::Seed(seed) => Some(ReplicaMirror::from_seed(seed).unwrap()),
            _ => None,
        })
        .expect("new authorized seed");
    (id, client, mirror)
}
fn ui_view(mirror: &ReplicaMirror, machine: EntityId) -> Option<MachineView> {
    MachineView::rebuild(&machine_ui(), machine, |id, schema| {
        mirror
            .state
            .entities
            .iter()
            .find(|entity| entity.id == id)?
            .components
            .get(schema)
    })
}
fn ui_apply(client: &mut MemoryTransport, mirror: &mut ReplicaMirror) -> Vec<ReplyStatus> {
    let mut statuses = Vec::new();
    for message in ui_receive(client) {
        match message {
            ServerMessage::Batch(batch) => mirror.apply(batch).unwrap(),
            ServerMessage::Result { reply, .. } => statuses.push(reply.status),
            _ => {}
        }
    }
    statuses
}

#[test]
fn machine_ui_two_clients_authorization_dedup_and_new_baseline() {
    let dir = Directory::new();
    let (world, _) = assemble(&[machine_dir()], true, &[]);
    let (machine, input, output) = (world.machines[0].1, world.input, world.output);
    let mut server = create(&dir, world);
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let actor = ui_actor(machine, input, output);
    let (owner_id, mut owner, mut first) = ui_connect(&mut server, &mut hub, actor.clone());
    let (_, mut second_client, mut second) = ui_connect(&mut server, &mut hub, actor.clone());
    let (_, mut guest, mut private) = ui_connect(
        &mut server,
        &mut hub,
        ActorContext::new(base::key("test:visitor")),
    );
    let partial_actor =
        ActorContext::new(base::key("test:partial")).target(ActionTarget::Entity(machine));
    let (_, _, partial) = ui_connect(&mut server, &mut hub, partial_actor);
    let partial = ui_view(&partial, machine).unwrap();
    assert!(partial.input.is_none() && partial.output.is_none());
    assert!(partial.transfer_one().is_none());
    assert!(ui_view(&private, machine).is_none());
    assert!(
        private
            .state
            .entities
            .iter()
            .all(|entity| entity.components.is_empty())
    );
    assert_eq!(ui_view(&first, machine), ui_view(&second, machine));
    let reader_actor = ActorContext::new(base::key("test:reader"))
        .target(ActionTarget::Entity(machine))
        .target(ActionTarget::Entity(input))
        .target(ActionTarget::Entity(output));
    let (_, mut reader, mut reader_mirror) = ui_connect(&mut server, &mut hub, reader_actor);
    assert_eq!(ui_view(&reader_mirror, machine), ui_view(&first, machine));
    let intent = ui_view(&first, machine).unwrap().transfer_one().unwrap();
    let call = Call {
        action: intent.action,
        version: intent.version,
        target: ActionTarget::Entity(intent.target),
        parameters: intent.parameters,
    };
    ui_send(
        &mut reader,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: call.clone(),
        },
    );
    for _ in 0..2 {
        ui_send(
            &mut owner,
            ClientMessage::Request {
                seq: RequestSeq(1),
                call: call.clone(),
            },
        );
    }
    ui_send(
        &mut guest,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: call.clone(),
        },
    );
    hub.poll(&mut server).unwrap();
    server.drive_tick(&mut hub, 1024).unwrap();
    hub.poll(&mut server).unwrap();
    assert_eq!(
        ui_apply(&mut owner, &mut first),
        vec![ReplyStatus::Committed]
    );
    ui_apply(&mut second_client, &mut second);
    assert_eq!(
        ui_apply(&mut guest, &mut private),
        vec![ReplyStatus::Rejected(RejectCode::PermissionDenied)]
    );
    assert_eq!(ui_view(&first, machine), ui_view(&second, machine));
    assert_eq!(
        ui_apply(&mut reader, &mut reader_mirror),
        vec![ReplyStatus::Rejected(RejectCode::PermissionDenied)]
    );
    let view = ui_view(&first, machine).unwrap();
    assert!(view.input.unwrap().items.is_empty());
    assert_eq!(view.output.unwrap().items[&base::key("base:stone")], 1);
    let position = server.durability().through;
    ui_send(
        &mut owner,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call,
        },
    );
    hub.poll(&mut server).unwrap();
    assert!(ui_receive(&mut owner).iter().any(|message| matches!(
        message,
        ServerMessage::Admission {
            admission: Admission::Duplicate,
            ..
        }
    )));
    assert_eq!(position, server.durability().through);
    server.drive_tick(&mut hub, 1024).unwrap();
    hub.poll(&mut server).unwrap();
    ui_apply(&mut owner, &mut first);
    ui_apply(&mut second_client, &mut second);
    ui_apply(&mut guest, &mut private);
    assert_eq!(ui_view(&first, machine).unwrap().progress, 1);
    assert_eq!(ui_view(&first, machine), ui_view(&second, machine));
    assert!(ui_view(&private, machine).is_none());
    hub.revoke(&mut server, owner_id).unwrap();
    assert!(owner.receive().is_err());
    let (_, _, fresh) = ui_connect(&mut server, &mut hub, actor);
    assert_ne!(first.subscription, fresh.subscription);
    assert_eq!(ui_view(&fresh, machine), ui_view(&second, machine));
    let (_, _, revoked) = ui_connect(
        &mut server,
        &mut hub,
        ActorContext::new(base::key("test:revoked")),
    );
    assert!(ui_view(&revoked, machine).is_none());
    drop(server);
    let (mut server, stats) = reopen(&dir, &[machine_dir()], true);
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let (_, _, recovered) = ui_connect(&mut server, &mut hub, ui_actor(machine, input, output));
    assert_ne!(fresh.binding.epoch, recovered.binding.epoch);
    assert_eq!(ui_view(&fresh, machine), ui_view(&recovered, machine));
    assert!(stats.borrow().guest_dispatches.is_empty());
}

fn block_call(place: bool, block: &str) -> Call {
    Call {
        action: base::key(if place {
            "world:action/place"
        } else {
            "world:action/break"
        }),
        version: 1,
        target: ActionTarget::Block(BlockPos::new(8, 1, 1)),
        parameters: if place {
            [(
                base::key("world:field/block"),
                ComponentValue::Content(base::key(block)),
            )]
            .into()
        } else {
            Default::default()
        },
    }
}
fn block_session(server: &mut base::Files) {
    server
        .register_session(
            Viewer {
                session: SessionId(1),
                actor: ActorContextId(1),
            },
            ui_actor(EntityId(3), EntityId(1), EntityId(2)),
            base::scope(),
        )
        .unwrap();
}
fn block_request(server: &base::Files, seq: u64, call: Call) -> WorldRequest<Call> {
    WorldRequest {
        id: RequestId {
            world: base::WORLD,
            epoch: server.binding().epoch,
            session: SessionId(1),
            seq: RequestSeq(seq),
        },
        actor: ActorContextId(1),
        input: call,
    }
}
fn block_event_count(host: &ExecutionHost) -> usize {
    host.runtime()
        .events
        .iter()
        .filter(|event| {
            matches!(
        event.event.payload.get(&base::key("machine:event/kind")),
        Some(ComponentValue::Text(kind)) if kind.starts_with("block-"))
        })
        .count()
}

#[test]
fn block_notifications_commit_once_and_recover_without_guest_replay() {
    let dir = Directory::new();
    let (world, stats) = assemble(&[machine_dir()], true, &[]);
    let mut server = create(&dir, world);
    block_session(&mut server);
    let place = block_request(&server, 1, block_call(true, "machine:block/casing"));
    assert!(matches!(
        server.submit(copy_request(&place)),
        Admission::Queued { .. }
    ));
    assert_eq!(server.submit(copy_request(&place)), Admission::Duplicate);
    // Both plans see air, but the losing commit must register no notification.
    let conflict = block_request(&server, 2, block_call(true, "machine:block/casing"));
    server.submit(copy_request(&conflict));
    tick(&mut server);
    assert_eq!(block_event_count(server.authority().unwrap()), 1);
    assert_eq!(
        server.receipt(conflict.id).unwrap().unwrap().reply.status,
        ReplyStatus::Rejected(RejectCode::Conflict)
    );
    assert!(stats.borrow().guest_dispatches.is_empty());
    drop(server);
    let (mut server, stats) = reopen(&dir, &[machine_dir()], true);
    assert!(stats.borrow().guest_dispatches.is_empty());
    assert_eq!(block_event_count(server.authority().unwrap()), 1);
    // Freeze simulation notifications, then restart before publishing them.
    loop {
        server.drive_one(&mut NoReplicas).unwrap();
        if server
            .authority()
            .unwrap()
            .runtime()
            .work
            .as_ref()
            .is_some_and(|work| work.tasks.iter().any(|task| task.consume.is_some()))
        {
            break;
        }
    }
    assert_eq!(stats.borrow().guest_dispatches.get("machine"), Some(&2));
    drop(server);
    let (mut server, stats) = reopen(&dir, &[machine_dir()], true);
    tick(&mut server);
    assert!(
        stats.borrow().guest_dispatches.is_empty(),
        "frozen tasks bypass guest"
    );
    assert_eq!(block_event_count(server.authority().unwrap()), 0);
    block_session(&mut server);
    let break_request = block_request(&server, 1, block_call(false, ""));
    server.submit(copy_request(&break_request));
    tick(&mut server);
    assert_eq!(block_event_count(server.authority().unwrap()), 1);
    let broken = server
        .authority()
        .unwrap()
        .runtime()
        .events
        .iter()
        .find(|event| {
            event.event.payload.get(&base::key("machine:event/kind"))
                == Some(&ComponentValue::Text("block-broken".into()))
        })
        .unwrap();
    assert_eq!(
        broken.event.payload[&base::key("machine:event/block")],
        ComponentValue::Content(base::key("machine:block/casing"))
    );
    assert_eq!(
        broken.event.payload[&base::key("machine:event/x")],
        ComponentValue::I64(8)
    );
    assert_eq!(
        broken.event.payload[&base::key("machine:event/values")],
        ComponentValue::List(vec![])
    );
    assert!(
        world_of(&server)
            .store()
            .query(BlockPos::new(8, 1, 1))
            .ready()
            .unwrap()
            .is_empty()
    );
    assert_eq!(server.submit(break_request), Admission::Duplicate);
    tick(&mut server);
    assert_eq!(block_event_count(server.authority().unwrap()), 0);
    drop(server);
    let (server, stats) = reopen(&dir, &[machine_dir()], true);
    assert_eq!(block_event_count(server.authority().unwrap()), 0);
    assert!(stats.borrow().guest_dispatches.is_empty());
}

#[test]
fn unbound_blocks_and_denied_actions_produce_no_mod_notification() {
    let dir = Directory::new();
    let (world, _) = assemble(&[machine_dir()], true, &[]);
    let mut server = create(&dir, world);
    block_session(&mut server);
    let request = block_request(&server, 1, block_call(true, "base:stone"));
    server.submit(request);
    tick(&mut server);
    assert_eq!(block_event_count(server.authority().unwrap()), 0);
    server.disconnect(SessionId(1)).unwrap();
    server
        .register_session(
            Viewer {
                session: SessionId(2),
                actor: ActorContextId(2),
            },
            ActorContext::new(base::key("test:visitor")),
            base::scope(),
        )
        .unwrap();
    let mut request = block_request(&server, 1, block_call(false, ""));
    request.id.session = SessionId(2);
    request.actor = ActorContextId(2);
    server.submit(copy_request(&request));
    tick(&mut server);
    assert_eq!(
        server.receipt(request.id).unwrap().unwrap().reply.status,
        ReplyStatus::Rejected(RejectCode::PermissionDenied)
    );
    assert_eq!(block_event_count(server.authority().unwrap()), 0);
}

fn copy_request(request: &WorldRequest<Call>) -> WorldRequest<Call> {
    WorldRequest {
        id: request.id,
        actor: request.actor,
        input: request.input.clone(),
    }
}

#[test]
fn block_notification_crash_child() {
    let Ok(path) = std::env::var("TORSE_B10_PATH") else {
        return;
    };
    let mode = std::env::var("TORSE_B10_MODE").unwrap();
    let (world, _) = assemble(&[machine_dir()], true, &[]);
    let compatibility = world.world.coordinator().compatibility();
    let mut server = base::Files::create_file(
        WorldDirectory::acquire(path, base::WORLD, 8192).unwrap(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            ExecutionHost::new(
                binding,
                world.world,
                world.actions,
                world.rules,
                base::execution_limits(),
            )
        },
    )
    .unwrap();
    block_session(&mut server);
    let request = block_request(&server, 1, block_call(true, "machine:block/casing"));
    assert!(matches!(server.submit(request), Admission::Queued { .. }));
    for _ in 0..100 {
        server.drive_one(&mut NoReplicas).unwrap();
        let host = server.authority().unwrap();
        let action_frozen = host.runtime().work.as_ref().is_some_and(|work| {
            work.tasks
                .iter()
                .any(|task| task.request.is_some() && !task.schedule.is_empty())
        });
        let event_frozen = host
            .runtime()
            .work
            .as_ref()
            .is_some_and(|work| work.tasks.iter().any(|task| task.consume.is_some()));
        if (mode == "action-frozen" && action_frozen)
            || (mode == "committed" && block_event_count(host) == 1)
            || (mode == "event-frozen" && event_frozen)
            || (mode == "event-done" && host.resume_point().tick == Tick(2))
        {
            std::process::exit(71);
        }
    }
    panic!("block crash point not reached");
}

#[test]
fn process_exit_keeps_block_and_notification_in_one_durable_settlement() {
    for mode in ["action-frozen", "committed", "event-frozen", "event-done"] {
        let dir = Directory::new();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "block_notification_crash_child", "--nocapture"])
            .env("TORSE_B10_PATH", &dir.0)
            .env("TORSE_B10_MODE", mode)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(71), "{mode}");
        let (mut server, stats) = reopen(&dir, &[machine_dir()], true);
        assert!(stats.borrow().guest_dispatches.is_empty());
        let block = world_of(&server)
            .store()
            .query(BlockPos::new(8, 1, 1))
            .ready()
            .unwrap();
        assert_eq!(block.is_empty(), mode == "action-frozen");
        assert_eq!(
            block_event_count(server.authority().unwrap()),
            usize::from(mode == "committed" || mode == "event-frozen")
        );
        while server.authority().unwrap().resume_point().tick < Tick(2) {
            tick(&mut server);
        }
        assert_eq!(block_event_count(server.authority().unwrap()), 0);
        assert!(
            !world_of(&server)
                .store()
                .query(BlockPos::new(8, 1, 1))
                .ready()
                .unwrap()
                .is_empty()
        );
        if mode == "event-frozen" || mode == "event-done" {
            assert!(
                stats.borrow().guest_dispatches.is_empty(),
                "frozen/replayed guest: {mode}"
            );
        } else {
            assert_eq!(
                stats.borrow().guest_dispatches.get("machine"),
                Some(&2),
                "{mode}"
            );
        }
    }
}
