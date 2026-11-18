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

fn reopen(dir: &support::Directory, pages: &SectionPages) -> base::Files {
    let (world, actions) = base::world().unwrap();
    base::Files::open_file(
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
    .unwrap()
}

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
            other => panic!("parking job did not finish: {other:?}"),
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
        assert!(matches!(
            server.poll_generated_parking(&mut NoReplicas).unwrap(),
            ParkingProgress::Pending
        ));
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
        let duplicate = fixture::column(server.authority().unwrap(), x, 0, &template, false);
        assert_eq!(
            server
                .begin_generated_parking(pages.clone(), duplicate, LIMITS)
                .unwrap_err()
                .code,
            FaultCode::InvalidData
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
fn parking_reserves_ids_before_unrelated_allocations_without_rewriting_pages() {
    let dir = support::Directory::new();
    let (mut server, pages, template) = fixture::create(dir.acquire()).unwrap();
    for (x, entities) in [(3, false), (6, true)] {
        let mut input = fixture::column(server.authority().unwrap(), x, 0, &template, false);
        if !entities {
            let watermark = server
                .authority()
                .unwrap()
                .world()
                .entities()
                .next_entity_id();
            for page in &mut input {
                page.entities.clear();
                page.next_entity_id = watermark;
            }
        }
        server
            .begin_generated_parking(pages.clone(), input.clone(), LIMITS)
            .unwrap();
        let first = server
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id();
        let count = if entities { 3 } else { 0 };
        assert!(matches!(
            server.poll_generated_parking(&mut NoReplicas).unwrap(),
            ParkingProgress::Pending
        ));
        assert_eq!(
            server
                .authority()
                .unwrap()
                .world()
                .entities()
                .next_entity_id(),
            first + count
        );
        if entities {
            server.checkpoint().unwrap();
        }
        let unrelated = fixture::column(server.authority().unwrap(), x + 1, 0, &template, false);
        server
            .install_section_batch(&pages, unrelated, &mut NoReplicas)
            .unwrap();
        let watermark = server
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id();
        let (commit, syncs, _) = finish(&mut server).unwrap();
        assert!(matches!(commit, CommitStep::Durable(_)));
        assert_eq!(syncs, 1);
        let host = server.authority().unwrap();
        let mut ids = Vec::new();
        for page in input {
            let reference = &host.parked_sections()[&page.sections[0].pos];
            assert_eq!(pages.get(reference.digest).unwrap(), page);
            ids.extend(reference.entities.iter().copied());
        }
        assert_eq!(
            ids,
            (first..first + count).map(EntityId).collect::<Vec<_>>()
        );
        assert_eq!(host.world().entities().next_entity_id(), watermark);
    }
    let expected = server.authority().unwrap().parked_sections().clone();
    drop(server);
    let mut recovered = reopen(&dir, &pages);
    assert_eq!(recovered.authority().unwrap().parked_sections(), &expected);
    for (&pos, reference) in expected
        .iter()
        .filter(|(_, page)| !page.entities.is_empty())
    {
        recovered
            .reload_section(&pages, pos, &mut NoReplicas)
            .unwrap();
        for id in &reference.entities {
            assert!(
                recovered
                    .authority()
                    .unwrap()
                    .world()
                    .entities()
                    .handle_of(*id)
                    .is_some()
            );
        }
    }
    drop(recovered);
    let recovered = reopen(&dir, &pages);
    let entities = recovered.authority().unwrap().world().entities();
    for id in expected.values().flat_map(|reference| &reference.entities) {
        assert!(entities.handle_of(*id).is_some());
    }
}

#[test]
fn retirement_batches_sync_once_and_recheck_foldback() {
    use torse_server::residency::{ResidencyConfig, ResidencyProgress};

    let dir = support::Directory::new();
    let (mut server, pages, template) = fixture::create(dir.acquire()).unwrap();
    let old = fixture::column(server.authority().unwrap(), 0, 0, &template, false);
    let keep = old[0].sections[0].pos;
    server
        .install_section_batch(&pages, old, &mut NoReplicas)
        .unwrap();
    let forward = fixture::column(server.authority().unwrap(), 1, 0, &template, false);
    let near = forward[0].sections[0].pos;
    server
        .begin_generated_parking(pages.clone(), forward, LIMITS)
        .unwrap();
    finish(&mut server).unwrap();
    server
        .enable_residency(
            ResidencyConfig {
                max_resident: 48,
                max_io_bytes: pages.budget().max_bytes * 4 * 25,
            },
            pages.clone(),
            None,
            &[near],
        )
        .unwrap();
    server.set_residency_batch_pages(24).unwrap();
    let start = Instant::now();
    assert!(matches!(
        server.poll_residency(&mut NoReplicas).unwrap(),
        ResidencyProgress::Pending
    ));
    let in_flight = server.residency_io_diagnostics().unwrap();
    assert!(in_flight.read_pending && in_flight.retirement_age.is_some());
    assert!(in_flight.retirement_reserved_bytes <= pages.budget().max_bytes * 4 * 24);
    server.set_residency_interest(&[keep, near]).unwrap();
    let mut retired = 0;
    let mut page_syncs = 0;
    while retired < 23
        || server
            .authority()
            .unwrap()
            .world()
            .store()
            .section(near)
            .is_none()
    {
        let before = server.flush_metrics().calls;
        match server.poll_residency(&mut NoReplicas).unwrap() {
            ResidencyProgress::Batch(report) => {
                assert!(report.failure.is_none(), "{report:?}");
                page_syncs += report.page_file_syncs;
                retired += report.committed.iter().filter(|pos| pos.x == 0).count();
                if report.page_file_syncs != 0 {
                    assert_eq!(server.flush_metrics().calls - before, 1);
                }
            }
            ResidencyProgress::Pending => std::thread::yield_now(),
            other => panic!("unexpected retirement result: {other:?}"),
        }
        assert!(start.elapsed() < Duration::from_secs(10));
    }
    assert_eq!(retired, 23);
    assert_eq!(page_syncs, 1);
    assert!(
        server
            .residency_io_diagnostics()
            .unwrap()
            .retirement_age
            .is_none()
    );
    assert!(
        server
            .authority()
            .unwrap()
            .world()
            .store()
            .section(keep)
            .is_some()
    );
    let expected = server.authority().unwrap().parked_sections().clone();
    drop(server);
    let recovered = reopen(&dir, &pages);
    let host = recovered.authority().unwrap();
    assert_eq!(host.parked_sections(), &expected);
    assert!(host.world().store().section(keep).is_some());
    assert!(host.world().store().section(near).is_some());
}

#[test]
fn parking_unpublished_crash_child() {
    let Some(path) = std::env::var_os("TORSE_TEST_UNPUBLISHED_PARKING") else {
        return;
    };
    let directory = torse_server::WorldDirectory::acquire(path, base::WORLD, 8192).unwrap();
    let (mut server, pages, template) = fixture::create(directory).unwrap();
    let input = fixture::column(server.authority().unwrap(), 3, 0, &template, false);
    let paths: Vec<_> = input
        .iter()
        .map(|p| pages.path(pages.digest(p).unwrap()))
        .collect();
    server
        .begin_generated_parking(pages, input, LIMITS)
        .unwrap();
    assert!(matches!(
        server.poll_generated_parking(&mut NoReplicas).unwrap(),
        ParkingProgress::Pending
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !paths.iter().all(|path| path.is_file()) {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    std::process::exit(76);
}

#[test]
fn written_unpublished_bundle_recovers_only_its_reservation() {
    let dir = support::Directory::new();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "parking_unpublished_crash_child"])
        .env("TORSE_TEST_UNPUBLISHED_PARKING", &dir.0)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(76));
    let pages = SectionPages::new(
        dir.0.join("sections"),
        base::budget(),
        base::codec_limits(),
        4096,
    )
    .unwrap();
    let server = reopen(&dir, &pages);
    let host = server.authority().unwrap();
    assert_eq!(host.world().entities().next_entity_id(), 4);
    assert!(host.parked_sections().is_empty());
    assert!(host.world().entities().iter().next().is_none());
}

#[test]
fn canceled_parking_does_not_return_reserved_ids() {
    let dir = support::Directory::new();
    let (mut server, pages, template) = fixture::create(dir.acquire()).unwrap();
    let input = fixture::column(server.authority().unwrap(), 3, 0, &template, false);
    server
        .begin_generated_parking(pages.clone(), input, LIMITS)
        .unwrap();
    assert!(matches!(
        server.poll_generated_parking(&mut NoReplicas).unwrap(),
        ParkingProgress::Pending
    ));
    server.cancel_generated_parking().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match server.poll_generated_parking(&mut NoReplicas).unwrap() {
            ParkingProgress::Pending => std::thread::yield_now(),
            ParkingProgress::Canceled => break,
            other => panic!("unexpected parking progress: {other:?}"),
        }
        assert!(Instant::now() < deadline);
    }
    assert!(server.authority().unwrap().parked_sections().is_empty());
    assert_eq!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id(),
        4
    );
    drop(server);
    let recovered = reopen(&dir, &pages);
    assert_eq!(
        recovered
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id(),
        4
    );
}

