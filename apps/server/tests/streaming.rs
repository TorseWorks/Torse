//! Persistent section residency, entity identity, replica control, and page integrity.
mod support;
use support::Directory;
use torse_codec::{StorageCodec, wire, wire::Wire};
use torse_server::{bootstrap as base, execution::*, section_pages::SectionPages, *};
use torse_sim::{WorldAuthority, action::ActorContext, durable::*};
use torse_types::*;
use torse_world::*;

#[test]
fn legacy_dense_uniform_pages_replay_after_runtime_compaction() {
    let (world, _) = base::world().unwrap();
    let mut page = world
        .capture_section(SectionPos::ZERO, base::budget())
        .unwrap();
    page.entities.clear();
    page.states.clear();
    page.sections[0].cells =
        SnapshotCells::Dense(vec![BlockStateId::EMPTY; (page.side as usize).pow(3)]);
    let mut encoded = Vec::new();
    wire::encode(&page, &mut encoded, base::budget(), base::codec_limits()).unwrap();
    let old: Snapshot = wire::decode(&encoded[..], base::budget(), base::codec_limits()).unwrap();
    let restored = old.restore(world.registry(), world.schemas()).unwrap();
    assert!(matches!(
        restored.store.section(SectionPos::ZERO).unwrap().storage(),
        SectionStorage::Uniform(_)
    ));
    let mut recovery = torse_sim::Recovery::new(world.world(), world.registry(), world.schemas());
    recovery.restore_snapshot(&old).unwrap();
    let mut changes = ChangeSet {
        meta: CommitMeta {
            previous: old.meta.cursor.seq,
            cursor: CommitCursor {
                world: world.world(),
                seq: old.meta.cursor.seq.next().unwrap(),
            },
            compatibility: old.meta.compatibility,
            resume_at: old.meta.resume_at,
        },
        edits: Vec::new(),
        entity_moves: Vec::new(),
        component_edits: Vec::new(),
        entity_lifecycle: None,
    };
    recovery
        .replay_section(&changes, &old, false, base::budget())
        .unwrap();
    changes.meta.previous = changes.meta.cursor.seq;
    changes.meta.cursor.seq = changes.meta.cursor.seq.next().unwrap();
    recovery
        .replay_section(&changes, &old, true, base::budget())
        .unwrap();
    let restored = recovery.finish().unwrap();
    assert_eq!(
        restored.store().query(BlockPos::new(1, 1, 1)),
        TerrainQuery::Ready(BlockStateId::EMPTY)
    );
}

#[test]
fn section_tokens_preserve_tickets_handles_and_revision_dependencies() {
    let (mut world, _) = base::world().unwrap();
    let other = world
        .entities_mut()
        .spawn(Position::new(17.0, 1.0, 1.0))
        .unwrap();
    let page = world
        .capture_section(SectionPos::ZERO, base::budget())
        .unwrap();
    let old = world.entities().handle_of(page.entities[0].id).unwrap();
    let revision = world.store().revision(SectionPos::ZERO).unwrap();
    let before = world.seq();
    let prepared = world.prepare_section(&page, false, base::budget()).unwrap();
    let ticket = world
        .store_mut()
        .acquire_ticket(SectionPos::ZERO, TicketKind::Transaction)
        .unwrap();
    assert!(world.publish_section(prepared).is_err());
    assert_eq!(world.seq(), before);
    assert!(world.prepare_section(&page, false, base::budget()).is_err());
    world.store_mut().release_ticket(ticket).unwrap();
    let prepared = world.prepare_section(&page, false, base::budget()).unwrap();
    drop(prepared);
    assert_eq!(world.seq(), before);
    let prepared = world.prepare_section(&page, false, base::budget()).unwrap();
    world.publish_section(prepared).unwrap();
    assert!(world.entities().get(other).is_some());
    assert!(world.entities().get(old).is_none());
    let prepared = world.prepare_section(&page, true, base::budget()).unwrap();
    world.publish_section(prepared).unwrap();
    assert!(world.entities().get(other).is_some());
    assert!(world.entities().get(old).is_none());
    assert!(world.store().revision(SectionPos::ZERO).unwrap() > revision);
}

