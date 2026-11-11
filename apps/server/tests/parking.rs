#[allow(dead_code)]
#[path = "../examples/dense_install_baseline/workload.rs"]
mod fixture;
mod support;
#[allow(dead_code, unused_imports)]
#[path = "support/visual.rs"]
mod visual;

use std::time::{Duration, Instant};
use torse_server::{
    CommitStep, HostAuthority, NoReplicas, bootstrap as base,
    execution::{ExecutionRecovery, installs::SectionInstallLimits},
    parking::ParkingProgress,
    section_pages::SectionPages,
};
use torse_types::*;

const LIMITS: SectionInstallLimits = SectionInstallLimits {
    max_pages: 24,
    max_bytes: 8 * 1024 * 1024,
};

fn finish(server: &mut base::Files) -> Result<(CommitStep, u32, Duration), Fault> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut max_poll = Duration::ZERO;
    loop {
        let start = Instant::now();
        let progress = server.poll_generated_parking(&mut NoReplicas)?;
        max_poll = max_poll.max(start.elapsed());
        match progress {
            ParkingProgress::Pending => {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            ParkingProgress::Finished {
                commit,
                page_file_syncs,
            } => {
                return Ok((commit, page_file_syncs, max_poll));
            }
            ParkingProgress::Idle => panic!("parking job disappeared"),
        }
    }
}

#[test]
fn complete_generated_columns_park_across_ticks_and_recover_without_activation() {
    let dir = support::Directory::new();
    let (mut server, pages, template) = fixture::create(dir.acquire()).unwrap();
    let near = fixture::column(server.authority().unwrap(), 0, 0, &template, true);
    server
        .install_section_batch(&pages, near, &mut NoReplicas)
        .unwrap();
    let mut expected = Vec::new();
    for (index, x) in (3..5).enumerate() {
        let host = server.authority().unwrap();
        let input = fixture::column(host, x, 0, &template, false);
        let old_id = host.world().entities().next_entity_id();
        let before = host.cursor();
        let start = Instant::now();
        server
            .begin_generated_parking(pages.clone(), input.clone(), LIMITS)
            .unwrap();
        assert_eq!(server.authority().unwrap().cursor(), before);
        assert_eq!(
            server
                .authority()
                .unwrap()
                .world()
                .entities()
                .next_entity_id(),
            old_id
        );
        if index == 0 {
            server.drive_tick(&mut NoReplicas, 1024).unwrap();
        }
        let flush = server.flush_metrics().calls;
        let (result, file_syncs, _) = finish(&mut server).unwrap();
        assert!(matches!(result, CommitStep::Durable(_)));
        assert_eq!(file_syncs, 1);
        assert_eq!(server.flush_metrics().calls - flush, 1);
        let host = server.authority().unwrap();
        assert_eq!(host.world().store().len(), 24);
        assert_eq!(host.world().entities().iter().count(), 3);
        assert_eq!(host.world().entities().next_entity_id(), old_id + 3);
        for page in &input {
            let pos = page.sections[0].pos;
            let saved = &host.parked_sections()[&pos];
            assert!(host.world().store().section(pos).is_none());
            assert_eq!(pages.get(saved.digest).unwrap(), *page);
        }
        assert!(
            server
                .begin_generated_parking(pages.clone(), input.clone(), LIMITS)
                .is_err()
        );
        eprintln!(
            "direct parking 24 pages in {:?}, {file_syncs} bundle sync and 1 journal flush",
            start.elapsed()
        );
        expected.extend(input);
    }
    let desired = visual::scope(3, 1);
    let (mut hub, id, mut client, _, _) = visual::connect(&mut server, visual::scope(0, 1));
    let mut mirror = visual::mirror(&server);
    let pos = SectionPos::new(3, 0, 0);
    hub.enable_visual_interest(
        &server,
        id,
        pages.clone(),
        visual::approval(),
        desired,
        pos,
        visual::limits(),
    )
    .unwrap();
    visual::fill(&mut hub, &mut server, &mut client, &mut mirror);
    assert!(mirror.page(pos).unwrap().entities.is_empty());
    assert_eq!(
        mirror.page(pos).unwrap().sections,
        expected
            .iter()
            .find(|p| p.sections[0].pos == pos)
            .unwrap()
            .sections
    );
    assert_eq!(server.authority().unwrap().world().store().len(), 24);
    drop(hub);
    let watermark = server
        .authority()
        .unwrap()
        .world()
        .entities()
        .next_entity_id();
    drop(server);
    let (world, actions) = base::world().unwrap();
    let mut recovered = base::Files::open_file(
        dir.acquire(),
        world.coordinator().compatibility(),
        base::Codec::new(base::codec_limits()).unwrap(),
        fixture::file_policy(),
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                world.registry(),
                world.schemas(),
                actions,
                Vec::new(),
                fixture::limits(),
            )
            .with_section_pages(pages.clone()))
        },
    )
    .unwrap();
    let host = recovered.authority().unwrap();
    assert_eq!(host.world().store().len(), 24);
    assert_eq!(host.world().entities().iter().count(), 3);
    assert_eq!(host.world().entities().next_entity_id(), watermark);
    assert_eq!(host.parked_sections().len(), 48);
    for page in expected {
        let pos = page.sections[0].pos;
        let saved = recovered.authority().unwrap().parked_sections()[&pos].digest;
        assert_eq!(pages.get(saved).unwrap(), page);
        recovered
            .reload_section(&pages, pos, &mut NoReplicas)
            .unwrap();
        for entity in page.entities {
            assert!(
                recovered
                    .authority()
                    .unwrap()
                    .world()
                    .entities()
                    .handle_of(entity.id)
                    .is_some()
            );
        }
    }
    assert!(recovered.authority().unwrap().parked_sections().is_empty());
}

