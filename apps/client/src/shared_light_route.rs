//! File-backed host -> wire -> terrain mirror -> mesh -> GPU route fixture.
#[path = "../../server/tests/support/visual.rs"]
#[allow(dead_code, unused_imports)]
mod fixture;
use crate::{camera::Camera, renderer::Renderer, scene::Scene, support::Directory};
use std::time::{Duration, Instant};
use torse_net::{
    MemoryTransport,
    terrain::{TerrainLimits, TerrainMirror},
};
use torse_server::{
    bootstrap as base,
    lighting::HostLightLimits,
    network::*,
    visual::{VisualTerrainApproval, VisualTerrainLimits},
};
use torse_sim::replica::ReplicaScope;
use torse_types::*;
use torse_world_services::lighting::{LightFieldLimits, LightLimits};

#[test]
#[ignore = "file-backed GPU route measurement"]
fn shared_light_route_to_offscreen_draw() {
    let dir = Directory::new();
    let (mut server, pages, _) = fixture::create(&dir);
    let mut hub =
        NetworkHub::<MemoryTransport>::new(server.binding(), base::network_limits()).unwrap();
    hub.enable_shared_lighting(
        &server,
        pages.clone(),
        HostLightLimits {
            input_sections: 512,
            input_bytes: 8 * 1024 * 1024,
            pages_per_poll: 1,
            field: LightFieldLimits {
                sections: 8,
                cache_bytes: 64 * 1024 * 1024,
                consumers: 4,
                interests: 32,
                job: LightLimits {
                    cells: 1_000_000,
                    dependencies: 512,
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
    let (id, mut client, near, _) = fixture::interest::attach(&mut hub, &mut server, near);
    let mut scene = Scene::from_snapshot(near.state()).unwrap();
    scene.enable_lighting().unwrap();
    let mut renderer = Renderer::new(128, 128).unwrap();
    renderer.set_palette(scene.palette());
    let output =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.work/shared-light-route-gpu");
    std::fs::create_dir_all(&output).unwrap();
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
        "sample,x,terrain_ms,terrain_to_first_draw_ms,light_ready_ms,light_to_draw_ms,pending_peak,cache_peak,complete"
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
        let camera = Camera::new(
            [x as f32 * 16. + 8., 16., 24.],
            [x as f32 * 16. + 8., 0., 8.],
        );
        let start = Instant::now();
        let mut terrain_time = None;
        let mut first_draw = None;
        loop {
            hub.poll(&mut server).unwrap();
            for message in fixture::drain(&mut client).0 {
                let changes = match message {
                    ServerMessage::Terrain(update) => mirror.apply(update).unwrap(),
                    ServerMessage::Light(update) => mirror.apply_light(update).unwrap(),
                    _ => continue,
                };
                let received = Instant::now();
                scene
                    .try_apply_display(
                        near.state(),
                        &mirror,
                        changes.changed.into_iter().chain(changes.removed),
                    )
                    .unwrap();
                if mirror.page(pos).is_some() {
                    terrain_time.get_or_insert(received);
                }
                if mirror.page(pos).is_none() {
                    continue;
                }
                scene.refresh_meshes();
                let (vertices, indices) = scene.section_geometry(pos).unwrap();
                assert!(renderer.upload_section(pos, &Scene::vertex_bytes(&vertices), &indices));
                assert!(renderer.render(camera.view_projection(1.), camera.eye));
                renderer
                    .save_png(output.join(format!("{sample:02}.png")).to_str().unwrap())
                    .unwrap();
                first_draw.get_or_insert_with(|| terrain_time.unwrap().elapsed());
                if let Some(light) = mirror.light(pos) {
                    let d = hub.shared_light_diagnostics().unwrap();
                    println!(
                        "{sample},{x},{:.6},{:.6},{:.6},{:.6},{},{},{}",
                        terrain_time.unwrap().duration_since(start).as_secs_f64() * 1000.,
                        first_draw.unwrap().as_secs_f64() * 1000.,
                        received.duration_since(start).as_secs_f64() * 1000.,
                        received.elapsed().as_secs_f64() * 1000.,
                        d.pending_peak,
                        d.cache_peak,
                        light.complete
                    );
                    assert_eq!(d.pending, 0);
                    break;
                }
            }
            if mirror.light(pos).is_some() {
                break;
            }
            if start.elapsed() > Duration::from_secs(30) {
                println!("{sample},{x},NA,NA,NA,NA,0,0,false");
                panic!("unfinished draw sample");
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