#[test]
fn unload_invalidates_only_intersecting_network_peers_and_reconnect_gets_new_seed() {
    use torse_net::{MemoryTransport, Transport, TransportLimits};
    use torse_server::network::{ClientMessage, NetworkHub, PeerPolicy};
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(&dir);
    let mut hub = NetworkHub::new(server.binding(), base::network_limits()).unwrap();
    let mut clients = Vec::new();
    for (name, pos) in [
        ("test:near", SectionPos::ZERO),
        ("test:far", SectionPos::new(1, 0, 0)),
    ] {
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
                    actor: ActorContext::new(base::key(name)),
                    scope: base::scope(),
                },
                Tick(0),
            )
            .unwrap();
        for message in [
            ClientMessage::Hello {
                version: torse_protocol::PROTOCOL_VERSION,
                compatibility: server.binding().compatibility,
            },
            ClientMessage::Subscribe {
                sections: vec![pos],
            },
        ] {
            let mut bytes = Vec::new();
            wire::encode(&message, &mut bytes, base::budget(), base::codec_limits()).unwrap();
            client.send(bytes).unwrap();
        }
        hub.poll(&mut server).unwrap();
        // Leave the near seed queued: invalidation must purge transport output too.
        clients.push((id, client));
    }
    durable(
        server
            .unload_section(&pages, SectionPos::ZERO, &mut hub)
            .unwrap(),
    );
    assert!(clients[0].1.receive().is_err());
    assert!(clients[1].1.receive().unwrap().is_some());
    hub.poll(&mut server).unwrap();
    assert!(!hub.contains(clients[0].0));
    assert!(hub.contains(clients[1].0));
    durable(
        server
            .reload_section(&pages, SectionPos::ZERO, &mut hub)
            .unwrap(),
    );
    assert!(hub.contains(clients[1].0));
    server
        .register_session(
            Viewer {
                session: SessionId(99),
                actor: ActorContextId(99),
            },
            ActorContext::new(base::key("test:new")),
            base::scope(),
        )
        .unwrap();
    let seed = server
        .open_replica(SessionId(99), base::scope(), base::budget())
        .unwrap();
    assert_eq!(seed.state.sections.len(), 2);
}

#[test]
fn events_and_frozen_work_prevent_unsafe_unload() {
    use torse_sim::{Phase, SystemSpec};
    struct Rule;
    impl DurableRule for Rule {
        fn spec(&self) -> SystemSpec {
            SystemSpec {
                id: base::key("test:pin"),
                phase: Phase::Simulation,
                rule_priority: 0,
                before: Vec::new(),
                after: Vec::new(),
            }
        }
        fn plan(&self, _: &WorldAuthority, tick: Tick) -> Result<Vec<WorkPlan>, Fault> {
            Ok(if tick == Tick(0) {
                vec![WorkPlan {
                    transaction: None,
                    schedule: vec![EventDraft {
                        tick: Tick(10),
                        rule: base::key("test:pin"),
                        payload: Default::default(),
                    }],
                    cancel: Vec::new(),
                }]
            } else {
                Vec::new()
            })
        }
    }
    let dir = Directory::new();
    let pages = pages(&dir);
    let (world, actions) = base::world().unwrap();
    let compatibility = world.coordinator().compatibility();
    let mut server = base::Files::create_file(
        dir.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |b| {
            ExecutionHost::new(
                b,
                world,
                actions,
                vec![Box::new(Rule)],
                base::execution_limits(),
            )
        },
    )
    .unwrap();
    server.drive_one(&mut NoReplicas).unwrap();
    assert!(
        server
            .unload_section(&pages, SectionPos::ZERO, &mut NoReplicas)
            .is_err()
    );
    server.drive_tick(&mut NoReplicas, 1024).unwrap();
    let cursor = server.authority().unwrap().cursor();
    let events = server.authority().unwrap().runtime().events.clone();
    assert!(
        server
            .unload_section(&pages, SectionPos::ZERO, &mut NoReplicas)
            .is_err()
    );
    assert_eq!(server.authority().unwrap().cursor(), cursor);
    assert_eq!(server.authority().unwrap().runtime().events, events);
}

