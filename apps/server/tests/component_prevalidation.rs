//! Owned component tokens preserve serial records, backpressure and file recovery.
mod support;
#[allow(dead_code)]
#[path = "../examples/component_baseline/workload.rs"]
mod workload;
use support::Directory;
use torse_codec::{StorageCodec, wire};
use torse_server::{bootstrap as base, execution::*, *};
use torse_sim::{Phase, action::ActorContext, durable::ExecutionSnapshot};
use torse_types::*;
use workload::Load;

fn load() -> Load {
    Load {
        tasks: 8,
        records: 32,
        members: 1,
        contended: false,
        deny_plan: false,
    }
}
fn policy() -> FileServerConfig {
    let mut p = base::file_policy();
    p.server.snapshot_budget = workload::budget();
    p.journal.codec_budget = workload::budget();
    p.checkpoint.codec_budget = workload::budget();
    p.journal.codec_limits = workload::limits().codec;
    p.checkpoint.codec_limits = workload::limits().codec;
    p.journal.max_pending_records = 2;
    p
}
fn create(dir: &Directory, load: Load) -> base::Files {
    let world = load.world();
    let actions = torse_sim::action::ActionService::bind(vec![], vec![], world.schemas()).unwrap();
    base::Files::create_file(
        dir.acquire(),
        world.coordinator().compatibility(),
        base::Codec::new(workload::limits().codec).unwrap(),
        policy(),
        |binding| ExecutionHost::new(binding, world, actions, load.rules(), workload::limits()),
    )
    .unwrap()
}
fn reopen(dir: &Directory, load: Load) -> base::Files {
    let world = load.world();
    let actions = torse_sim::action::ActionService::bind(vec![], vec![], world.schemas()).unwrap();
    base::Files::open_file(
        dir.acquire(),
        world.coordinator().compatibility(),
        base::Codec::new(workload::limits().codec).unwrap(),
        policy(),
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                world.registry(),
                world.schemas(),
                actions,
                load.rules(),
                workload::limits(),
            ))
        },
    )
    .unwrap()
}
fn config(workers: usize) -> PrevalidationConfig {
    PrevalidationConfig {
        max_in_flight: 4,
        min_component_bytes: 0,
        ..workload::config(workers)
    }
}
fn state(host: &ExecutionHost) -> Vec<u8> {
    let meta = SnapshotMeta {
        cursor: host.cursor(),
        compatibility: host.binding().compatibility,
        resume_at: host.resume_point(),
    };
    let snapshot = WorldSnapshot {
        meta,
        state: ExecutionSnapshot {
            parked: Default::default(),
            parking_reservation: None,
            world: host.world().capture_snapshot(meta).unwrap(),
            runtime: host.runtime().clone(),
        },
    };
    let mut bytes = vec![];
    base::Codec::new(workload::limits().codec)
        .unwrap()
        .encode_snapshot(&snapshot, &mut bytes, workload::budget())
        .unwrap();
    bytes
}

#[test]
fn one_and_four_workers_match_serial_component_groups_journals_and_encoded_replica_streams_with_busy_retries()
 {
    for members in [1, 2] {
        for contended in [false, true] {
            let load = Load {
                members,
                contended,
                ..load()
            };
            let mut reference = None;
            for workers in [0, 1, 4] {
                let dir = Directory::new();
                let mut server = create(&dir, load);
                server.set_commit_batch_limit(16).unwrap();
                if workers > 0 {
                    server.set_prevalidation(Some(config(workers))).unwrap();
                }
                server
                    .register_session(
                        Viewer {
                            session: SessionId(1),
                            actor: ActorContextId(1),
                        },
                        ActorContext::new(base::key("c:viewer")),
                        base::scope(),
                    )
                    .unwrap();
                let seed = server
                    .open_replica(SessionId(1), base::scope(), workload::budget())
                    .unwrap();
                server.drive_tick(&mut NoReplicas, 128).unwrap();
                let mut replicas = vec![];
                wire::encode(
                    &server
                        .take_replica(seed.subscription, workload::budget())
                        .unwrap(),
                    &mut replicas,
                    workload::budget(),
                    workload::limits().codec,
                )
                .unwrap();
                let host = server.authority().unwrap();
                let stats = host.prevalidation_stats();
                if workers > 0 && !contended {
                    assert_eq!(stats.prepared, 8);
                    assert!(stats.hits > 0);
                }
                assert!(stats.peak_jobs <= 4);
                assert!(stats.peak_cached_bytes <= stats.peak_reserved_bytes);
                let bytes = state(host);
                drop(server);
                let actual = (
                    bytes,
                    replicas,
                    std::fs::read(dir.0.join("journal.log")).unwrap(),
                );
                if let Some(expected) = &reference {
                    assert_eq!(&actual, expected);
                } else {
                    reference = Some(actual);
                }
            }
        }
    }
}

#[test]
fn component_cache_is_discarded_on_partial_recovery_without_replanning() {
    for completed in [0, 1, 3, 7] {
        let dir = Directory::new();
        let load = Load {
            members: 2,
            ..load()
        };
        let mut server = create(&dir, load);
        server.set_prevalidation(Some(config(4))).unwrap();
        while server.authority().unwrap().runtime().work.is_none()
            || usize::from(server.authority().unwrap().resume_point().next_phase)
                != Phase::Simulation.index()
        {
            server.drive_one(&mut NoReplicas).unwrap();
        }
        for _ in 0..completed {
            server.drive_one(&mut NoReplicas).unwrap();
        }
        drop(server);
        let mut recovered = reopen(
            &dir,
            Load {
                deny_plan: true,
                ..load
            },
        );
        assert_eq!(
            recovered
                .authority()
                .unwrap()
                .prevalidation_stats()
                .prepared,
            0
        );
        recovered.set_prevalidation(Some(config(4))).unwrap();
        recovered.drive_tick(&mut NoReplicas, 128).unwrap();
        let other = Directory::new();
        let mut reference = create(&other, load);
        reference.drive_tick(&mut NoReplicas, 128).unwrap();
        assert_eq!(
            state(recovered.authority().unwrap()),
            state(reference.authority().unwrap())
        );
    }
}

#[test]
fn component_mode_is_opt_in_and_threshold_or_work_budget_falls_back_to_serial() {
    for cfg in [
        PrevalidationConfig {
            components: false,
            ..config(4)
        },
        PrevalidationConfig {
            min_component_bytes: usize::MAX,
            ..config(4)
        },
        PrevalidationConfig {
            max_job_bytes: 1,
            ..config(4)
        },
    ] {
        let dir = Directory::new();
        let mut server = create(&dir, load());
        server.set_prevalidation(Some(cfg)).unwrap();
        server.drive_tick(&mut NoReplicas, 128).unwrap();
        let host = server.authority().unwrap();
        assert_eq!(host.prevalidation_stats().prepared, 0);
        assert!(host.prevalidation_stats().skipped > 0);
        let expected = state(host);
        let other = Directory::new();
        let mut reference = create(&other, load());
        reference.drive_tick(&mut NoReplicas, 128).unwrap();
        assert_eq!(expected, state(reference.authority().unwrap()));
    }
}
