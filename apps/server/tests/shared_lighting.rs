mod support;
#[path = "support/visual.rs"]
#[allow(dead_code, unused_imports)]
mod visual_fixture;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use support::Directory;
use torse_server::{
    NoReplicas,
    lighting::{HostLightField, HostLightLimits, LightInputStatus},
};
use torse_types::*;
use torse_world_services::lighting::{LightFieldLimits, LightLimits};
use visual_fixture::{interest, streaming as fixture};

fn limits() -> HostLightLimits {
    HostLightLimits {
        input_sections: 64,
        input_bytes: 2_000_000,
        pages_per_poll: 1,
        field: LightFieldLimits {
            sections: 8,
            cache_bytes: 12_000_000,
            consumers: 4,
            interests: 32,
            job: LightLimits {
                cells: 200_000,
                dependencies: 64,
                frontier: 200_000,
                bytes: 5_000_000,
            },
        },
    }
}
fn settle(field: &mut HostLightField, host: &torse_server::execution::ExecutionHost) {
    let start = Instant::now();
    loop {
        field.poll(host, 20_000).unwrap();
        if field.diagnostics().pending == 0 {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            field.diagnostics()
        );
        std::thread::yield_now();
    }
}
#[test]
fn nearby_ready_column_publishes_before_distant_saved_inputs_are_verified() {
    let dir = Directory::new();
    let mut server = fixture::create(&dir);
    let pages = fixture::pages(&dir);
    let near = SectionPos::ZERO;
    let mut targets = vec![near];
    // Negative coordinates deliberately precede the near target in map order.
    for x in -40..-16 {
        let pos = SectionPos::new(x, 0, 0);
        let page = fixture::generated(server.authority().unwrap().world(), pos);
        server
            .install_section(&pages, page, &mut NoReplicas)
            .unwrap();
        server.unload_section(&pages, pos, &mut NoReplicas).unwrap();
        targets.push(pos);
    }
    targets.sort_unstable();
    let mut config = limits();
    config.input_sections = 512;
    config.field.sections = 32;
    config.field.interests = 64;
    let mut light = HostLightField::new(server.authority().unwrap(), pages, config).unwrap();
    assert_eq!(
        light.subscribe_near(1, &targets, near).unwrap(),
        targets.len()
    );
    for _ in 0..10 {
        light.poll(server.authority().unwrap(), 1_000_000).unwrap();
        if light
            .publication(1, near, server.authority().unwrap())
            .is_some()
        {
            assert_ne!(
                light.input_status(targets[0]),
                Some(LightInputStatus::Ready)
            );
            assert!(light.diagnostics().pending > 0);
            return;
        }
    }
    panic!("ready near column waited for unrelated saved terrain");
}

#[test]
fn parked_input_pipeline_is_bounded_and_refresh_rejects_old_reads() {
    let dir = Directory::new();
    let mut server = fixture::create(&dir);
    let pages = fixture::pages(&dir);
    let targets = [
        SectionPos::new(4, 0, 0),
        SectionPos::new(5, 0, 0),
        SectionPos::new(6, 0, 0),
    ];
    for pos in targets {
        let page = fixture::generated(server.authority().unwrap().world(), pos);
        server
            .install_section(&pages, page, &mut NoReplicas)
            .unwrap();
        server.unload_section(&pages, pos, &mut NoReplicas).unwrap();
    }
    let mut config = limits();
    config.pages_per_poll = 3;
    let mut light = HostLightField::new(server.authority().unwrap(), pages, config).unwrap();
    light.subscribe_near(1, &targets, targets[0]).unwrap();
    light.poll(server.authority().unwrap(), 1_000_000).unwrap();
    assert_eq!(light.resource_diagnostics().io_in_flight, 3);
    assert_eq!(light.diagnostics().published, 0);
    light.refresh().unwrap();
    for pos in targets {
        assert!(
            light
                .publication(1, pos, server.authority().unwrap())
                .is_none()
        );
    }
    settle(&mut light, server.authority().unwrap());
    assert_eq!(light.resource_diagnostics().io_in_flight, 0);
    assert_eq!(light.diagnostics().published, 3);
}

