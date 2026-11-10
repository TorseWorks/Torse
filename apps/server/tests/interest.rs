//! Ordered interest cuts must match complete authorized projections.
#[path = "support/interest.rs"]
mod fixture;
mod support;
use fixture::*;
use support::Directory;
use torse_server::{bootstrap as base, network::*};
use torse_types::*;

fn copy_batch(
    batch: &torse_sim::replica::WorldReplicaBatch,
) -> torse_sim::replica::WorldReplicaBatch {
    torse_sim::replica::WorldReplicaBatch {
        binding: batch.binding,
        subscription: batch.subscription,
        seq: batch.seq,
        through: batch.through,
        changes: batch.changes.clone(),
    }
}
fn copy_mirror(mirror: &torse_net::ReplicaMirror) -> torse_net::ReplicaMirror {
    mirror.clone()
}

#[test]
fn overlap_256_sections_preserves_224_terrain_payloads_and_matches_full_snapshots() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let (mut hub, id, mut client, mut mirror, initial_bytes) = connect(&mut server, scope(0));
    let subscription = mirror.subscription;
    let cursor = mirror.through;
    for start in [32, 0, 32, 0] {
        hub.replace_policy(
            &mut server,
            id,
            PeerPolicy {
                actor: actor(),
                scope: scope(start),
            },
        )
        .unwrap();
        let (messages, bytes) = drain(&mut client);
        assert!(bytes < initial_bytes / 4);
        assert_eq!(messages.len(), 1);
        let ServerMessage::Interest(batch) = messages.into_iter().next().unwrap() else {
            panic!("expected interest delta")
        };
        assert_eq!(batch.subscription, subscription);
        assert_eq!(batch.through, cursor);
        assert_eq!(batch.changes.sections.len(), 32);
        assert_eq!(batch.changes.removed_sections.len(), 32);
        if start == 32 {
            assert_eq!(batch.changes.removed_entities, [EntityId(1), EntityId(2)]);
        } else {
            assert_eq!(batch.changes.entities.len(), 2);
        }
        assert!(
            batch
                .changes
                .sections
                .iter()
                .all(|section| !(32..256).contains(&section.pos.x))
        );
        let before = copy_mirror(&mirror);
        let mut ordinary = copy_mirror(&before);
        assert!(
            ordinary.apply(copy_batch(&batch)).is_err(),
            "ordinary edits must still advance world progress"
        );
        let mut missing = copy_mirror(&mirror);
        let mut gap = copy_batch(&batch);
        gap.seq.0 += 1;
        assert_eq!(
            missing.apply_interest(gap).unwrap_err().code,
            FaultCode::ResetRequired
        );
        assert_eq!(missing.state(), before.state());
        mirror.apply_interest(copy_batch(&batch)).unwrap();
        let after = copy_mirror(&mirror);
        assert!(mirror.apply_interest(batch).is_err());
        assert_eq!(mirror.state(), after.state());
        assert_eq!(
            mirror.state(),
            &reference(&mut server, actor(), scope(start))
        );
    }
}

#[test]
fn tightening_grants_with_pending_transport_output_closes_and_purges() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let (mut hub, id, mut client, _mirror, _) = connect(&mut server, scope(0));
    hub.replace_policy(
        &mut server,
        id,
        PeerPolicy {
            actor: actor(),
            scope: scope(32),
        },
    )
    .unwrap();
    // Leave the first interest frame unread in the shared output queue.
    assert_eq!(
        hub.replace_policy(
            &mut server,
            id,
            PeerPolicy {
                actor: torse_sim::action::ActorContext::new(base::key("test:observer")),
                scope: scope(0)
            }
        )
        .unwrap_err()
        .code,
        FaultCode::ResetRequired
    );
    assert!(!hub.contains(id));
    use torse_net::Transport;
    assert!(client.receive().is_err());
    let (_, _fresh_client, fresh, _) = attach(&mut hub, &mut server, scope(0));
    assert_eq!(fresh.state().sections.len(), 256);
}