#[test]
fn invalid_allocator_budget_and_file_capacity_do_not_publish_a_prefix() {
    let dir = support::Directory::new();
    let (mut server, pages, template) = fixture::create(dir.acquire()).unwrap();
    let input = fixture::column(server.authority().unwrap(), 3, 0, &template, false);
    let before = server.authority().unwrap().cursor();
    assert!(
        server
            .begin_generated_parking(
                pages.clone(),
                input.clone(),
                SectionInstallLimits {
                    max_bytes: 64,
                    ..LIMITS
                }
            )
            .is_err()
    );
    let tiny_pages = SectionPages::new(
        dir.0.join("sections"),
        base::budget(),
        base::codec_limits(),
        24,
    )
    .unwrap();
    server
        .begin_generated_parking(tiny_pages, input.clone(), LIMITS)
        .unwrap();
    assert_eq!(
        finish(&mut server).unwrap_err().code,
        FaultCode::ResourceLimit
    );
    let mut invalid = input.clone();
    invalid.last_mut().unwrap().next_entity_id += 1;
    server
        .begin_generated_parking(pages.clone(), invalid, LIMITS)
        .unwrap();
    assert_eq!(
        finish(&mut server).unwrap_err().code,
        FaultCode::InvalidData
    );
    let host = server.authority().unwrap();
    assert_eq!(host.cursor(), before);
    assert_eq!(host.world().entities().next_entity_id(), 1);
    assert!(host.world().store().is_empty());
    assert!(host.parked_sections().is_empty());
    server
        .begin_generated_parking(pages, input, LIMITS)
        .unwrap();
    assert!(matches!(
        finish(&mut server).unwrap().0,
        CommitStep::Durable(_)
    ));
}

#[test]
fn near_page_reads_continue_with_a_generated_parking_job_pending() {
    use torse_server::residency::{ResidencyConfig, ResidencyProgress};

    let dir = support::Directory::new();
    let (mut server, pages, template) = fixture::create(dir.acquire()).unwrap();
    let input = fixture::column(server.authority().unwrap(), 3, 0, &template, false);
    server
        .begin_generated_parking(pages.clone(), input, LIMITS)
        .unwrap();
    finish(&mut server).unwrap();
    let pos = SectionPos::new(3, -3, 0);
    server
        .enable_residency(
            ResidencyConfig {
                max_resident: 4,
                max_io_bytes: pages.budget().max_bytes * 4,
            },
            pages.clone(),
            None,
            &[pos],
        )
        .unwrap();
    let input = fixture::column(server.authority().unwrap(), 4, 0, &template, false);
    server
        .begin_generated_parking(pages, input, LIMITS)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match server.poll_residency(&mut NoReplicas).unwrap() {
            ResidencyProgress::Pending => {}
            ResidencyProgress::Published(CommitStep::Durable(_)) => break,
            other => panic!("unexpected near residency progress: {other:?}"),
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(
        server
            .authority()
            .unwrap()
            .world()
            .store()
            .section(pos)
            .is_some()
    );
    assert!(matches!(
        finish(&mut server).unwrap().0,
        CommitStep::Durable(_)
    ));
}

#[test]
#[ignore = "compare on a local file volume in Release"]
fn compare_direct_parking_with_install_then_unload_and_a_large_index() {
    for direct in [false, true] {
        let dir = support::Directory::new();
        let (mut server, pages, template) = fixture::create(dir.acquire()).unwrap();
        let near = fixture::column(server.authority().unwrap(), 0, 0, &template, true);
        server
            .install_section_batch(&pages, near, &mut NoReplicas)
            .unwrap();
        for x in 1..=if direct { 102 } else { 6 } {
            let host = server.authority().unwrap();
            let index = host.parked_sections().len();
            let input = fixture::column(host, x, 0, &template, false);
            let before = server.flush_metrics().calls;
            let start = Instant::now();
            let (page_file_syncs, max_host_call) = if direct {
                server
                    .begin_generated_parking(pages.clone(), input, LIMITS)
                    .unwrap();
                let submit = start.elapsed();
                let (commit, syncs, max_poll) = finish(&mut server).unwrap();
                assert!(matches!(commit, CommitStep::Durable(_)));
                (syncs, submit.max(max_poll))
            } else {
                let positions: Vec<_> = input.iter().map(|p| p.sections[0].pos).collect();
                server
                    .install_section_batch(&pages, input, &mut NoReplicas)
                    .unwrap();
                for pos in positions {
                    server.unload_section(&pages, pos, &mut NoReplicas).unwrap();
                }
                (24, start.elapsed())
            };
            let elapsed = start.elapsed();
            let journal_flushes = server.flush_metrics().calls - before;
            assert_eq!(journal_flushes, if direct { 1 } else { 25 });
            assert_eq!(page_file_syncs, if direct { 1 } else { 24 });
            assert_eq!(server.authority().unwrap().world().store().len(), 24);
            if x <= 6 || x > 96 {
                println!(
                    "parking_sample,{direct},{index},{},{},{page_file_syncs},{journal_flushes}",
                    elapsed.as_nanos(),
                    max_host_call.as_nanos()
                );
            }
        }
    }
}