#[test]
fn sparse_radius_32_light_admission_preserves_other_peers_and_replaces_old_links() {
    let dir = Directory::new();
    let server = fixture::create(&dir);
    let mut config = limits();
    // A single target's 3x3 horizontal halo spans two vertical sections.
    config.input_sections = 18;
    let mut light =
        HostLightField::new(server.authority().unwrap(), fixture::pages(&dir), config).unwrap();
    let origin = SectionPos::ZERO;
    let far = SectionPos::new(32, 0, 32);
    let sparse: Vec<_> = (-32..=32)
        .step_by(8)
        .flat_map(|x| (-32..=32).step_by(8).map(move |z| SectionPos::new(x, 0, z)))
        .collect();
    assert_eq!(light.subscribe_near(1, &sparse, origin).unwrap(), 1);
    settle(&mut light, server.authority().unwrap());
    assert!(
        light
            .publication(1, origin, server.authority().unwrap())
            .is_some()
    );
    assert_eq!(light.subscribe_near(2, &sparse, far).unwrap(), 1);
    assert_eq!(light.resource_diagnostics().admitted_links, 2);
    // The second peer reuses the first peer's covered origin, rather than
    // displacing it to satisfy its own otherwise unaffordable far target.
    assert!(Arc::ptr_eq(
        light
            .publication(1, origin, server.authority().unwrap())
            .unwrap(),
        light
            .publication(2, origin, server.authority().unwrap())
            .unwrap(),
    ));
    assert_eq!(light.subscribe_near(2, &[far], far).unwrap(), 0);
    assert!(
        light
            .publication(2, origin, server.authority().unwrap())
            .is_none()
    );
    assert!(
        light
            .publication(1, origin, server.authority().unwrap())
            .is_some()
    );
    light.unsubscribe(1);
    assert_eq!(light.subscribe_near(2, &[far], far).unwrap(), 1);
    assert_eq!(light.resource_diagnostics().input_sections, 18);
    assert!(light.resource_diagnostics().admission_denials > 0);
}

#[test]
fn consumer_and_link_caps_only_deny_optional_light_links() {
    for consumer_cap in [true, false] {
        let dir = Directory::new();
        let server = fixture::create(&dir);
        let mut config = limits();
        if consumer_cap {
            config.field.consumers = 1;
        } else {
            config.field.interests = 1;
        }
        let mut light =
            HostLightField::new(server.authority().unwrap(), fixture::pages(&dir), config).unwrap();
        assert_eq!(
            light
                .subscribe_near(1, &[SectionPos::ZERO], SectionPos::ZERO)
                .unwrap(),
            1
        );
        assert_eq!(
            light
                .subscribe_near(2, &[SectionPos::ZERO], SectionPos::ZERO)
                .unwrap(),
            0
        );
        settle(&mut light, server.authority().unwrap());
        assert!(
            light
                .publication(1, SectionPos::ZERO, server.authority().unwrap())
                .is_some()
        );
        assert!(
            light
                .publication(2, SectionPos::ZERO, server.authority().unwrap())
                .is_none()
        );
        light.unsubscribe(1);
        assert_eq!(
            light
                .subscribe_near(2, &[SectionPos::ZERO], SectionPos::ZERO)
                .unwrap(),
            1
        );
    }
}