#[test]
fn ordinary_scope_change_defers_pending_output_then_applies_once() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let (mut hub, id, mut client, mut mirror, _) = connect(&mut server, scope(0));
    hub.replace_policy(
        &mut server,
        id,
        PeerPolicy {
            actor: actor(),
            scope: scope(32),
        },
    )
    .unwrap();
    // Leave the preceding cut queued; the next movement must not revoke it.
    for _ in 0..3 {
        assert_eq!(
            hub.try_change_interest(&mut server, id, scope(0)).unwrap(),
            InterestUpdate::Deferred
        );
        assert!(hub.contains(id));
    }
    let messages = drain(&mut client).0;
    assert_eq!(messages.len(), 1);
    let ServerMessage::Interest(batch) = messages.into_iter().next().unwrap() else {
        panic!("preceding cut")
    };
    mirror.apply_interest(batch).unwrap();
    assert_eq!(
        hub.try_change_interest(&mut server, id, scope(0)).unwrap(),
        InterestUpdate::Applied
    );
    let messages = drain(&mut client).0;
    assert_eq!(messages.len(), 1);
    let ServerMessage::Interest(batch) = messages.into_iter().next().unwrap() else {
        panic!("retried cut")
    };
    mirror.apply_interest(batch).unwrap();
    let seq = mirror.seq;
    assert_eq!(
        hub.try_change_interest(&mut server, id, scope(0)).unwrap(),
        InterestUpdate::Applied
    );
    assert!(drain(&mut client).0.is_empty());
    assert_eq!(mirror.seq, seq);
    assert_eq!(mirror.state, reference(&mut server, actor(), scope(0)));
}

#[test]
fn ordinary_change_waits_for_source_edits_and_rejects_invalid_projections() {
    use torse_sim::{action::ActionTarget, durable::Call};
    use torse_world::ComponentValue;
    let dir = Directory::new();
    let mut server = create(&dir);
    let (mut hub, id, mut client, mut mirror, _) = connect(&mut server, scope(0));
    send(
        &mut client,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: Call {
                action: base::key("world:action/place"),
                version: 1,
                target: ActionTarget::Block(BlockPos::new(640, 1, 1)),
                parameters: [(
                    base::key("world:field/block"),
                    ComponentValue::Content(base::key("base:stone")),
                )]
                .into(),
            },
        },
    );
    hub.poll(&mut server).unwrap();
    drain(&mut client);
    server.drive_tick(&mut hub, 1024).unwrap();
    assert_eq!(
        hub.try_change_interest(&mut server, id, scope(32)).unwrap(),
        InterestUpdate::Deferred
    );
    hub.poll(&mut server).unwrap();
    for message in drain(&mut client).0 {
        if let ServerMessage::Batch(batch) = message {
            mirror.apply(batch).unwrap();
        }
    }
    assert_eq!(
        hub.try_change_interest(&mut server, id, scope(32)).unwrap(),
        InterestUpdate::Applied
    );
    let messages = drain(&mut client).0;
    assert_eq!(messages.len(), 1);
    let ServerMessage::Interest(batch) = messages.into_iter().next().unwrap() else {
        panic!("ordered interest")
    };
    mirror.apply_interest(batch).unwrap();
    assert_eq!(mirror.state, reference(&mut server, actor(), scope(32)));
    server
        .change_session_interest(
            SessionId(1),
            actor(),
            scope(0),
            OutputBudget {
                max_items: 1,
                max_bytes: 1,
            },
        )
        .unwrap_err();
    assert_eq!(
        hub.try_change_interest(&mut server, id, scope(0))
            .unwrap_err()
            .code,
        FaultCode::ResetRequired
    );
    assert!(hub.contains(id));
    assert!(drain(&mut client).0.is_empty());
}

