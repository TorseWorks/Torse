//! Real native item-body planning must preserve serial durable behavior.
mod support;
#[path = "../examples/drop_baseline/workload.rs"]
mod workload;

use support::Directory;
use torse_items::drops;
use torse_server::{bootstrap as base, execution::*, *};
use torse_sim::Phase;

fn configure(server: &mut base::Files, workers: usize) {
    if workers != 0 {
        server
            .set_parallel_planning(Some(PlanningConfig {
                workers,
                max_in_flight: 4,
            }))
            .unwrap();
    }
}
fn create(
    directory: &Directory,
    workers: usize,
    clustered: bool,
    runtime_bytes: u64,
) -> base::Files {
    let (world, actions, catalog) = workload::world(64, clustered);
    let compatibility = world.coordinator().compatibility();
    let mut limits = base::execution_limits();
    limits.runtime.max_bytes = runtime_bytes;
    let mut server = base::Files::create_file(
        directory.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| ExecutionHost::new(binding, world, actions, workload::rules(catalog), limits),
    )
    .unwrap();
    configure(&mut server, workers);
    server
}

#[test]
fn real_drop_plans_journals_and_worlds_match_with_merges_and_instance_data() {
    for clustered in [false, true] {
        let mut reference = None;
        for workers in [0, 1, 4] {
            let directory = Directory::new();
            let mut server = create(&directory, workers, clustered, 2_097_152);
            for _ in 0..12 {
                server.drive_tick(&mut NoReplicas, 4096).unwrap();
            }
            let stats = server.authority().unwrap().native_planning_stats();
            if workers == 0 {
                assert_eq!(stats.batches, 0);
            } else {
                assert!(stats.jobs > 0);
                assert_eq!(stats.peak_lanes, workers);
            }
            let world = server.authority().unwrap().world();
            assert_eq!(world.entities().len(), if clustered { 62 } else { 66 });
            let catalog = torse_items::fixture_catalog(world.registry())
                .unwrap()
                .with_world_drops();
            let count: u32 = world
                .entities()
                .iter()
                .filter_map(|(_, e)| drops::DropState::read(e, &catalog).ok())
                .map(|state| state.stack.count)
                .sum();
            assert_eq!(count, 128);
            let snapshot = workload::snapshot(server.authority().unwrap());
            drop(server);
            let journal = std::fs::read(directory.0.join("journal.log")).unwrap();
            let result = (snapshot, journal);
            if let Some(expected) = &reference {
                assert_eq!(&result, expected);
            }
            reference = Some(result);
        }
    }
}

#[test]
fn small_work_budget_falls_back_without_changing_frozen_plans() {
    let mut reference = None;
    for workers in [0, 4] {
        let directory = Directory::new();
        let mut server = create(&directory, workers, false, 200_000);
        server.drive_tick(&mut NoReplicas, 4096).unwrap();
        let stats = server.authority().unwrap().native_planning_stats();
        assert_eq!(stats.batches, 0);
        assert_eq!(stats.budget_fallbacks > 0, workers != 0);
        let state = workload::snapshot(server.authority().unwrap());
        if let Some(expected) = &reference {
            assert_eq!(&state, expected);
        }
        reference = Some(state);
    }
}

#[test]
fn partially_frozen_drop_batch_recovers_under_a_different_worker_setting() {
    let mut reference = None;
    for cut in [0, 1, 17, 61, 62] {
        let directory = Directory::new();
        let mut server = create(&directory, 4, false, 2_097_152);
        let mut found = false;
        for _ in 0..100 {
            server.drive_one(&mut NoReplicas).unwrap();
            let runtime = server.authority().unwrap().runtime();
            if let Some(work) = &runtime.work
                && usize::from(server.authority().unwrap().resume_point().next_phase)
                    == Phase::Simulation.index()
                && work.next as usize == cut
            {
                found = true;
                break;
            }
        }
        assert!(found, "cut point {cut} was not reached in the first tick");
        let before = server.authority().unwrap().cursor();
        drop(server);
        let (world, actions, _) = workload::world(0, false);
        let compatibility = world.coordinator().compatibility();
        let mut server = base::Files::open_file(
            directory.acquire(),
            compatibility,
            base::Codec::new(base::codec_limits()).unwrap(),
            base::file_policy(),
            |binding| {
                Ok(ExecutionRecovery::new(
                    binding,
                    world.registry(),
                    world.schemas(),
                    actions,
                    vec![Box::new(FrozenOnly)],
                    base::execution_limits(),
                ))
            },
        )
        .unwrap();
        assert_eq!(server.authority().unwrap().cursor(), before);
        configure(&mut server, 1);
        server.drive_tick(&mut NoReplicas, 4096).unwrap();
        assert_eq!(
            server.authority().unwrap().native_planning_stats().batches,
            0
        );
        let state = workload::snapshot(server.authority().unwrap());
        if let Some(expected) = &reference {
            assert_eq!(&state, expected);
        }
        reference = Some(state);
    }
}