#[test]
fn optional_light_budgets_do_not_block_sparse_terrain_or_interest_moves() {
    use torse_net::{
        MemoryTransport,
        terrain::{TerrainLimits, TerrainMirror},
    };
    use torse_server::{
        bootstrap as base,
        network::*,
        visual::{VisualTerrainApproval, VisualTerrainLimits},
    };
    use torse_sim::replica::ReplicaScope;
    for cause in ["input envelope", "input bytes", "targets", "wire items"] {
        let dir = Directory::new();
        let mut server = fixture::create(&dir);
        let pages = fixture::pages(&dir);
        let near = SectionPos::ZERO;
        let far = SectionPos::new(32, 0, 32);
        let page = fixture::generated(server.authority().unwrap().world(), far);
        server
            .install_section(&pages, page, &mut NoReplicas)
            .unwrap();
        server.unload_section(&pages, far, &mut NoReplicas).unwrap();
        let scope = ReplicaScope {
            sections: (-32..=32)
                .step_by(8)
                .flat_map(|x| (-32..=32).step_by(8).map(move |z| SectionPos::new(x, 0, z)))
                .collect(),
        };
        let mut network = base::network_limits();
        let mut config = limits();
        config.pages_per_poll = 64;
        match cause {
            "input envelope" => config.input_sections = 1,
            "input bytes" => config.input_bytes = 1,
            "targets" => config.field.sections = 1,
            "wire items" => network.budget.max_items = 1000,
            _ => unreachable!(),
        }
        let mut hub = NetworkHub::<MemoryTransport>::new(server.binding(), network).unwrap();
        hub.enable_shared_lighting(&server, pages.clone(), config, 1_000_000)
            .unwrap();
        let (id, mut client, _, _) = interest::attach(
            &mut hub,
            &mut server,
            ReplicaScope {
                sections: [near].into(),
            },
        );
        hub.enable_visual_interest(
            &server,
            id,
            pages,
            VisualTerrainApproval::rectangular(SectionPos::new(-32, 0, -32), far).unwrap(),
            scope,
            near,
            VisualTerrainLimits {
                max_sections: 81,
                cache_bytes: 512 * 1024,
                io_bytes: 32 * 1024 * 1024,
                batch_bytes: 64 * 1024,
                pages_per_poll: 8,
            },
        )
        .unwrap();
        let mut mirror = TerrainMirror::new(
            server.binding(),
            16,
            server.authority().unwrap().world().store().bounds(),
            TerrainLimits {
                max_sections: 81,
                max_bytes: 512 * 1024,
            },
            None,
        )
        .unwrap();
        let start = Instant::now();
        loop {
            hub.poll(&mut server).unwrap();
            for message in interest::drain(&mut client).0 {
                match message {
                    ServerMessage::Terrain(update) => {
                        mirror.apply(update).unwrap();
                    }
                    ServerMessage::Light(update) => {
                        mirror.apply_light(update).unwrap();
                    }
                    _ => {}
                }
            }
            if mirror.page(near).is_some() && mirror.page(far).is_some() {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "{cause}");
            std::thread::yield_now();
        }
        assert!(
            mirror.light(far).is_none(),
            "{cause}: fallback target must have terrain only"
        );
        assert!(hub.contains(id), "{cause}");
        assert!(hub.shared_light_resources().unwrap().input_sections <= config.input_sections);
        hub.set_visual_interest(
            id,
            ReplicaScope {
                sections: [far].into(),
            },
            far,
        )
        .unwrap();
        loop {
            hub.poll(&mut server).unwrap();
            for message in interest::drain(&mut client).0 {
                match message {
                    ServerMessage::Terrain(update) => {
                        mirror.apply(update).unwrap();
                    }
                    ServerMessage::Light(update) => {
                        mirror.apply_light(update).unwrap();
                    }
                    _ => {}
                }
            }
            if mirror.page(near).is_none() && mirror.page(far).is_some() {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "move: {cause}");
            std::thread::yield_now();
        }
        assert!(hub.contains(id), "{cause}");
    }
}

