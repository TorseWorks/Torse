#[path = "support/visual.rs"]
mod fixture;
mod support;
use fixture::*;
use support::Directory;
use torse_net::Transport;
use torse_server::{HostAuthority, network::*};
use torse_sim::replica::ReplicaScope;
use torse_types::*;

#[test]
fn unchanged_terrain_remains_visible_while_parking_and_reloading() {
    let dir = Directory::new();
    let (mut server, pages, _) = create(&dir);
    let desired = scope(2, 2);
    for &pos in &desired.sections {
        server
            .reload_section(&pages, pos, &mut torse_server::NoReplicas)
            .unwrap();
    }
    let (mut hub, id, mut client, _, _) = connect(&mut server, scope(0, 1));
    let mut display = mirror(&server);
    hub.enable_visual_interest(
        &server,
        id,
        pages.clone(),
        approval(),
        desired.clone(),
        SectionPos::new(2, 0, 0),
        limits(),
    )
    .unwrap();
    fill(&mut hub, &mut server, &mut client, &mut display);
    let expected: Vec<_> = display.pages().cloned().collect();
    for loading in [false, true] {
        for &pos in &desired.sections {
            if loading {
                server.reload_section(&pages, pos, &mut hub).unwrap();
            } else {
                server.unload_section(&pages, pos, &mut hub).unwrap();
            }
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            hub.poll(&mut server).unwrap();
            for message in drain(&mut client).0 {
                if let ServerMessage::Terrain(update) = message {
                    assert!(update.unavailable.is_empty());
                    display.apply(update).unwrap();
                }
            }
            assert_eq!(display.pages().cloned().collect::<Vec<_>>(), expected);
            if !hub.visual_terrain_diagnostics(id).unwrap().io_pending {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
    // A proven transition retains the old page across dispatch. A failed
    // verification still explicitly removes it.
    let pos = SectionPos::new(2, 0, 0);
    server.unload_section(&pages, pos, &mut hub).unwrap();
    let digest = server.authority().unwrap().parked_sections()[&pos].digest;
    std::fs::write(pages.path(digest), b"damaged after parking").unwrap();
    hub.poll(&mut server).unwrap();
    for message in drain(&mut client).0 {
        if let ServerMessage::Terrain(update) = message {
            assert!(update.unavailable.is_empty());
            display.apply(update).unwrap();
        }
    }
    assert!(display.page(pos).is_some());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while display.page(pos).is_some() {
        hub.poll(&mut server).unwrap();
        for message in drain(&mut client).0 {
            if let ServerMessage::Terrain(update) = message {
                assert!(
                    update
                        .unavailable
                        .contains(&(pos, TerrainUnavailable::Corrupt))
                );
                display.apply(update).unwrap();
            }
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
}

#[test]
fn full_height_explicit_approval_delivers_only_the_requested_pages() {
    let dir = Directory::new();
    let (mut server, pages, expected) = create(&dir);
    let approved = ReplicaScope {
        sections: (APPROVAL_MIN.x..=APPROVAL_MAX.x)
            .flat_map(|x| {
                (APPROVAL_MIN.y..=APPROVAL_MAX.y).flat_map(move |y| {
                    (APPROVAL_MIN.z..=APPROVAL_MAX.z).map(move |z| SectionPos::new(x, y, z))
                })
            })
            .collect(),
    };
    assert_eq!(approved.sections.len(), 101_400);
    let (mut hub, id, mut client, _, _) = connect(&mut server, scope(0, 1));
    let desired = scope(2, 8);
    let mut budget = limits();
    budget.pages_per_poll = 1;
    budget.batch_bytes = 16 * 1024;
    hub.enable_visual_interest(
        &server,
        id,
        pages,
        approved,
        desired.clone(),
        SectionPos::new(9, 0, 0),
        budget,
    )
    .unwrap();
    let mut display = mirror(&server);
    let mut received = Vec::new();
    let start = std::time::Instant::now();
    while received.len() < 8 {
        hub.poll(&mut server).unwrap();
        for message in drain(&mut client).0 {
            if let ServerMessage::Terrain(update) = message {
                assert!(update.pages.len() <= 1);
                for page in &update.pages {
                    assert!(torse_net::terrain::terrain_bytes(&page.terrain) <= budget.batch_bytes);
                    received.push(page.terrain.sections[0].pos.x);
                }
                display.apply(update).unwrap();
            }
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        std::thread::yield_now();
    }
    assert_eq!(received, (2..10).rev().collect::<Vec<_>>());
    assert_eq!(display.desired(), &desired.sections);
    for pos in desired.sections {
        assert_eq!(display.page(pos), expected.get(&pos));
    }
}

#[test]
fn saved_dense_pages_follow_forward_turn_and_revisit_without_simulation_growth() {
    let dir = Directory::new();
    let (mut server, pages, expected) = create(&dir);
    let (mut hub, id, mut client, mut near, _) = connect(&mut server, scope(0, 1));
    let mut display = mirror(&server);
    let approved = approval();
    hub.enable_visual_interest(
        &server,
        id,
        pages.clone(),
        approved.clone(),
        scope(2, 8),
        SectionPos::new(2, 0, 0),
        limits(),
    )
    .unwrap();
    let cursor = server.authority().unwrap().cursor();
    let ids = server
        .authority()
        .unwrap()
        .world()
        .entities()
        .next_entity_id();
    fill(&mut hub, &mut server, &mut client, &mut display);
    assert_eq!(server.authority().unwrap().cursor(), cursor);
    assert_eq!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id(),
        ids
    );
    assert_eq!(server.authority().unwrap().world().store().len(), 2);
    let mut previous = None;
    for center in (2..18).chain((2..18).rev()).chain([2, 10, 2]) {
        let pos = SectionPos::new(center, 0, 0);
        if previous != Some(pos) {
            server.reload_section(&pages, pos, &mut hub).unwrap();
            hub.replace_policy(
                &mut server,
                id,
                PeerPolicy {
                    actor: actor(),
                    scope: scope(center, 1),
                },
            )
            .unwrap();
            for message in drain(&mut client).0 {
                match message {
                    ServerMessage::Interest(batch) => {
                        near.apply_interest(batch).unwrap();
                    }
                    ServerMessage::Seed(seed) => {
                        near = torse_net::ReplicaMirror::from_seed(seed).unwrap()
                    }
                    _ => panic!("unexpected message"),
                }
            }
            if let Some(old) = previous {
                server.unload_section(&pages, old, &mut hub).unwrap();
            }
        }
        previous = Some(pos);
        let desired = approved
            .select_window(
                pos,
                SectionPos::new(center + 7, 0, 0),
                limits().max_sections,
            )
            .unwrap();
        hub.set_visual_interest(id, desired.clone(), pos).unwrap();
        fill(&mut hub, &mut server, &mut client, &mut display);
        for p in desired.sections {
            assert_eq!(display.page(p), expected.get(&p));
        }
        assert_eq!(near.state().sections, expected[&pos].sections);
        assert!(display.pages().all(|p| p.entities.is_empty()));
        assert_eq!(server.authority().unwrap().world().store().len(), 3);
        assert!(display.accounted_bytes() <= 256 * 1024);
    }
    drop(hub);
    drop(server);
    // Saved modifications remain available after recovery, with no display-driven loads.
    let recovered = fixture::reopen(&dir, pages.clone());
    assert_eq!(recovered.authority().unwrap().world().store().len(), 3);
}

#[test]
fn pending_output_and_superseded_reads_converge_to_only_the_latest_window() {
    let dir = Directory::new();
    let (mut server, pages, expected) = create(&dir);
    let (mut hub, id, mut client, _, _) = connect(&mut server, scope(0, 1));
    let mut display = mirror(&server);
    hub.enable_visual_interest(
        &server,
        id,
        pages,
        approval(),
        scope(2, 8),
        SectionPos::ZERO,
        limits(),
    )
    .unwrap();
    hub.poll(&mut server).unwrap(); // Leave reset/output pending while I/O starts.
    assert!(hub.visual_terrain_diagnostics(id).unwrap().io_pending);
    for start in [8, 3, 12] {
        hub.set_visual_interest(id, scope(start, 8), SectionPos::new(start, 0, 0))
            .unwrap();
    }
    hub.set_visual_interest(id, scope(12, 2), SectionPos::new(12, 0, 0))
        .unwrap();
    fill(&mut hub, &mut server, &mut client, &mut display);
    assert_eq!(display.desired(), &scope(12, 2).sections);
    for pos in display.desired() {
        assert_eq!(display.page(*pos), expected.get(pos));
    }
    hub.set_visual_interest(id, scope(13, 1), SectionPos::new(13, 0, 0))
        .unwrap();
    fill(&mut hub, &mut server, &mut client, &mut display);
    assert!(display.page(SectionPos::new(12, 0, 0)).is_none());
    assert_eq!(display.pages().count(), 1);
    assert!(hub.contains(id));
}

#[test]
fn display_approval_cannot_grant_interaction_and_principal_change_purges_output() {
    use torse_sim::{
        action::ActionTarget,
        durable::{Call, ReplyStatus},
    };

    let dir = Directory::new();
    let (mut server, pages, _) = create(&dir);
    let (mut hub, id, mut client, _, _) = connect(&mut server, scope(0, 1));
    hub.enable_visual_interest(
        &server,
        id,
        pages,
        approval(),
        scope(2, 8),
        SectionPos::ZERO,
        limits(),
    )
    .unwrap();
    assert_eq!(
        hub.set_visual_interest(id, scope(33, 1), SectionPos::ZERO)
            .unwrap_err()
            .code,
        FaultCode::InvalidData
    );
    assert_eq!(
        hub.set_visual_interest(id, scope(2, 9), SectionPos::ZERO)
            .unwrap_err()
            .code,
        FaultCode::ResourceLimit
    );
    assert_eq!(hub.visual_terrain_diagnostics(id).unwrap().generation, 1);
    send(
        &mut client,
        ClientMessage::Subscribe {
            sections: vec![SectionPos::new(2, 0, 0)],
        },
    );
    hub.poll(&mut server).unwrap();
    assert!(drain(&mut client).0.iter().any(|message| matches!(
        message,
        ServerMessage::Rejected(RejectCode::PermissionDenied)
    )));
    send(
        &mut client,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: Call {
                action: torse_server::bootstrap::key("world:action/break"),
                version: 1,
                target: ActionTarget::Block(BlockPos::new(32, 0, 0)),
                parameters: Default::default(),
            },
        },
    );
    hub.poll(&mut server).unwrap();
    drain(&mut client);
    server.drive_tick(&mut hub, 1024).unwrap();
    hub.poll(&mut server).unwrap();
    assert!(drain(&mut client).0.iter().any(|message| matches!(
        message,
        ServerMessage::Result { seq: RequestSeq(1), reply }
            if reply.status == ReplyStatus::Rejected(RejectCode::PermissionDenied)
    )));
    hub.refresh_visual_interest(id).unwrap();
    hub.poll(&mut server).unwrap();
    assert_eq!(
        hub.replace_policy(
            &mut server,
            id,
            PeerPolicy {
                actor: torse_sim::action::ActorContext::new(torse_server::bootstrap::key(
                    "test:other"
                )),
                scope: scope(0, 1)
            }
        )
        .unwrap_err()
        .code,
        FaultCode::ResetRequired
    );
    assert!(!hub.contains(id));
    assert!(client.receive().is_err());
    assert_eq!(server.authority().unwrap().world().store().len(), 2);
}

#[test]
fn cache_budget_stabilizes_and_refresh_can_retry_without_installing_pages() {
    let dir = Directory::new();
    let (mut server, pages, _) = create(&dir);
    let (mut hub, id, mut client, _, _) = connect(&mut server, scope(0, 1));
    let mut budget = limits();
    budget.cache_bytes = 16 * 1024;
    let mut display = mirror(&server);
    hub.enable_visual_interest(
        &server,
        id,
        pages,
        approval(),
        scope(2, 8),
        SectionPos::new(2, 0, 0),
        budget,
    )
    .unwrap();
    let mut denied = std::collections::BTreeSet::new();
    let start = std::time::Instant::now();
    while denied.len() < 7 || display.pages().count() != 1 {
        hub.poll(&mut server).unwrap();
        for message in drain(&mut client).0 {
            if let ServerMessage::Terrain(update) = message {
                denied.extend(update.unavailable.iter().filter_map(|(pos, reason)| {
                    (*reason == TerrainUnavailable::Budget).then_some(*pos)
                }));
                display.apply(update).unwrap();
            }
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        std::thread::yield_now();
    }
    for _ in 0..16 {
        hub.poll(&mut server).unwrap();
        assert!(!hub.visual_terrain_diagnostics(id).unwrap().io_pending);
        assert!(drain(&mut client).0.is_empty());
    }
    assert_eq!(display.pages().count(), 1);
    assert!(display.page(SectionPos::new(2, 0, 0)).is_some());
    assert_eq!(denied, scope(3, 7).sections);
    hub.refresh_visual_interest(id).unwrap();
    assert_eq!(server.authority().unwrap().world().store().len(), 2);
}

#[test]
fn missing_corrupt_and_repaired_sources_are_explicit_and_retryable() {
    let dir = Directory::new();
    let (mut server, pages, expected) = create(&dir);
    let pos = SectionPos::new(24, 0, 0);
    let hash = server.authority().unwrap().parked_sections()[&pos].digest;
    let path = pages.path(hash);
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"broken page").unwrap();
    let (mut hub, id, mut client, _, _) = connect(&mut server, scope(0, 1));
    let mut display = mirror(&server);
    hub.enable_visual_interest(
        &server,
        id,
        pages,
        scope(24, 8),
        scope(24, 8),
        pos,
        limits(),
    )
    .unwrap();
    let start = std::time::Instant::now();
    let mut statuses = std::collections::BTreeMap::new();
    while statuses.get(&pos) != Some(&TerrainUnavailable::Corrupt)
        || statuses.get(&SectionPos::new(26, 0, 0)) != Some(&TerrainUnavailable::Missing)
        || display.page(SectionPos::new(25, 0, 0)).is_none()
    {
        hub.poll(&mut server).unwrap();
        for message in drain(&mut client).0 {
            if let ServerMessage::Terrain(update) = message {
                statuses.extend(update.unavailable.iter().copied());
                display.apply(update).unwrap();
            }
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        std::thread::yield_now();
    }
    assert!(display.page(pos).is_none());
    std::fs::write(path, original).unwrap();
    hub.set_visual_interest(id, scope(24, 2), pos).unwrap();
    hub.refresh_visual_interest(id).unwrap();
    fill(&mut hub, &mut server, &mut client, &mut display);
    assert_eq!(display.page(pos), expected.get(&pos));
}

#[test]
fn shared_pool_denial_is_retryable_and_closure_releases_display_reservations() {
    let dir = Directory::new();
    let (mut server, pages, _) = create(&dir);
    let pool = MemoryPool::new(MemoryBudget::new(1024 * 1024 * 1024));
    server.attach_memory(pool.clone(), 0).unwrap();
    let (mut hub, id, mut client, _, _) = connect(&mut server, scope(0, 1));
    let base = pool.usage().total;
    let held = pool
        .reserve(
            MemoryClass::Resource,
            1024 * 1024 * 1024 - base,
            EvictionRank::Pinned,
        )
        .unwrap();
    assert_eq!(
        hub.enable_visual_interest(
            &server,
            id,
            pages.clone(),
            approval(),
            scope(2, 8),
            SectionPos::ZERO,
            limits()
        )
        .unwrap_err()
        .code,
        FaultCode::ResourceLimit
    );
    assert!(hub.visual_terrain_diagnostics(id).is_none());
    drop(held);
    hub.enable_visual_interest(
        &server,
        id,
        pages,
        approval(),
        scope(2, 8),
        SectionPos::ZERO,
        limits(),
    )
    .unwrap();
    let reserved = hub.visual_terrain_diagnostics(id).unwrap().reserved_bytes;
    assert_eq!(pool.usage().total - base, reserved);
    let mut display = mirror(&server);
    fill(&mut hub, &mut server, &mut client, &mut display);
    hub.revoke(&mut server, id).unwrap();
    assert!(hub.visual_terrain_diagnostics(id).is_none());
    assert!(pool.usage().total <= base);
    drop(hub);
    drop(client);
    drop(server);
    assert_eq!(pool.usage().total, 0);
}

#[test]
fn a_near_edit_survives_leaving_saving_and_revisiting_the_display_window() {
    use torse_sim::{action::ActionTarget, durable::Call};
    use torse_world::{BlockStateId, TerrainQuery};
    let dir = Directory::new();
    let (mut server, pages, _) = create(&dir);
    let pos = SectionPos::new(2, 0, 0);
    let block = BlockPos::new(32, 0, 0);
    let (mut hub, id, mut client, mut near, _) = connect(&mut server, scope(0, 1));
    let mut display = mirror(&server);
    hub.enable_visual_interest(
        &server,
        id,
        pages.clone(),
        approval(),
        scope(2, 8),
        pos,
        limits(),
    )
    .unwrap();
    fill(&mut hub, &mut server, &mut client, &mut display);
    server.reload_section(&pages, pos, &mut hub).unwrap();
    hub.replace_policy(
        &mut server,
        id,
        PeerPolicy {
            actor: actor().target(ActionTarget::Block(block)),
            scope: scope(2, 1),
        },
    )
    .unwrap();
    for message in drain(&mut client).0 {
        if let ServerMessage::Interest(batch) = message {
            display.observe_authority(&batch.changes, batch.through);
            near.apply_interest(batch).unwrap();
        }
    }
    send(
        &mut client,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: Call {
                action: torse_server::bootstrap::key("world:action/break"),
                version: 1,
                target: ActionTarget::Block(block),
                parameters: Default::default(),
            },
        },
    );
    hub.poll(&mut server).unwrap();
    for message in drain(&mut client).0 {
        if let ServerMessage::Terrain(update) = message {
            display.apply(update).unwrap();
        }
    }
    server.drive_tick(&mut hub, 1024).unwrap();
    assert_eq!(
        server.authority().unwrap().world().store().query(block),
        TerrainQuery::Ready(BlockStateId::EMPTY)
    );
    hub.poll(&mut server).unwrap();
    for message in drain(&mut client).0 {
        match message {
            ServerMessage::Batch(batch) => {
                display.observe_authority(&batch.changes, batch.through);
                near.apply(batch).unwrap();
            }
            ServerMessage::Terrain(update) => {
                display.apply(update).unwrap();
            }
            _ => {}
        }
    }
    hub.replace_policy(
        &mut server,
        id,
        PeerPolicy {
            actor: actor(),
            scope: scope(0, 1),
        },
    )
    .unwrap();
    drain(&mut client);
    server.unload_section(&pages, pos, &mut hub).unwrap();
    hub.set_visual_interest(id, scope(10, 8), SectionPos::new(10, 0, 0))
        .unwrap();
    fill(&mut hub, &mut server, &mut client, &mut display);
    assert!(display.page(pos).is_none());
    hub.set_visual_interest(id, scope(2, 8), pos).unwrap();
    fill(&mut hub, &mut server, &mut client, &mut display);
    assert_eq!(display.page(pos).unwrap().sections, near.state().sections);
    assert_eq!(server.authority().unwrap().world().store().len(), 2);
}
