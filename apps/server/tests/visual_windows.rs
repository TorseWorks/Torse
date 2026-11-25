#[path = "support/visual.rs"]
#[allow(dead_code, unused_imports)]
mod fixture;
mod support;
use std::time::{Duration, Instant};
use support::Directory;
use torse_codec::wire;
use torse_net::{
    MemoryTransport, Transport,
    terrain::{TerrainLimits, TerrainMirror},
};
use torse_server::{
    NoReplicas, bootstrap as base,
    execution::ExecutionHost,
    installs::SectionInstallLimits,
    network::*,
    parking::ParkingProgress,
    section_pages::SectionPages,
    visual::{VisualTerrainApproval, VisualTerrainLimits},
};
use torse_sim::replica::ReplicaScope;
use torse_types::*;
use torse_world::{SnapshotCells, StateEntry, WorldBounds};

fn network() -> NetworkLimits {
    let mut n = base::network_limits();
    n.budget.max_items = 2_000_000;
    n
}
fn limits() -> VisualTerrainLimits {
    VisualTerrainLimits {
        max_sections: 131_072,
        cache_bytes: 512 * 1024 * 1024,
        io_bytes: 32 * 1024 * 1024,
        batch_bytes: 512 * 1024,
        control_bytes: 8 * 1024 * 1024,
        pages_per_poll: 64,
    }
}
fn create(dir: &Directory) -> (base::Files, SectionPages) {
    let (world, actions) = base::world().unwrap();
    let mut snapshot = world
        .capture_snapshot(torse_sim::streaming::page_meta(&world))
        .unwrap();
    snapshot.bounds = WorldBounds::new(-64, 320).unwrap();
    let mut recovery = world.begin_restore();
    recovery.restore_snapshot(&snapshot).unwrap();
    let world = recovery.finish().unwrap();
    let compatibility = world.coordinator().compatibility();
    let mut execution = base::execution_limits();
    execution.runtime = OutputBudget {
        max_items: 4_000_000,
        max_bytes: 64 * 1024 * 1024,
    };
    execution.codec.max_frame_bytes = 64 * 1024 * 1024;
    execution.codec.max_field_bytes = 4 * 1024 * 1024;
    let mut policy = base::file_policy();
    policy.server.snapshot_budget = OutputBudget {
        max_items: 8_000_000,
        max_bytes: 128 * 1024 * 1024,
    };
    policy.checkpoint.codec_budget = policy.server.snapshot_budget;
    policy.checkpoint.codec_limits = execution.codec;
    policy.journal.codec_budget = execution.runtime;
    policy.journal.codec_limits = execution.codec;
    let mut server = base::Files::create_file(
        dir.acquire(),
        compatibility,
        base::Codec::new(execution.codec).unwrap(),
        policy,
        |binding| ExecutionHost::new(binding, world, actions, Vec::new(), execution),
    )
    .unwrap();
    server.set_section_install_limit(512).unwrap();
    let path = dir.0.join("sections");
    std::fs::create_dir_all(&path).unwrap();
    let pages = SectionPages::new(path, base::budget(), base::codec_limits(), 131_072).unwrap();
    (server, pages)
}
fn desired(approval: &VisualTerrainApproval, center: i64, radius: i64) -> ReplicaScope {
    approval
        .select_window(
            SectionPos::new(center - radius, -4, -radius),
            SectionPos::new(center + radius, 19, radius),
            limits().max_sections,
        )
        .unwrap()
}
fn drain(client: &mut MemoryTransport) -> Vec<ServerMessage> {
    let mut result = Vec::new();
    while let Some(bytes) = client.receive().unwrap() {
        result.push(wire::decode(&bytes, network().budget, network().codec).unwrap());
    }
    result
}
#[test]
#[ignore = "explicit Release fixture for repeated wide visual admission"]
fn wide_visual_interest_replacement_profile() {
    let dir = Directory::new();
    let (mut server, pages) = create(&dir);
    let approval = VisualTerrainApproval::rectangular(
        SectionPos::new(-32, -4, -32),
        SectionPos::new(40, 19, 32),
    )
    .unwrap();
    let left = desired(&approval, 0, 16);
    let right = desired(&approval, 7, 16);
    assert_eq!(left.sections.len(), 26_136);
    assert_eq!(right.sections.len(), left.sections.len());
    let mut hub = NetworkHub::<MemoryTransport>::new(server.binding(), network()).unwrap();
    let near = ReplicaScope {
        sections: [SectionPos::ZERO].into(),
    };
    let (id, _client, _, _) = fixture::interest::attach(&mut hub, &mut server, near);
    hub.enable_visual_interest(
        &server,
        id,
        pages,
        approval,
        left.clone(),
        SectionPos::ZERO,
        limits(),
    )
    .unwrap();
    let initial = hub.visual_terrain_diagnostics(id).unwrap().generation;
    for sample in 0..25 {
        let (request, center) = if sample % 2 == 0 {
            (right.clone(), SectionPos::new(7, 0, 0))
        } else {
            (left.clone(), SectionPos::ZERO)
        };
        let started = Instant::now();
        hub.set_visual_interest_deferred(id, request, center)
            .unwrap();
        let elapsed_ns = started.elapsed().as_nanos();
        let generation = hub.visual_terrain_diagnostics(id).unwrap().generation;
        assert_eq!(generation, initial + sample + 1);
        println!("visual sample={sample} elapsed_ns={elapsed_ns} generation={generation}");
    }
}