#[test]
fn section_crash_child() {
    let Ok(root) = std::env::var("TORSE_SECTION_CRASH_ROOT") else {
        return;
    };
    let dir = Directory(std::path::PathBuf::from(root));
    let pages = pages(&dir);
    let mut server = create(&dir);
    let page = server
        .authority()
        .unwrap()
        .world()
        .capture_section(SectionPos::ZERO, base::budget())
        .unwrap();
    pages.put(&page).unwrap();
    if std::env::var("TORSE_SECTION_CRASH_PHASE").unwrap() == "page" {
        std::process::exit(78);
    }
    durable(
        server
            .unload_section(&pages, SectionPos::ZERO, &mut NoReplicas)
            .unwrap(),
    );
    std::process::exit(79);
}

#[test]
fn real_process_exit_before_and_after_unload_recovers_only_durable_residency() {
    for phase in ["page", "commit"] {
        let dir = Directory::new();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "section_crash_child", "--nocapture"])
            .env("TORSE_SECTION_CRASH_ROOT", &dir.0)
            .env("TORSE_SECTION_CRASH_PHASE", phase)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(if phase == "page" { 78 } else { 79 }));
        let pages = pages(&dir);
        let mut server = open(&dir, pages.clone()).unwrap();
        assert_eq!(
            server
                .authority()
                .unwrap()
                .world()
                .store()
                .phase(SectionPos::ZERO)
                .is_some(),
            phase == "page"
        );
        if phase == "commit" {
            durable(
                server
                    .reload_section(&pages, SectionPos::ZERO, &mut NoReplicas)
                    .unwrap(),
            );
        }
        assert_eq!(server.authority().unwrap().world().entities().len(), 2);
        drop(server);
        let server = open(&dir, pages).unwrap();
        assert!(
            server
                .authority()
                .unwrap()
                .world()
                .store()
                .phase(SectionPos::ZERO)
                .is_some()
        );
    }
}

fn pages(dir: &Directory) -> SectionPages {
    let root = dir.0.join("sections");
    std::fs::create_dir_all(&root).unwrap();
    SectionPages::new(root, base::budget(), base::codec_limits(), 64).unwrap()
}

#[test]
fn page_collection_traces_parked_checkpoints_and_replay_before_removing_orphans() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(&dir);
    let orphan = generated(
        server.authority().unwrap().world(),
        SectionPos::new(9, 0, 0),
    );
    let orphan_digest = pages.put(&orphan).unwrap();
    std::fs::write(dir.0.join("sections/page-123-0.part"), b"torn publication").unwrap();
    durable(
        server
            .unload_section(&pages, SectionPos::ZERO, &mut NoReplicas)
            .unwrap(),
    );
    let parked = server.authority().unwrap().parked_sections()[&SectionPos::ZERO].digest;
    let report = server.collect_section_pages(&pages, 2).unwrap();
    assert_eq!(report.removed_pages, 1);
    assert_eq!(report.removed_temporaries, 1);
    assert!(!pages.path(orphan_digest).exists());
    assert!(pages.get(parked).is_ok());
    durable(
        server
            .reload_section(&pages, SectionPos::ZERO, &mut NoReplicas)
            .unwrap(),
    );
    // The older checkpoint and retained reload record still own the page.
    assert_eq!(
        server
            .collect_section_pages(&pages, 2)
            .unwrap()
            .removed_pages,
        0
    );
    drop(server);
    let mut server = open(&dir, pages.clone()).unwrap();
    assert!(server.authority().unwrap().parked_sections().is_empty());
    // Once every retained checkpoint has resident data and the reload is covered, collect it.
    assert_eq!(
        server
            .collect_section_pages(&pages, 1)
            .unwrap()
            .removed_pages,
        1
    );
    assert!(!pages.path(parked).exists());
    drop(server);
    assert!(open(&dir, pages).is_ok());
}

