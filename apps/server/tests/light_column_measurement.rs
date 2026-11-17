//! File-backed column scheduling comparison; run explicitly in Release.
#[path = "support/streaming.rs"]
#[allow(dead_code)]
mod fixture;
use sha2::{Digest as _, Sha256};
mod support;
use std::time::{Duration, Instant};
use support::Directory;
use torse_server::{
    NoReplicas,
    lighting::{HostLightField, HostLightLimits},
};
use torse_types::SectionPos;
use torse_world::{SnapshotCells, StateEntry};
use torse_world_services::lighting::{LightFieldLimits, LightLimits};

#[test]
#[ignore = "release file-backed column scheduling measurement"]
fn shared_light_column_progress_measurement() {
    println!(
        "sample,near_ms,all_ms,max_poll_ms,polls,far_ready_at_near,published,cache_peak,input_peak,output_sha256"
    );
    let mut expected = None;
    for sample in 0..6 {
        let dir = Directory::new();
        let mut server = fixture::create(&dir);
        let pages = fixture::pages(&dir);
        let near = SectionPos::ZERO;
        let mut targets = vec![near];
        for x in -40..-16 {
            let pos = SectionPos::new(x, 0, 0);
            let world = server.authority().unwrap().world();
            let mut page = fixture::generated(world, pos);
            let stone = world
                .registry()
                .default_state(&torse_server::bootstrap::key("base:stone"))
                .unwrap();
            page.states = vec![StateEntry {
                id: stone,
                block: torse_server::bootstrap::key("base:stone"),
                values: vec![],
            }];
            page.sections[0].cells = SnapshotCells::Dense(
                (0..4096)
                    .map(|i| {
                        if (i + x.unsigned_abs() as usize) % 7 < 3 {
                            stone
                        } else {
                            torse_world::BlockStateId::EMPTY
                        }
                    })
                    .collect(),
            );
            server
                .install_section(&pages, page, &mut NoReplicas)
                .unwrap();
            server.unload_section(&pages, pos, &mut NoReplicas).unwrap();
            targets.push(pos);
        }
        targets.sort_unstable();
        let mut light = HostLightField::new(
            server.authority().unwrap(),
            pages,
            HostLightLimits {
                input_sections: 512,
                input_bytes: 2_000_000,
                pages_per_poll: 4,
                field: LightFieldLimits {
                    sections: 32,
                    cache_bytes: 256 * 1024 * 1024,
                    solver_cache_bytes: 256 * 1024 * 1024,
                    consumers: 4,
                    interests: 64,
                    job: LightLimits {
                        cells: 200_000,
                        dependencies: 64,
                        frontier: 200_000,
                        bytes: 5_000_000,
                    },
                },
            },
        )
        .unwrap();
        // Keep the entry point identical on both baseline and candidate.
        light.subscribe(1, &targets).unwrap();
        let start = Instant::now();
        let mut near_ms = None;
        let mut ready_at_near = 0;
        let mut max_poll = Duration::ZERO;
        let mut polls = 0;
        loop {
            let poll = Instant::now();
            light.poll(server.authority().unwrap(), 1_000_000).unwrap();
            max_poll = max_poll.max(poll.elapsed());
            polls += 1;
            if near_ms.is_none()
                && light
                    .publication(1, near, server.authority().unwrap())
                    .is_some()
            {
                near_ms = Some(start.elapsed().as_secs_f64() * 1000.);
                ready_at_near = targets
                    .iter()
                    .filter(|&&p| {
                        p != near
                            && light.input_status(p)
                                == Some(torse_server::lighting::LightInputStatus::Ready)
                    })
                    .count();
            }
            if light.diagnostics().pending == 0 {
                let all_ms = start.elapsed().as_secs_f64() * 1000.;
                let d = light.diagnostics();
                let mut hash = Sha256::new();
                for &target in &targets {
                    let publication = light
                        .publication(1, target, server.authority().unwrap())
                        .unwrap();
                    hash.update(target.x.to_le_bytes());
                    hash.update(target.y.to_le_bytes());
                    hash.update(target.z.to_le_bytes());
                    hash.update(publication.emitted.levels());
                    hash.update(publication.sky.levels());
                }
                let output = format!("{:x}", hash.finalize());
                assert_eq!(expected.get_or_insert_with(|| output.clone()), &output);
                println!(
                    "{sample},{:.6},{:.6},{:.6},{polls},{ready_at_near},{},{},{},{output}",
                    near_ms.unwrap(),
                    all_ms,
                    max_poll.as_secs_f64() * 1000.,
                    d.published,
                    d.cache_peak,
                    light.resource_diagnostics().input_peak
                );
                assert_eq!(d.published, targets.len() as u64);
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "unfinished sample {sample}: {:?}",
                light.diagnostics()
            );
            std::thread::yield_now();
        }
    }
}