struct FrozenOnly;
impl DurableRule for FrozenOnly {
    fn spec(&self) -> torse_sim::SystemSpec {
        torse_sim::SystemSpec {
            id: base::key("items:system/physics"),
            phase: Phase::Simulation,
            rule_priority: 10,
            before: vec![],
            after: vec![],
        }
    }
    fn plan(
        &self,
        _: &torse_sim::WorldAuthority,
        _: torse_types::Tick,
    ) -> Result<Vec<WorkPlan>, torse_types::Fault> {
        panic!("recovery must not replan frozen body physics")
    }
}

#[test]
fn activation_custom_colliders_unloaded_sweeps_and_small_batches_match_serial() {
    use torse_sim::durable::FrozenTask;
    use torse_types::{Binding, Tick, WorldEpoch};
    use torse_world::{EntityCollider, Position};
    for case in 0..4 {
        let mut reference: Option<Vec<FrozenTask>> = None;
        for workers in [0, 4] {
            let (mut world, actions, catalog) =
                workload::world(if case == 3 { 4 } else { 32 }, false);
            let handle = world
                .entities()
                .iter()
                .find(|(_, entity)| entity.components.component(&drops::schema()).is_some())
                .unwrap()
                .0;
            let schemas = world.schemas().clone();
            if case <= 1 {
                world
                    .entities_mut()
                    .components_mut(handle)
                    .unwrap()
                    .insert(
                        &schemas,
                        &torse_world::collider_schema(),
                        if case == 0 {
                            EntityCollider::Disabled
                        } else {
                            EntityCollider::Box { half: [250_000; 3] }
                        }
                        .fields(),
                    )
                    .unwrap();
            } else if case == 2 {
                world
                    .entities_mut()
                    .move_to(handle, Position::new(1.5, 15.0, 1.5))
                    .unwrap();
                let entity = world.entities().get(handle).unwrap();
                let mut state = drops::DropState::read(entity, &catalog).unwrap();
                state.velocity = [0, 64_000_000, 0];
                world
                    .entities_mut()
                    .components_mut(handle)
                    .unwrap()
                    .insert(&schemas, &drops::schema(), state.fields())
                    .unwrap();
            }
            let binding = Binding {
                world: base::WORLD,
                epoch: WorldEpoch(1),
                compatibility: world.coordinator().compatibility(),
            };
            let mut host = ExecutionHost::new(
                binding,
                world,
                actions,
                workload::rules(catalog),
                base::execution_limits(),
            )
            .unwrap();
            if workers != 0 {
                host.set_parallel_planning(Some(PlanningConfig {
                    workers,
                    max_in_flight: 4,
                }))
                .unwrap();
            }
            for _ in 0..16 {
                if usize::from(host.resume_point().next_phase) == Phase::Simulation.index()
                    && host.runtime().work.is_some()
                {
                    break;
                }
                let prepared = host.prepare_next().unwrap().unwrap();
                host.publish(prepared).unwrap();
            }
            assert_eq!(host.resume_point().tick, Tick(0));
            let tasks = host.runtime().work.as_ref().unwrap().tasks.to_vec();
            if let Some(expected) = &reference {
                assert_eq!(&tasks, expected);
            }
            reference = Some(tasks);
            assert_eq!(
                host.native_planning_stats().batches > 0,
                case == 2 && workers != 0
            );
        }
    }
}