#[test]
fn file_backed_host_shares_results_and_reuses_identical_unload_reload_inputs() {
    let dir = Directory::new();
    let mut server = fixture::create(&dir);
    let pages = fixture::pages(&dir);
    let target = SectionPos::ZERO;
    let mut light =
        HostLightField::new(server.authority().unwrap(), pages.clone(), limits()).unwrap();
    light.subscribe(1, &[target]).unwrap();
    light.subscribe(2, &[target]).unwrap();
    settle(&mut light, server.authority().unwrap());
    let first = light
        .publication(1, target, server.authority().unwrap())
        .unwrap()
        .clone();
    assert!(Arc::ptr_eq(
        &first,
        light
            .publication(2, target, server.authority().unwrap())
            .unwrap()
    ));
    assert_eq!(light.diagnostics().published, 1);
    server
        .unload_section(&pages, target, &mut NoReplicas)
        .unwrap();
    assert!(
        light
            .publication(1, target, server.authority().unwrap())
            .is_none()
    );
    settle(&mut light, server.authority().unwrap());
    assert!(Arc::ptr_eq(
        &first,
        light
            .publication(1, target, server.authority().unwrap())
            .unwrap()
    ));
    assert_eq!(light.diagnostics().started, 1);
    server
        .reload_section(&pages, target, &mut NoReplicas)
        .unwrap();
    settle(&mut light, server.authority().unwrap());
    assert!(Arc::ptr_eq(
        &first,
        light
            .publication(1, target, server.authority().unwrap())
            .unwrap()
    ));
    assert_eq!(light.diagnostics().started, 1);
    light.unsubscribe(1);
    assert!(
        light
            .publication(1, target, server.authority().unwrap())
            .is_none()
    );
    light.unsubscribe(2);
    assert_eq!(light.input_bytes(), 0);
    assert_eq!(light.diagnostics().cache_bytes, 0);
}
#[test]
fn corrupted_saved_input_is_not_replaced_with_air_and_refresh_retries() {
    let dir = Directory::new();
    let mut server = fixture::create(&dir);
    let pages = fixture::pages(&dir);
    let target = SectionPos::ZERO;
    server
        .unload_section(&pages, target, &mut NoReplicas)
        .unwrap();
    let digest = server.authority().unwrap().parked_sections()[&target].digest;
    let path = pages.path(digest);
    let valid = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"damaged").unwrap();
    let mut light = HostLightField::new(server.authority().unwrap(), pages, limits()).unwrap();
    light.subscribe(1, &[target]).unwrap();
    let start = Instant::now();
    while light.input_status(target) != Some(LightInputStatus::Corrupt) {
        light.poll(server.authority().unwrap(), 20_000).unwrap();
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::yield_now();
    }
    assert_eq!(light.diagnostics().published, 0);
    assert!(
        light
            .publication(1, target, server.authority().unwrap())
            .is_none()
    );
    std::fs::write(&path, valid).unwrap();
    light.refresh().unwrap();
    settle(&mut light, server.authority().unwrap());
    assert!(
        light
            .publication(1, target, server.authority().unwrap())
            .is_some()
    );
}

#[test]
fn shared_display_light_reaches_fast_peer_while_other_peer_keeps_output_pending() {
    use torse_net::{
        MemoryTransport,
        terrain::{TerrainLimits, TerrainMirror},
    };
    use torse_server::{
        bootstrap as base,
        network::*,
        visual::{VisualTerrainApproval, VisualTerrainLimits},
    };
    use torse_sim::replica::ReplicaScope;
    let dir = Directory::new();
    let mut server = fixture::create(&dir);
    let pages = fixture::pages(&dir);
    let target = SectionPos::ZERO;
    let scope = ReplicaScope {
        sections: [target].into(),
    };
    let mut hub =
        NetworkHub::<MemoryTransport>::new(server.binding(), base::network_limits()).unwrap();
    hub.enable_shared_lighting(&server, pages.clone(), limits(), 20_000)
        .unwrap();
    let (slow, mut slow_client, _, _) = interest::attach(&mut hub, &mut server, scope.clone());
    let (fast, mut fast_client, _, _) = interest::attach(&mut hub, &mut server, scope.clone());
    let visual = VisualTerrainLimits {
        max_sections: 1,
        cache_bytes: 128 * 1024,
        io_bytes: 32 * 1024 * 1024,
        batch_bytes: 64 * 1024,
        pages_per_poll: 1,
    };
    for id in [slow, fast] {
        hub.enable_visual_interest(
            &server,
            id,
            pages.clone(),
            VisualTerrainApproval::rectangular(target, target).unwrap(),
            scope.clone(),
            target,
            visual,
        )
        .unwrap();
    }
    let mut mirror = TerrainMirror::new(
        server.binding(),
        16,
        server.authority().unwrap().world().store().bounds(),
        TerrainLimits {
            max_sections: 1,
            max_bytes: 128 * 1024,
        },
        None,
    )
    .unwrap();
    let start = Instant::now();
    loop {
        hub.poll(&mut server).unwrap();
        for message in interest::drain(&mut fast_client).0 {
            match message {
                ServerMessage::Terrain(update) => {
                    mirror.apply(update).unwrap();
                }
                ServerMessage::Light(update) => {
                    mirror.apply_light(update).unwrap();
                }
                _ => {}
            }
        }
        if mirror.light(target).is_some() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::yield_now();
    }
    assert_eq!(hub.shared_light_diagnostics().unwrap().started, 1);
    assert_eq!(hub.shared_light_diagnostics().unwrap().published, 1);
    assert_eq!(hub.shared_light_diagnostics().unwrap().pending, 0);
    let generation = mirror.light(target).unwrap().generation;
    // Drain the slow peer only after the other peer has finished.
    let mut other = TerrainMirror::new(
        server.binding(),
        16,
        server.authority().unwrap().world().store().bounds(),
        TerrainLimits {
            max_sections: 1,
            max_bytes: 128 * 1024,
        },
        None,
    )
    .unwrap();
    loop {
        for message in interest::drain(&mut slow_client).0 {
            match message {
                ServerMessage::Terrain(update) => {
                    other.apply(update).unwrap();
                }
                ServerMessage::Light(update) => {
                    other.apply_light(update).unwrap();
                }
                _ => {}
            }
        }
        if other.light(target).is_some() {
            break;
        }
        hub.poll(&mut server).unwrap();
        assert!(start.elapsed() < Duration::from_secs(10));
    }
    assert_eq!(other.light(target).unwrap().generation, generation);
    hub.revoke(&mut server, slow).unwrap();
    hub.revoke(&mut server, fast).unwrap();
    assert_eq!(hub.shared_light_diagnostics().unwrap().cache_bytes, 0);
}