#[test]
fn page_collection_validates_every_root_and_filename_before_any_deletion() {
    use std::collections::BTreeSet;
    let dir = Directory::new();
    let pages = pages(&dir);
    let (world, _) = base::world().unwrap();
    let page = generated(&world, SectionPos::new(9, 0, 0));
    let orphan = pages.put(&page).unwrap();
    let mut references = BTreeSet::new();
    references.insert(Digest([1; 32]));
    assert!(pages.collect(&references).is_err());
    assert!(pages.path(orphan).exists());
    std::fs::write(dir.0.join("sections/unrelated.txt"), b"keep").unwrap();
    assert!(pages.collect(&BTreeSet::new()).is_err());
    assert!(pages.path(orphan).exists());
    std::fs::remove_file(dir.0.join("sections/unrelated.txt")).unwrap();
    assert_eq!(pages.collect(&BTreeSet::new()).unwrap().removed_pages, 1);
    assert_eq!(pages.collect(&BTreeSet::new()).unwrap().removed_pages, 0);
}
fn create(dir: &Directory) -> base::Files {
    let (mut world, actions) = base::world().unwrap();
    let schemas = world.schemas().clone();
    let handle = world.entities().iter().next().unwrap().0;
    world
        .entities_mut()
        .components_mut(handle)
        .unwrap()
        .insert(
            &schemas,
            &collider_schema(),
            EntityCollider::Box {
                half: [300_000, 900_000, 300_000],
            }
            .fields(),
        )
        .unwrap();
    let compatibility = world.coordinator().compatibility();
    base::Files::create_file(
        dir.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            ExecutionHost::new(
                binding,
                world,
                actions,
                Vec::new(),
                base::execution_limits(),
            )
        },
    )
    .unwrap()
}
fn open(dir: &Directory, pages: SectionPages) -> Result<base::Files, Fault> {
    let (world, actions) = base::world().unwrap();
    base::Files::open_file(
        dir.acquire(),
        world.coordinator().compatibility(),
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                world.registry(),
                world.schemas(),
                actions,
                Vec::new(),
                base::execution_limits(),
            )
            .with_section_pages(pages))
        },
    )
}
fn generated(world: &WorldAuthority, pos: SectionPos) -> Snapshot {
    let mut page = world
        .capture_section(SectionPos::ZERO, base::budget())
        .unwrap();
    page.sections[0].pos = pos;
    page.sections[0].cells = SnapshotCells::Uniform(BlockStateId::EMPTY);
    page.entities.clear();
    page.states.clear();
    page
}
fn durable(step: CommitStep) {
    assert!(matches!(step, CommitStep::Durable(_)));
}

#[test]
fn unload_replay_checkpoint_reload_preserve_exact_entities_watermark_and_changed_cells() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(&dir);
    let before = server
        .authority()
        .unwrap()
        .world()
        .capture_section(SectionPos::ZERO, base::budget())
        .unwrap();
    assert!(!before.entities.is_empty());
    let handle = server
        .authority()
        .unwrap()
        .world()
        .entities()
        .handle_of(before.entities[0].id)
        .unwrap();
    assert_eq!(
        EntityCollider::of(
            server
                .authority()
                .unwrap()
                .world()
                .entities()
                .get(handle)
                .unwrap()
        )
        .unwrap(),
        Some(EntityCollider::Box {
            half: [300_000, 900_000, 300_000]
        })
    );
    durable(
        server
            .unload_section(&pages, SectionPos::ZERO, &mut NoReplicas)
            .unwrap(),
    );
    let host = server.authority().unwrap();
    assert_eq!(
        host.world().store().query(BlockPos::new(1, 1, 1)),
        TerrainQuery::NotLoaded
    );
    assert!(host.world().entities().get(handle).is_none());
    assert_eq!(
        host.world().entities().next_entity_id(),
        before.next_entity_id
    );
    drop(server);
    // Only the initial checkpoint exists; this must replay the unload journal.
    let mut server = open(&dir, pages.clone()).unwrap();
    assert!(
        server
            .authority()
            .unwrap()
            .parked_sections()
            .contains_key(&SectionPos::ZERO)
    );
    server.checkpoint().unwrap();
    drop(server);
    let mut server = open(&dir, pages.clone()).unwrap();
    durable(
        server
            .reload_section(&pages, SectionPos::ZERO, &mut NoReplicas)
            .unwrap(),
    );
    let after = server
        .authority()
        .unwrap()
        .world()
        .capture_section(SectionPos::ZERO, base::budget())
        .unwrap();
    assert_eq!(before.entities, after.entities);
    assert_eq!(before.sections, after.sections);
    assert_eq!(before.states, after.states);
    assert_eq!(before.next_entity_id, after.next_entity_id);
    server.drive_tick(&mut NoReplicas, 1024).unwrap();
    drop(server);
    let server = open(&dir, pages).unwrap();
    assert!(server.authority().unwrap().parked_sections().is_empty());
    assert_eq!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id(),
        before.next_entity_id
    );
}