#[test]
fn generated_parking_rejects_a_target_installed_during_background_io() {
    let dir = support::Directory::new();
    let (mut server, pages, template) = fixture::create(dir.acquire()).unwrap();
    let input = fixture::column(server.authority().unwrap(), 3, 0, &template, false);
    let target = input[0].sections[0].pos;
    server
        .begin_generated_parking(pages.clone(), input.clone(), LIMITS)
        .unwrap();
    assert!(matches!(
        server.poll_generated_parking(&mut NoReplicas).unwrap(),
        ParkingProgress::Pending
    ));
    let installation = fixture::column(server.authority().unwrap(), 3, 0, &template, false);
    server
        .install_section(&pages, installation[0].clone(), &mut NoReplicas)
        .unwrap();
    let host = server.authority().unwrap();
    let cursor = host.cursor();
    let before = host
        .world()
        .capture_section(target, base::budget())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match server.poll_generated_parking(&mut NoReplicas).unwrap() {
            ParkingProgress::Pending => std::thread::yield_now(),
            ParkingProgress::Stale { position } if position == target => break,
            other => panic!("unexpected parking progress: {other:?}"),
        }
        assert!(Instant::now() < deadline);
    }
    let host = server.authority().unwrap();
    assert_eq!(host.cursor(), cursor);
    assert!(host.parked_sections().is_empty());
    assert_eq!(
        host.world()
            .capture_section(target, base::budget())
            .unwrap(),
        before
    );
}