#[test]
fn ordinary_edits_and_interest_cuts_share_sequence_and_recheck_retained_entity_grants() {
    use torse_sim::{action::ActionTarget, durable::Call};
    use torse_world::ComponentValue;
    let dir = Directory::new();
    let mut server = create(&dir);
    let (mut hub, id, mut client, mut mirror, _) = connect(&mut server, scope(0));
    assert!(
        mirror
            .state()
            .entities
            .iter()
            .any(|entity| !entity.components.is_empty())
    );
    send(
        &mut client,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: Call {
                action: base::key("world:action/place"),
                version: 1,
                target: ActionTarget::Block(BlockPos::new(640, 1, 1)),
                parameters: [(
                    base::key("world:field/block"),
                    ComponentValue::Content(base::key("base:stone")),
                )]
                .into(),
            },
        },
    );
    hub.poll(&mut server).unwrap();
    drain(&mut client);
    server.drive_tick(&mut hub, 1024).unwrap();
    hub.poll(&mut server).unwrap();
    let mut saw_edit = false;
    for message in drain(&mut client).0 {
        if let ServerMessage::Batch(batch) = message {
            assert!(
                batch
                    .changes
                    .sections
                    .iter()
                    .any(|section| section.pos == SectionPos::new(40, 0, 0))
            );
            mirror.apply(batch).unwrap();
            saw_edit = true;
        }
    }
    assert!(saw_edit);
    assert_eq!(mirror.state(), &reference(&mut server, actor(), scope(0)));
    hub.replace_policy(
        &mut server,
        id,
        PeerPolicy {
            actor: actor(),
            scope: scope(32),
        },
    )
    .unwrap();
    for message in drain(&mut client).0 {
        let ServerMessage::Interest(batch) = message else {
            panic!("expected interest")
        };
        assert!(
            !batch
                .changes
                .sections
                .iter()
                .any(|section| section.pos == SectionPos::new(40, 0, 0))
        );
        mirror.apply_interest(batch).unwrap();
    }
    assert_eq!(mirror.state(), &reference(&mut server, actor(), scope(32)));
    let denied = torse_sim::action::ActorContext::new(base::key("test:observer"));
    hub.replace_policy(
        &mut server,
        id,
        PeerPolicy {
            actor: denied.clone(),
            scope: scope(0),
        },
    )
    .unwrap();
    for message in drain(&mut client).0 {
        let ServerMessage::Interest(batch) = message else {
            panic!("expected interest")
        };
        mirror.apply_interest(batch).unwrap();
    }
    assert!(
        mirror
            .state()
            .entities
            .iter()
            .all(|entity| entity.components.is_empty())
    );
    assert_eq!(mirror.state(), &reference(&mut server, denied, scope(0)));
}

#[test]
fn explicit_reset_uses_a_new_seed_and_output_budget_failure_invalidates_projection() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let (mut hub, id, mut client, mirror, _) = connect(&mut server, scope(0));
    hub.reset_policy(
        &mut server,
        id,
        PeerPolicy {
            actor: actor(),
            scope: scope(32),
        },
    )
    .unwrap();
    let messages = drain(&mut client).0;
    let ServerMessage::Seed(seed) = messages.into_iter().next().unwrap() else {
        panic!("reset needs seed")
    };
    assert_ne!(seed.subscription, mirror.subscription);
    assert_eq!(seed.state, reference(&mut server, actor(), scope(32)));
    let error = server
        .change_session_interest(
            SessionId(1),
            actor(),
            scope(0),
            OutputBudget {
                max_items: 1,
                max_bytes: 1,
            },
        )
        .unwrap_err();
    assert_eq!(error.code, FaultCode::ResourceLimit);
    assert!(
        !server
            .authority()
            .unwrap()
            .can_change_interest(SessionId(1))
    );
    // No partial interest delta was placed in transport; the host must reset.
    assert!(drain(&mut client).0.is_empty());
}

#[test]
fn pending_source_edit_falls_back_to_a_complete_current_seed() {
    use torse_sim::{action::ActionTarget, durable::Call};
    use torse_world::ComponentValue;
    let dir = Directory::new();
    let mut server = create(&dir);
    let (mut hub, id, mut client, mirror, _) = connect(&mut server, scope(0));
    send(
        &mut client,
        ClientMessage::Request {
            seq: RequestSeq(1),
            call: Call {
                action: base::key("world:action/place"),
                version: 1,
                target: ActionTarget::Block(BlockPos::new(640, 1, 1)),
                parameters: [(
                    base::key("world:field/block"),
                    ComponentValue::Content(base::key("base:stone")),
                )]
                .into(),
            },
        },
    );
    hub.poll(&mut server).unwrap();
    drain(&mut client);
    server.drive_tick(&mut hub, 1024).unwrap();
    // Commit produced a source delta, but the network has not taken it yet.
    assert!(
        !server
            .authority()
            .unwrap()
            .can_change_interest(SessionId(1))
    );
    assert_eq!(
        hub.try_change_interest(&mut server, id, scope(32)).unwrap(),
        InterestUpdate::Deferred
    );
    assert!(drain(&mut client).0.is_empty());
    hub.replace_policy(
        &mut server,
        id,
        PeerPolicy {
            actor: actor(),
            scope: scope(32),
        },
    )
    .unwrap();
    let messages = drain(&mut client).0;
    assert_eq!(messages.len(), 1);
    let ServerMessage::Seed(seed) = messages.into_iter().next().unwrap() else {
        panic!("pending source requires seed")
    };
    assert_ne!(seed.subscription, mirror.subscription);
    assert!(seed.baseline.seq > mirror.through.seq);
    assert_eq!(seed.state, reference(&mut server, actor(), scope(32)));
}