#[test]
fn fresh_terrain_is_durable_and_rejects_existing_sections_or_imported_entity_ids() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(&dir);
    let pos = SectionPos::new(2, 0, 0);
    let page = generated(server.authority().unwrap().world(), pos);
    durable(
        server
            .install_section(&pages, page.clone(), &mut NoReplicas)
            .unwrap(),
    );
    let cursor = server.authority().unwrap().cursor();
    assert!(
        server
            .install_section(&pages, page, &mut NoReplicas)
            .is_err()
    );
    let mut bad = generated(
        server.authority().unwrap().world(),
        SectionPos::new(3, 0, 0),
    );
    bad.entities = server
        .authority()
        .unwrap()
        .world()
        .capture_section(SectionPos::ZERO, base::budget())
        .unwrap()
        .entities;
    assert!(
        server
            .install_section(&pages, bad, &mut NoReplicas)
            .is_err()
    );
    assert_eq!(server.authority().unwrap().cursor(), cursor);
    drop(server);
    let server = open(&dir, pages).unwrap();
    assert_eq!(
        server
            .authority()
            .unwrap()
            .world()
            .store()
            .query(BlockPos::new(32, 0, 0)),
        TerrainQuery::Ready(BlockStateId::EMPTY)
    );
}

#[test]
fn generated_entities_allocate_atomically_and_survive_journal_and_page_recovery() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(&dir);
    let pos = SectionPos::new(2, 0, 0);
    let mut page = generated(server.authority().unwrap().world(), pos);
    let next = page.next_entity_id;
    page.entities.push(SnapshotEntity {
        id: EntityId(next),
        position_bits: Position::new(33.5, 1.5, 1.5).to_bits(),
        components: Vec::new(),
    });
    page.next_entity_id = next + 1;
    let mut invalid = page.clone();
    invalid.entities[0].id = EntityId(next - 1);
    assert!(
        server
            .install_section(&pages, invalid, &mut NoReplicas)
            .is_err()
    );
    assert_eq!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id(),
        next
    );
    let mut gap = page.clone();
    gap.entities[0].id = EntityId(next + 1);
    gap.next_entity_id = next + 2;
    assert!(
        server
            .install_section(&pages, gap, &mut NoReplicas)
            .is_err()
    );
    durable(
        server
            .install_section(&pages, page, &mut NoReplicas)
            .unwrap(),
    );
    drop(server);
    let mut server = open(&dir, pages.clone()).unwrap();
    assert!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .handle_of(EntityId(next))
            .is_some()
    );
    assert_eq!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id(),
        next + 1
    );
    durable(server.unload_section(&pages, pos, &mut NoReplicas).unwrap());
    server.checkpoint().unwrap();
    drop(server);
    let mut server = open(&dir, pages.clone()).unwrap();
    durable(server.reload_section(&pages, pos, &mut NoReplicas).unwrap());
    assert!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .handle_of(EntityId(next))
            .is_some()
    );
    assert_eq!(
        server
            .authority()
            .unwrap()
            .world()
            .entities()
            .next_entity_id(),
        next + 1
    );
}

#[test]
fn corrupt_or_missing_parked_pages_refuse_recovery_without_rewriting_world_data() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let mut server = create(&dir);
    durable(
        server
            .unload_section(&pages, SectionPos::ZERO, &mut NoReplicas)
            .unwrap(),
    );
    let reference = server.authority().unwrap().parked_sections()[&SectionPos::ZERO].clone();
    server.checkpoint().unwrap();
    drop(server);
    let journal = std::fs::read(dir.0.join("journal.log")).unwrap();
    let file = pages.path(reference.digest);
    let original = std::fs::read(&file).unwrap();
    for replacement in [Vec::new(), vec![0; 32], {
        let mut v = original.clone();
        v[10] ^= 1;
        v
    }] {
        std::fs::write(&file, replacement).unwrap();
        assert!(open(&dir, pages.clone()).is_err());
        assert_eq!(std::fs::read(dir.0.join("journal.log")).unwrap(), journal);
    }
    std::fs::remove_file(&file).unwrap();
    assert!(open(&dir, pages.clone()).is_err());
    std::fs::write(&file, original).unwrap();
    assert!(open(&dir, pages).is_ok());
}