#[test]
fn ready_shared_light_is_attached_to_the_first_terrain_page() {
    use torse_net::MemoryTransport;
    use torse_server::{
        bootstrap as base,
        network::*,
        visual::{VisualTerrainApproval, VisualTerrainLimits},
    };
    use torse_sim::replica::ReplicaScope;
    let dir = Directory::new();
    let mut server = fixture::create(&dir);
    let pages = fixture::pages(&dir);
    let target = SectionPos::ZERO;
    let scope = ReplicaScope {
        sections: [target].into(),
    };
    let mut hub =
        NetworkHub::<MemoryTransport>::new(server.binding(), base::network_limits()).unwrap();
    let mut light = limits();
    light.pages_per_poll = 64;
    hub.enable_shared_lighting(&server, pages.clone(), light, 1_000_000)
        .unwrap();
    let (id, mut client, _, _) = interest::attach(&mut hub, &mut server, scope.clone());
    hub.enable_visual_interest(
        &server,
        id,
        pages,
        VisualTerrainApproval::rectangular(target, target).unwrap(),
        scope,
        target,
        VisualTerrainLimits {
            max_sections: 1,
            cache_bytes: 128 * 1024,
            io_bytes: 32 * 1024 * 1024,
            batch_bytes: 64 * 1024,
            pages_per_poll: 1,
        },
    )
    .unwrap();
    let start = Instant::now();
    loop {
        hub.poll(&mut server).unwrap();
        for message in interest::drain(&mut client).0 {
            if let ServerMessage::Terrain(update) = message
                && let Some(page) = update.pages.first()
            {
                let light = page
                    .light
                    .as_ref()
                    .expect("ready light must accompany the initial terrain page");
                assert_eq!(light.terrain, page.digest);
                assert_eq!(light.pos, target);
                assert_eq!(hub.shared_light_diagnostics().unwrap().published, 1);
                return;
            }
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::yield_now();
    }
}

#[test]
fn light_cache_denial_keeps_the_connection_and_terrain_delivery_alive() {
    use torse_net::MemoryTransport;
    use torse_server::{
        bootstrap as base,
        network::*,
        visual::{VisualTerrainApproval, VisualTerrainLimits},
    };
    use torse_sim::replica::ReplicaScope;
    let dir = Directory::new();
    let mut server = fixture::create(&dir);
    let pages = fixture::pages(&dir);
    let target = SectionPos::ZERO;
    let scope = ReplicaScope {
        sections: [target].into(),
    };
    let mut hub =
        NetworkHub::<MemoryTransport>::new(server.binding(), base::network_limits()).unwrap();
    let mut light = limits();
    light.pages_per_poll = 64;
    light.field.cache_bytes = 1;
    hub.enable_shared_lighting(&server, pages.clone(), light, 1_000_000)
        .unwrap();
    let (id, mut client, _, _) = interest::attach(&mut hub, &mut server, scope.clone());
    hub.enable_visual_interest(
        &server,
        id,
        pages,
        VisualTerrainApproval::rectangular(target, target).unwrap(),
        scope,
        target,
        VisualTerrainLimits {
            max_sections: 1,
            cache_bytes: 128 * 1024,
            io_bytes: 32 * 1024 * 1024,
            batch_bytes: 64 * 1024,
            pages_per_poll: 1,
        },
    )
    .unwrap();
    let start = Instant::now();
    let mut terrain = false;
    loop {
        hub.poll(&mut server).unwrap();
        for message in interest::drain(&mut client).0 {
            if let ServerMessage::Terrain(update) = message {
                terrain |= !update.pages.is_empty();
            }
        }
        if terrain && hub.shared_light_diagnostics().unwrap().budget_denials > 0 {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::yield_now();
    }
    let started = hub.shared_light_diagnostics().unwrap().started;
    for _ in 0..20 {
        hub.poll(&mut server).unwrap();
        interest::drain(&mut client);
    }
    assert!(hub.contains(id));
    assert_eq!(hub.shared_light_diagnostics().unwrap().started, started);
    assert_eq!(hub.shared_light_diagnostics().unwrap().published, 0);
}

#[test]
fn inline_light_that_exceeds_the_wire_item_budget_is_deferred_without_disconnect() {
    use torse_net::{
        MemoryTransport,
        terrain::{TerrainLimits, TerrainMirror},
    };
    use torse_server::{
        bootstrap as base,
        network::*,
        visual::{VisualTerrainApproval, VisualTerrainLimits},
    };
    use torse_sim::replica::ReplicaScope;
    let dir = Directory::new();
    let (mut world, actions) = base::world().unwrap();
    let stone = world
        .registry()
        .default_state(&base::key("base:stone"))
        .unwrap();
    world
        .store_mut()
        .edit(BlockPos::new(1, 1, 1), stone)
        .unwrap();
    let mut server = fixture::create_with_world(&dir, (world, actions));
    let pages = fixture::pages(&dir);
    let target = SectionPos::ZERO;
    let scope = ReplicaScope {
        sections: [target].into(),
    };
    let mut network = base::network_limits();
    network.budget.max_items = 9000;
    let mut hub = NetworkHub::<MemoryTransport>::new(server.binding(), network).unwrap();
    let mut light = limits();
    light.pages_per_poll = 64;
    hub.enable_shared_lighting(&server, pages.clone(), light, 1_000_000)
        .unwrap();
    let (id, mut client, _, _) = interest::attach(&mut hub, &mut server, scope.clone());
    hub.enable_visual_interest(
        &server,
        id,
        pages,
        VisualTerrainApproval::rectangular(target, target).unwrap(),
        scope,
        target,
        VisualTerrainLimits {
            max_sections: 1,
            cache_bytes: 128 * 1024,
            io_bytes: 32 * 1024 * 1024,
            batch_bytes: 64 * 1024,
            pages_per_poll: 1,
        },
    )
    .unwrap();
    let mut mirror = TerrainMirror::new(
        server.binding(),
        16,
        server.authority().unwrap().world().store().bounds(),
        TerrainLimits {
            max_sections: 1,
            max_bytes: 128 * 1024,
        },
        None,
    )
    .unwrap();
    let start = Instant::now();
    let mut deferred = false;
    loop {
        hub.poll(&mut server).unwrap();
        for message in interest::drain(&mut client).0 {
            match message {
                ServerMessage::Terrain(update) => {
                    for page in &update.pages {
                        deferred |= page.light.is_none();
                    }
                    mirror.apply(update).unwrap();
                }
                ServerMessage::Light(update) => {
                    mirror.apply_light(update).unwrap();
                }
                _ => {}
            }
        }
        if mirror.light(target).is_some() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::yield_now();
    }
    assert!(deferred);
    assert!(hub.contains(id));
    assert_eq!(hub.shared_light_diagnostics().unwrap().published, 1);
}

/// CPU-ready measurements, not a claim about a monitor's first displayed frame.
#[test]
#[ignore = "release fixed-route measurement"]
fn shared_light_fixed_route_measurement() {
    use torse_net::{
        MemoryTransport,
        terrain::{TerrainLimits, TerrainMirror},
    };
    use torse_server::{
        bootstrap as base,
        network::*,
        visual::{VisualTerrainApproval, VisualTerrainLimits},
    };
    use torse_sim::replica::ReplicaScope;
    let dir = Directory::new();
    let (mut server, pages, _) = visual_fixture::create(&dir);
    let mut hub =
        NetworkHub::<MemoryTransport>::new(server.binding(), base::network_limits()).unwrap();
    let mut light = limits();
    light.input_sections = 512;
    light.input_bytes = 8 * 1024 * 1024;
    light.field.cache_bytes = 64 * 1024 * 1024;
    light.field.job = LightLimits {
        cells: 1_000_000,
        dependencies: 512,
        frontier: 1_000_000,
        bytes: 32 * 1024 * 1024,
    };
    hub.enable_shared_lighting(&server, pages.clone(), light, 20_000)
        .unwrap();
    let near = ReplicaScope {
        sections: [SectionPos::ZERO].into(),
    };
    let (id, mut client, _, _) = interest::attach(&mut hub, &mut server, near);
    let visual = VisualTerrainLimits {
        max_sections: 1,
        cache_bytes: 128 * 1024,
        io_bytes: 32 * 1024 * 1024,
        batch_bytes: 64 * 1024,
        pages_per_poll: 1,
    };
    let mut mirror = TerrainMirror::new(
        server.binding(),
        16,
        server.authority().unwrap().world().store().bounds(),
        TerrainLimits {
            max_sections: 1,
            max_bytes: 128 * 1024,
        },
        None,
    )
    .unwrap();
    println!(
        "sample,x,terrain_ms,light_ready_ms,max_poll_ms,pending_peak,cache_peak,complete,first_draw_ms"
    );
    for (sample, x) in [2, 3, 4, 5, 6, 5, 4, 3, 2].into_iter().enumerate() {
        let pos = SectionPos::new(x, 0, 0);
        let desired = ReplicaScope {
            sections: [pos].into(),
        };
        if sample == 0 {
            hub.enable_visual_interest(
                &server,
                id,
                pages.clone(),
                VisualTerrainApproval::rectangular(
                    SectionPos::new(2, 0, 0),
                    SectionPos::new(6, 0, 0),
                )
                .unwrap(),
                desired,
                pos,
                visual,
            )
            .unwrap();
        } else {
            hub.set_visual_interest(id, desired, pos).unwrap();
        }
        let start = Instant::now();
        let mut terrain_ms = None;
        let mut max_poll = Duration::ZERO;
        loop {
            let poll = Instant::now();
            hub.poll(&mut server).unwrap();
            max_poll = max_poll.max(poll.elapsed());
            for message in interest::drain(&mut client).0 {
                match message {
                    ServerMessage::Terrain(update) => {
                        mirror.apply(update).unwrap();
                    }
                    ServerMessage::Light(update) => {
                        mirror.apply_light(update).unwrap();
                    }
                    _ => {}
                }
            }
            if mirror.page(pos).is_some() {
                terrain_ms.get_or_insert(start.elapsed());
            }
            if let Some(result) = mirror.light(pos) {
                let d = hub.shared_light_diagnostics().unwrap();
                println!(
                    "{sample},{x},{:.6},{:.6},{:.6},{},{},{},NA",
                    terrain_ms.unwrap().as_secs_f64() * 1000.,
                    start.elapsed().as_secs_f64() * 1000.,
                    max_poll.as_secs_f64() * 1000.,
                    d.pending_peak,
                    d.cache_peak,
                    result.complete
                );
                assert_eq!(d.pending, 0);
                break;
            }
            if start.elapsed() > Duration::from_secs(30) {
                println!(
                    "{sample},{x},NA,NA,{:.6},0,0,false,NA",
                    max_poll.as_secs_f64() * 1000.
                );
                panic!("route sample incomplete");
            }
            std::thread::yield_now();
        }
    }
    let resources = hub.shared_light_resources().unwrap();
    println!(
        "input_peak_bytes={},reserved_bytes={},io_pending={}",
        resources.input_peak, resources.reserved_bytes, resources.io_pending
    );
}