#[test]
fn full_height_control_admission_precedes_session_mutation_and_keeps_small_cache_independent() {
    let dir = Directory::new();
    let (mut server, pages) = create(&dir);
    let approval = VisualTerrainApproval::rectangular(
        SectionPos::new(-32, -4, -32),
        SectionPos::new(33, 19, 32),
    )
    .unwrap();
    let scope = desired(&approval, 0, 32);
    assert_eq!(scope.sections.len(), 101_400);
    let mut hub = NetworkHub::<MemoryTransport>::new(server.binding(), network()).unwrap();
    let near = ReplicaScope {
        sections: [SectionPos::ZERO].into(),
    };
    let (id, mut client, _, _) = fixture::interest::attach(&mut hub, &mut server, near.clone());
    let mut small = limits();
    small.control_bytes = 4096;
    assert_eq!(
        hub.enable_visual_interest(
            &server,
            id,
            pages.clone(),
            approval.clone(),
            scope.clone(),
            SectionPos::ZERO,
            small
        )
        .unwrap_err()
        .code,
        FaultCode::ResourceLimit
    );
    assert!(hub.visual_terrain_diagnostics(id).is_none());
    hub.enable_visual_interest(
        &server,
        id,
        pages.clone(),
        approval.clone(),
        near.clone(),
        SectionPos::ZERO,
        small,
    )
    .unwrap();
    let generation = hub.visual_terrain_diagnostics(id).unwrap().generation;
    assert_eq!(
        hub.set_visual_interest(id, scope.clone(), SectionPos::ZERO)
            .unwrap_err()
            .code,
        FaultCode::ResourceLimit
    );
    assert_eq!(
        hub.visual_terrain_diagnostics(id).unwrap().generation,
        generation
    );
    hub.poll(&mut server).unwrap();
    assert!(drain(&mut client).into_iter().any(|m| matches!(m,ServerMessage::Terrain(u) if u.reset.as_ref().is_some_and(|r| r.sections==[SectionPos::ZERO]))));
    assert!(hub.contains(id));
    let (other, mut client, _, _) = fixture::interest::attach(&mut hub, &mut server, near);
    hub.enable_visual_interest(
        &server,
        other,
        pages,
        approval,
        scope.clone(),
        SectionPos::ZERO,
        limits(),
    )
    .unwrap();
    let admitted_generation = hub.visual_terrain_diagnostics(other).unwrap().generation;
    assert_eq!(
        hub.set_visual_interest(
            other,
            ReplicaScope {
                sections: [SectionPos::new(34, 0, 0)].into()
            },
            SectionPos::ZERO
        )
        .unwrap_err()
        .code,
        FaultCode::InvalidData
    );
    assert_eq!(
        hub.visual_terrain_diagnostics(other).unwrap().generation,
        admitted_generation
    );
    let mut mirror = TerrainMirror::new(
        server.binding(),
        16,
        server.authority().unwrap().world().store().bounds(),
        TerrainLimits {
            max_sections: 131_072,
            max_bytes: 4096,
            metadata_bytes: 128 * 1024 * 1024,
        },
        None,
    )
    .unwrap();
    hub.poll(&mut server).unwrap();
    for message in drain(&mut client) {
        if let ServerMessage::Terrain(update) = message {
            mirror.apply(update).unwrap();
        }
    }
    assert_eq!(mirror.desired(), &scope.sections);
    assert_eq!(mirror.cached_pages(), 0);
    assert_eq!(mirror.metadata_bytes(), 101_400 * 512);
    assert!(
        hub.visual_terrain_diagnostics(other)
            .unwrap()
            .metadata_scanned
            <= 4096
    );
}