#[test]
fn page_capacity_and_input_budget_fail_before_any_world_publication() {
    let dir = Directory::new();
    let store = pages(&dir);
    let mut server = create(&dir);
    let page = server
        .authority()
        .unwrap()
        .world()
        .capture_section(SectionPos::ZERO, base::budget())
        .unwrap();
    let digest = store.put(&page).unwrap();
    assert_eq!(store.put(&page).unwrap(), digest);
    let tiny = SectionPages::new(
        dir.0.join("sections"),
        OutputBudget::new(1, 64),
        base::codec_limits(),
        1,
    )
    .unwrap();
    let cursor = server.authority().unwrap().cursor();
    assert!(
        server
            .unload_section(&tiny, SectionPos::ZERO, &mut NoReplicas)
            .is_err()
    );
    assert_eq!(server.authority().unwrap().cursor(), cursor);
    let full = SectionPages::new(
        dir.0.join("sections"),
        base::budget(),
        base::codec_limits(),
        1,
    )
    .unwrap();
    assert!(
        server
            .unload_section(&full, SectionPos::new(1, 0, 0), &mut NoReplicas)
            .is_err()
    );
    assert_eq!(server.authority().unwrap().cursor(), cursor);
    assert_eq!(store.get(digest).unwrap(), page);
}

#[test]
fn journal_reservation_failure_preserves_residency_and_allows_retry() {
    let dir = Directory::new();
    let pages = pages(&dir);
    let (world, actions) = base::world().unwrap();
    let compatibility = world.coordinator().compatibility();
    let mut policy = base::file_policy();
    policy.journal.max_pending_bytes = 256;
    let mut server = base::Files::create_file(
        dir.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        policy,
        |b| ExecutionHost::new(b, world, actions, Vec::new(), base::execution_limits()),
    )
    .unwrap();
    let cursor = server.authority().unwrap().cursor();
    assert!(matches!(
        server
            .unload_section(&pages, SectionPos::ZERO, &mut NoReplicas)
            .unwrap(),
        CommitStep::Blocked(_)
    ));
    assert_eq!(server.authority().unwrap().cursor(), cursor);
    assert!(server.authority().unwrap().parked_sections().is_empty());
    assert!(
        server
            .authority()
            .unwrap()
            .world()
            .store()
            .phase(SectionPos::ZERO)
            .is_some()
    );
    drop(server);
    let mut server = open(&dir, pages.clone()).unwrap();
    durable(
        server
            .unload_section(&pages, SectionPos::ZERO, &mut NoReplicas)
            .unwrap(),
    );
}

#[test]
fn legacy_execution_snapshot_and_commit_layouts_remain_readable() {
    let dir = Directory::new();
    let server = create(&dir);
    let codec = base::Codec::new(base::codec_limits()).unwrap();

    let binding = server.binding();
    let (world, actions) = base::world().unwrap();
    let mut host = ExecutionHost::new(
        binding,
        world,
        actions,
        Vec::new(),
        base::execution_limits(),
    )
    .unwrap();
    let snapshot = host.capture_snapshot(base::budget()).unwrap();
    let mut bytes = Vec::new();
    let mut e = torse_codec::Encoder::new(&mut bytes, base::budget(), base::codec_limits());
    1u16.write(&mut e).unwrap();
    snapshot.meta.write(&mut e).unwrap();
    snapshot.state.world.write(&mut e).unwrap();
    snapshot.state.runtime.write(&mut e).unwrap();
    e.finish();
    assert_eq!(
        codec.decode_snapshot(&bytes, base::budget()).unwrap().state,
        snapshot.state
    );
    let p = host.prepare_next().unwrap().unwrap();
    let r = p.record();
    for version in [1u16, 2] {
        bytes.clear();
        let mut e = torse_codec::Encoder::new(&mut bytes, base::budget(), base::codec_limits());
        version.write(&mut e).unwrap();
        r.meta.write(&mut e).unwrap();
        r.changes.world.write(&mut e).unwrap();
        if version == 1 {
            host.runtime().clone().write(&mut e).unwrap();
        } else {
            r.changes.runtime.write(&mut e).unwrap();
        }
        r.changes.outcome.write(&mut e).unwrap();
        r.changes.cancelled.write(&mut e).unwrap();
        e.finish();
        assert!(
            codec
                .decode_commit(&bytes, base::budget())
                .unwrap()
                .changes
                .section
                .is_none()
        );
    }
}