#[test]
#[ignore = "real filesystem retirement timing; run in Release"]
fn compare_serial_and_batched_retirement() {
    use torse_server::residency::{ResidencyConfig, ResidencyProgress};

    for sample in 0..3 {
        for batched in [false, true] {
            let dir = support::Directory::new();
            let (mut server, pages, template) = fixture::create(dir.acquire()).unwrap();
            let column = fixture::column(server.authority().unwrap(), 0, 0, &template, false);
            let positions: Vec<_> = column.iter().map(|page| page.sections[0].pos).collect();
            server
                .install_section_batch(&pages, column, &mut NoReplicas)
                .unwrap();
            if batched {
                server
                    .enable_residency(
                        ResidencyConfig {
                            max_resident: 24,
                            max_io_bytes: pages.budget().max_bytes * 4 * 25,
                        },
                        pages.clone(),
                        None,
                        &[],
                    )
                    .unwrap();
                server.set_residency_batch_pages(24).unwrap();
            }
            let flushes = server.flush_metrics().calls;
            let start = Instant::now();
            let mut max_call = Duration::ZERO;
            let mut written = 0;
            if batched {
                let mut retired = 0;
                while retired < positions.len() {
                    let called = Instant::now();
                    let progress = server.poll_residency(&mut NoReplicas).unwrap();
                    max_call = max_call.max(called.elapsed());
                    match progress {
                        ResidencyProgress::Batch(report) => {
                            assert!(report.failure.is_none(), "{report:?}");
                            retired += report.committed.len();
                            written += report.page_file_syncs;
                        }
                        ResidencyProgress::Pending => std::thread::yield_now(),
                        other => panic!("unexpected retirement result: {other:?}"),
                    }
                    assert!(start.elapsed() < Duration::from_secs(10));
                }
            } else {
                for pos in positions {
                    let called = Instant::now();
                    assert!(matches!(
                        server.unload_section(&pages, pos, &mut NoReplicas).unwrap(),
                        CommitStep::Durable(_)
                    ));
                    max_call = max_call.max(called.elapsed());
                    // Each new standalone name requires one sync in SectionPages::put.
                    assert!(
                        pages
                            .path(server.authority().unwrap().parked_sections()[&pos].digest)
                            .is_file()
                    );
                    written += 1;
                }
            }
            assert_eq!(server.authority().unwrap().parked_sections().len(), 24);
            assert_eq!(written, if batched { 1 } else { 24 });
            let elapsed = start.elapsed();
            let log_syncs = server.flush_metrics().calls - flushes;
            if batched {
                // Journal admission may shorten the accepted prefix.
                assert!((1..24).contains(&log_syncs));
            } else {
                assert_eq!(log_syncs, 24);
            }
            println!(
                "{sample}, {batched}, {:.4}, {:.4}, {written}, {}",
                max_call.as_secs_f64() * 1000.,
                elapsed.as_secs_f64() * 1000.,
                server.flush_metrics().calls - flushes
            );
        }
    }
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
    let before_failure = server
        .authority()
        .unwrap()
        .world()
        .entities()
        .next_entity_id();
    assert_eq!(before_failure, 4);
    let input = fixture::column(server.authority().unwrap(), 3, 0, &template, false);
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
    assert!(host.cursor().seq > before.seq);
    assert_eq!(host.world().entities().next_entity_id(), before_failure);
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
            assert_eq!(journal_flushes, if direct { 2 } else { 25 });
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