#[test]
fn page_frames_split_to_fit_and_oversized_single_pages_report_budget_without_disconnect() {
    for single in [false, true] {
        let dir = Directory::new();
        let (mut server, pages, _) = fixture::create(&dir);
        let mut net = network();
        net.codec.max_frame_bytes = if single { 4096 } else { 64 * 1024 };
        let mut hub = NetworkHub::<MemoryTransport>::new(server.binding(), net).unwrap();
        let near = ReplicaScope {
            sections: if single {
                Default::default()
            } else {
                [SectionPos::ZERO].into()
            },
        };
        let (id, mut client, _, _) = fixture::interest::attach(&mut hub, &mut server, near);
        let scope = fixture::scope(2, if single { 1 } else { 24 });
        let mut configured = limits();
        configured.pages_per_poll = 16;
        hub.enable_visual_interest(
            &server,
            id,
            pages,
            fixture::approval(),
            scope.clone(),
            SectionPos::new(2, 0, 0),
            configured,
        )
        .unwrap();
        let mut mirror = TerrainMirror::new(
            server.binding(),
            16,
            server.authority().unwrap().world().store().bounds(),
            TerrainLimits {
                max_sections: 32,
                max_bytes: 1024 * 1024,
                metadata_bytes: 32 * 1024,
            },
            None,
        )
        .unwrap();
        let start = Instant::now();
        let mut denied = false;
        loop {
            hub.poll(&mut server).unwrap();
            while let Some(bytes) = client.receive().unwrap() {
                assert!(bytes.len() <= net.codec.max_frame_bytes as usize);
                if let ServerMessage::Terrain(update) =
                    wire::decode::<ServerMessage>(&bytes, net.budget, net.codec).unwrap()
                {
                    denied |= update.unavailable.iter().any(|&(p, reason)| {
                        p == SectionPos::new(2, 0, 0) && reason == TerrainUnavailable::Budget
                    });
                    mirror.apply(update).unwrap();
                }
            }
            if single && denied || !single && mirror.cached_pages() == 24 {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::yield_now();
        }
        assert!(hub.contains(id));
        assert_eq!(mirror.desired(), &scope.sections);
        if single {
            assert_eq!(mirror.cached_pages(), 0);
            for _ in 0..20 {
                hub.poll(&mut server).unwrap();
                assert!(drain(&mut client).is_empty());
            }
        }
    }
}

#[test]
fn codec_and_item_limits_reject_large_control_before_enabling_display() {
    for items in [false, true] {
        let dir = Directory::new();
        let (mut server, pages) = create(&dir);
        let approval = VisualTerrainApproval::rectangular(
            SectionPos::new(-32, -4, -32),
            SectionPos::new(32, 19, 32),
        )
        .unwrap();
        let scope = desired(&approval, 0, 32);
        let mut net = network();
        if items {
            net.budget.max_items = 10_000;
        } else {
            net.codec.max_frame_bytes = 64 * 1024;
        }
        let mut hub = NetworkHub::<MemoryTransport>::new(server.binding(), net).unwrap();
        let near = ReplicaScope {
            sections: [SectionPos::ZERO].into(),
        };
        let (id, _, _, _) = fixture::interest::attach(&mut hub, &mut server, near.clone());
        assert_eq!(
            hub.enable_visual_interest(
                &server,
                id,
                pages.clone(),
                approval.clone(),
                scope,
                SectionPos::ZERO,
                limits()
            )
            .unwrap_err()
            .code,
            FaultCode::ResourceLimit
        );
        assert!(hub.visual_terrain_diagnostics(id).is_none());
        assert!(hub.contains(id));
        hub.enable_visual_interest(
            &server,
            id,
            pages,
            approval,
            near,
            SectionPos::ZERO,
            limits(),
        )
        .unwrap();
    }
}

#[test]
#[ignore = "release full-height file-backed display route"]
fn full_height_display_route_measurement() {
    let dir = Directory::new();
    let (mut server, pages) = create(&dir);
    let approval = VisualTerrainApproval::rectangular(
        SectionPos::new(-32, -4, -32),
        SectionPos::new(33, 19, 32),
    )
    .unwrap();
    let positions = approval
        .select_window(
            SectionPos::new(-32, -4, -32),
            SectionPos::new(33, 19, 32),
            131_072,
        )
        .unwrap();
    let positions: Vec<_> = positions
        .sections
        .into_iter()
        .filter(|p| {
            server
                .authority()
                .unwrap()
                .world()
                .store()
                .phase(*p)
                .is_none()
        })
        .collect();
    let prepared = Instant::now();
    for chunk in positions.chunks(512) {
        let world = server.authority().unwrap().world();
        let template = fixture::streaming::generated(world, chunk[0]);
        let stone = world
            .registry()
            .default_state(&base::key("base:stone"))
            .unwrap();
        let input = chunk
            .iter()
            .map(|&pos| {
                let mut page = template.clone();
                page.sections[0].pos = pos;
                if pos.y < 0 && (pos.x + pos.z).rem_euclid(7) == 0 {
                    page.sections[0].cells = SnapshotCells::Uniform(stone);
                    page.states = vec![StateEntry {
                        id: stone,
                        block: base::key("base:stone"),
                        values: vec![],
                    }];
                }
                page
            })
            .collect();
        server
            .begin_generated_parking(
                pages.clone(),
                input,
                SectionInstallLimits {
                    max_pages: 512,
                    max_bytes: 8 * 1024 * 1024,
                },
            )
            .unwrap();
        let start = Instant::now();
        loop {
            if matches!(
                server.poll_generated_parking(&mut NoReplicas).unwrap(),
                ParkingProgress::Finished { .. }
            ) {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(60));
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    println!(
        "prepared_pages={},prepare_ms={:.3}",
        positions.len(),
        prepared.elapsed().as_secs_f64() * 1000.
    );
    let resident = server.authority().unwrap().world().store().len();
    let native: std::collections::BTreeMap<_, _> = server
        .authority()
        .unwrap()
        .world()
        .store()
        .positions()
        .map(|p| {
            (
                p,
                server
                    .authority()
                    .unwrap()
                    .world()
                    .capture_section(p, base::budget())
                    .unwrap()
                    .sections[0]
                    .cells
                    .clone(),
            )
        })
        .collect();
    let stone = server
        .authority()
        .unwrap()
        .world()
        .registry()
        .default_state(&base::key("base:stone"))
        .unwrap();
    let mut hub = NetworkHub::<MemoryTransport>::new(server.binding(), network()).unwrap();
    let (id, mut client, _, _) = fixture::interest::attach(
        &mut hub,
        &mut server,
        ReplicaScope {
            sections: [SectionPos::ZERO].into(),
        },
    );
    let mut mirror = TerrainMirror::new(
        server.binding(),
        16,
        server.authority().unwrap().world().store().bounds(),
        TerrainLimits {
            max_sections: 131_072,
            max_bytes: 512 * 1024 * 1024,
            metadata_bytes: 128 * 1024 * 1024,
        },
        None,
    )
    .unwrap();
    println!(
        "sample,center,radius,desired,coverage,first_ms,all_ms,max_poll_ms,polls,host_cache_bytes,client_bytes,reserved_bytes,pending_peak,client_peak"
    );
    for (sample, (center, radius)) in [(0, 32), (1, 32), (0, 32), (0, 16), (0, 32)]
        .into_iter()
        .enumerate()
    {
        let scope = desired(&approval, center, radius);
        let start = Instant::now();
        if sample == 0 {
            hub.enable_visual_interest(
                &server,
                id,
                pages.clone(),
                approval.clone(),
                scope.clone(),
                SectionPos::new(center, 0, 0),
                limits(),
            )
            .unwrap();
        } else {
            hub.set_visual_interest(id, scope.clone(), SectionPos::new(center, 0, 0))
                .unwrap();
        }
        let mut first = None;
        let mut max_poll = Duration::ZERO;
        let mut polls = 0;
        let mut matches_scope = false;
        let mut pending_peak = hub
            .visual_terrain_diagnostics(id)
            .unwrap()
            .pending_candidates;
        let mut client_peak = mirror.accounted_bytes();
        loop {
            let poll = Instant::now();
            hub.poll(&mut server).unwrap();
            max_poll = max_poll.max(poll.elapsed());
            polls += 1;
            for message in drain(&mut client) {
                if let ServerMessage::Terrain(update) = message {
                    assert!(
                        update.unavailable.is_empty(),
                        "{:?}",
                        update.unavailable.first()
                    );
                    if !update.pages.is_empty() {
                        first.get_or_insert(start.elapsed());
                    }
                    mirror.apply(update).unwrap();
                    matches_scope = mirror.desired() == &scope.sections;
                    if matches_scope && mirror.cached_pages() > 0 {
                        first.get_or_insert(start.elapsed());
                    }
                }
            }
            assert_eq!(server.authority().unwrap().world().store().len(), resident);
            assert!(hub.visual_terrain_diagnostics(id).unwrap().metadata_scanned <= 4096);
            pending_peak = pending_peak.max(
                hub.visual_terrain_diagnostics(id)
                    .unwrap()
                    .pending_candidates,
            );
            client_peak = client_peak.max(mirror.accounted_bytes());
            if matches_scope && mirror.cached_pages() == scope.sections.len() {
                break;
            }
            if start.elapsed() > Duration::from_secs(180) {
                println!(
                    "unfinished,{sample},{},{},{polls}",
                    scope.sections.len(),
                    mirror.cached_pages()
                );
                panic!("incomplete full-height sample");
            }
            std::thread::yield_now();
        }
        let d = hub.visual_terrain_diagnostics(id).unwrap();
        assert_eq!(mirror.pages().count(), scope.sections.len());
        for page in mirror.pages() {
            let section = &page.sections[0];
            let expected = native.get(&section.pos).cloned().unwrap_or_else(|| {
                SnapshotCells::Uniform(
                    if section.pos.y < 0 && (section.pos.x + section.pos.z).rem_euclid(7) == 0 {
                        stone
                    } else {
                        torse_world::BlockStateId::EMPTY
                    },
                )
            });
            assert_eq!(section.cells, expected);
            assert!(page.entities.is_empty());
        }
        println!(
            "{sample},{center},{radius},{},1.0,{:.3},{:.3},{:.3},{polls},{},{},{},{pending_peak},{client_peak}",
            scope.sections.len(),
            first.unwrap_or(start.elapsed()).as_secs_f64() * 1000.,
            start.elapsed().as_secs_f64() * 1000.,
            max_poll.as_secs_f64() * 1000.,
            d.cache_bytes,
            mirror.accounted_bytes(),
            d.reserved_bytes
        );
    }
}

#[test]
#[ignore = "release control/display scheduling comparison"]
fn separate_display_control_route_measurement() {
    use torse_server::lighting::{HostLightLimits, HostLightWorkBudget};
    use torse_world_services::lighting::{LightFieldLimits, LightLimits};
    println!(
        "mode,sample,center,control_calls,control_total_ms,control_max_ms,move_ms,display_ms,display_max_ms,first_near_column_payload_ms,received_pages,available_pages,host_cache_bytes,light_input_bytes,light_input_peak,light_pending,slow_connected"
    );
    for split in [false, true] {
        let dir = Directory::new();
        let (mut server, pages) = create(&dir);
        let approval = VisualTerrainApproval::rectangular(
            SectionPos::new(-16, -4, -12),
            SectionPos::new(16, 19, 12),
        )
        .unwrap();
        let stone = server
            .authority()
            .unwrap()
            .world()
            .registry()
            .default_state(&base::key("base:stone"))
            .unwrap();
        let input: Vec<_> = (-8..=8)
            .flat_map(|x| (-4..=19).map(move |y| SectionPos::new(x, y, 0)))
            .filter(|p| {
                server
                    .authority()
                    .unwrap()
                    .world()
                    .store()
                    .phase(*p)
                    .is_none()
            })
            .map(|pos| {
                let mut page =
                    fixture::streaming::generated(server.authority().unwrap().world(), pos);
                page.sections[0].cells = SnapshotCells::Dense(
                    (0..4096)
                        .map(|i| {
                            if (i + pos.x.unsigned_abs() as usize) % 7 < 3 {
                                stone
                            } else {
                                torse_world::BlockStateId::EMPTY
                            }
                        })
                        .collect(),
                );
                page.states = vec![StateEntry {
                    id: stone,
                    block: base::key("base:stone"),
                    values: vec![],
                }];
                page
            })
            .collect();
        let expected: std::collections::BTreeMap<_, _> = input
            .iter()
            .map(|page| (page.sections[0].pos, page.sections[0].cells.clone()))
            .collect();
        server
            .begin_generated_parking(
                pages.clone(),
                input,
                SectionInstallLimits {
                    max_pages: 512,
                    max_bytes: 64 * 1024 * 1024,
                },
            )
            .unwrap();
        let prepared = Instant::now();
        while !matches!(
            server.poll_generated_parking(&mut NoReplicas).unwrap(),
            ParkingProgress::Finished { .. }
        ) {
            assert!(prepared.elapsed() < Duration::from_secs(30));
            std::thread::yield_now();
        }
        let mut hub = NetworkHub::<MemoryTransport>::new(server.binding(), network()).unwrap();
        hub.enable_shared_lighting(
            &server,
            pages.clone(),
            HostLightLimits {
                input_sections: 2048,
                input_bytes: 32 * 1024 * 1024,
                pages_per_poll: 4,
                field: LightFieldLimits {
                    sections: 64,
                    cache_bytes: 8 * 1024 * 1024,
                    solver_cache_bytes: 32 * 1024 * 1024,
                    consumers: 4,
                    interests: 256,
                    job: LightLimits {
                        cells: 1_000_000,
                        dependencies: 2048,
                        frontier: 1_000_000,
                        bytes: 32 * 1024 * 1024,
                    },
                },
            },
            20_000,
        )
        .unwrap();
        let near = ReplicaScope {
            sections: [SectionPos::ZERO].into(),
        };
        let (id, mut client, _, _) = fixture::interest::attach(&mut hub, &mut server, near.clone());
        let (slow, _stalled, _, _) = fixture::interest::attach(&mut hub, &mut server, near);
        let mut config = limits();
        config.pages_per_poll = 4;
        config.cache_bytes = 16 * 1024 * 1024;
        for peer in [id, slow] {
            hub.enable_visual_interest(
                &server,
                peer,
                pages.clone(),
                approval.clone(),
                desired(&approval, 0, 12),
                SectionPos::ZERO,
                config,
            )
            .unwrap();
        }
        let mut mirror = TerrainMirror::new(
            server.binding(),
            16,
            server.authority().unwrap().world().store().bounds(),
            TerrainLimits {
                max_sections: 15000,
                max_bytes: 32 * 1024 * 1024,
                metadata_bytes: 16 * 1024 * 1024,
            },
            None,
        )
        .unwrap();
        for (sample, center) in [0, 1, 2, 1, 0].into_iter().enumerate() {
            let move_start = Instant::now();
            if split {
                hub.set_visual_interest_deferred(
                    id,
                    desired(&approval, center, 12),
                    SectionPos::new(center, 0, 0),
                )
                .unwrap();
            } else {
                hub.set_visual_interest(
                    id,
                    desired(&approval, center, 12),
                    SectionPos::new(center, 0, 0),
                )
                .unwrap();
            }
            let move_time = move_start.elapsed();
            let mut control_total = Duration::ZERO;
            let mut control_max = Duration::ZERO;
            let mut display_time = Duration::ZERO;
            let mut display_max = Duration::ZERO;
            let mut first_near = None;
            let mut received = 0;
            for _ in 0..24 {
                for _ in 0..17 {
                    fixture::interest::send(&mut client, ClientMessage::Ping);
                    let start = Instant::now();
                    if split {
                        hub.poll_control(&mut server).unwrap();
                    } else {
                        hub.poll(&mut server).unwrap();
                    }
                    let elapsed = start.elapsed();
                    control_total += elapsed;
                    control_max = control_max.max(elapsed);
                    for msg in drain(&mut client) {
                        match msg {
                            ServerMessage::Terrain(update) => {
                                for page in &update.pages {
                                    if let Some(cells) = expected.get(&page.terrain.sections[0].pos)
                                    {
                                        assert_eq!(&page.terrain.sections[0].cells, cells);
                                    }
                                }
                                if update.pages.iter().any(|page| {
                                    page.terrain.sections[0].pos.x.abs_diff(center) <= 1
                                }) {
                                    first_near.get_or_insert(move_start.elapsed());
                                }
                                received += update.pages.len();
                                mirror.apply(update).unwrap();
                            }
                            ServerMessage::Light(update) => {
                                mirror.apply_light(update).unwrap();
                            }
                            _ => {}
                        }
                    }
                }
                if split {
                    let start = Instant::now();
                    hub.poll_display(
                        &server,
                        DisplayWorkBudget {
                            sessions: 2,
                            light_admissions: 1,
                            metadata_checks_per_session: 256,
                            light_checks_per_session: 32,
                            lighting: HostLightWorkBudget {
                                input_checks: 256,
                                validation_checks: 16,
                                readiness_checks: 32,
                                solver_work: 340_000,
                            },
                        },
                    )
                    .unwrap();
                    let elapsed = start.elapsed();
                    display_time += elapsed;
                    display_max = display_max.max(elapsed);
                    for msg in drain(&mut client) {
                        match msg {
                            ServerMessage::Terrain(update) => {
                                for page in &update.pages {
                                    if let Some(cells) = expected.get(&page.terrain.sections[0].pos)
                                    {
                                        assert_eq!(&page.terrain.sections[0].cells, cells);
                                    }
                                }
                                if update.pages.iter().any(|page| {
                                    page.terrain.sections[0].pos.x.abs_diff(center) <= 1
                                }) {
                                    first_near.get_or_insert(move_start.elapsed());
                                }
                                received += update.pages.len();
                                mirror.apply(update).unwrap();
                            }
                            ServerMessage::Light(update) => {
                                mirror.apply_light(update).unwrap();
                            }
                            _ => {}
                        }
                    }
                }
                server.drive_tick(&mut hub, 1024).unwrap();
            }
            assert!(hub.contains(id));
            let visual = hub.visual_terrain_diagnostics(id).unwrap();
            let light = hub.shared_light_resources().unwrap();
            println!(
                "{},{sample},{center},408,{:.6},{:.6},{:.6},{:.6},{:.6},{},{received},{},{},{},{},{},{}",
                if split { "split" } else { "combined" },
                control_total.as_secs_f64() * 1000.,
                control_max.as_secs_f64() * 1000.,
                move_time.as_secs_f64() * 1000.,
                display_time.as_secs_f64() * 1000.,
                display_max.as_secs_f64() * 1000.,
                first_near.map_or("unobserved".into(), |t| format!(
                    "{:.6}",
                    t.as_secs_f64() * 1000.
                )),
                mirror.pages().count(),
                visual.cache_bytes,
                light.input_bytes,
                light.input_peak,
                light.field.pending,
                hub.contains(slow)
            );
        }
    }
}
