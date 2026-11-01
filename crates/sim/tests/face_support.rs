//! Face support is independent content metadata with ordinary terrain observations.

use torse_content::{
    CompiledContent, Face, FaceSupport, SupportCategory, compile, m0_fixture_pack,
};
use torse_sim::action::{ActionTarget, ActorContext, FaceSupportQuery};
use torse_sim::{OrderingKey, Phase, Transaction, WorldAuthority};
use torse_types::{
    BlockPos, Compatibility, ContentKey, RejectCode, SectionPos, SnapshotMeta, TransactionId,
    WorldId,
};
use torse_world::{
    BlockStateId, BlockStateRegistry, ComponentSchemas, ExteriorPolicy, Section, TerrainQuery,
    WorldBounds,
};

fn key(value: &str) -> ContentKey {
    ContentKey::parse(value).unwrap()
}

fn world(open_below: bool) -> WorldAuthority {
    let mut pack = m0_fixture_pack();
    let slab = pack
        .blocks
        .iter_mut()
        .find(|block| block.value.id == key("base:glass_slab"))
        .unwrap();
    slab.value.face_support = FaceSupport {
        attachment: 8,
        structural: 1,
    };
    slab.value.state_overrides[0].face_support = Some(FaceSupport::NONE);
    slab.value.state_overrides[1].face_support = Some(FaceSupport {
        attachment: 8,
        structural: 0,
    });
    let content = compile(pack).unwrap();
    let mut world = assemble(&content);
    if open_below {
        world
            .set_exterior_policy(ExteriorPolicy {
                below: true,
                above: false,
            })
            .unwrap();
    }
    world.store_mut().request(SectionPos::ZERO).unwrap();
    world
        .store_mut()
        .install(SectionPos::ZERO, Section::empty(16).unwrap())
        .unwrap();
    world.store_mut().finish_load(SectionPos::ZERO).unwrap();
    world
}

fn assemble(content: &CompiledContent) -> WorldAuthority {
    WorldAuthority::new(
        WorldId(1),
        BlockStateRegistry::build(content).unwrap(),
        ComponentSchemas::build(content),
        16,
        WorldBounds::new(0, 32).unwrap(),
        Compatibility {
            authority_schema: 1,
            rules_digest: content.authority_digest,
            content_digest: content.presentation_digest,
        },
    )
    .unwrap()
}

fn tx(id: u128) -> Transaction {
    Transaction::new(
        TransactionId(id),
        key("test:module"),
        OrderingKey {
            phase: Phase::Interaction,
            rule_priority: 0,
            stable_task_key: 1,
            local_sequence: 0,
        },
    )
}

fn states(world: &WorldAuthority) -> (BlockStateId, BlockStateId, BlockStateId) {
    let registry = world.registry();
    (
        registry.state_of(&key("base:glass_slab"), &[0]).unwrap(),
        registry.state_of(&key("base:glass_slab"), &[1]).unwrap(),
        registry.state_of(&key("base:glass_slab"), &[2]).unwrap(),
    )
}

#[test]
fn exact_state_replacements_inherit_or_explicitly_deny_all_support() {
    let world = world(false);
    let (denied, allowed, inherited) = states(&world);
    let registry = world.registry();
    assert_eq!(registry.face_support(denied), Some(&FaceSupport::NONE));
    assert!(
        registry
            .face_support(allowed)
            .unwrap()
            .supports(Face::PosY, SupportCategory::Attachment)
    );
    assert!(
        !registry
            .face_support(allowed)
            .unwrap()
            .supports(Face::NegX, SupportCategory::Structural)
    );
    assert!(
        registry
            .face_support(inherited)
            .unwrap()
            .supports(Face::NegX, SupportCategory::Structural)
    );
    assert_eq!(
        registry.face_support(BlockStateId::EMPTY),
        Some(&FaceSupport::NONE)
    );
    assert!(registry.face_support(BlockStateId(u16::MAX)).is_none());
    // The support query borrows the declared profile rather than cloning metadata.
    assert!(std::ptr::eq(
        registry.face_support(inherited).unwrap(),
        &registry.definition(inherited).unwrap().face_support
    ));
}

#[test]
fn identical_geometry_can_have_distinct_support_by_finite_state() {
    let mut pack = m0_fixture_pack();
    let slab = pack
        .blocks
        .iter_mut()
        .find(|block| block.value.id == key("base:glass_slab"))
        .unwrap();
    for profile in &mut slab.value.state_overrides {
        profile.render_shape = None;
        profile.collision_shape = None;
        profile.selection_shape = None;
        profile.occlusion = None;
    }
    slab.value.state_overrides[1].face_support = Some(FaceSupport {
        attachment: 8,
        structural: 0,
    });
    let content = compile(pack).unwrap();
    let registry = BlockStateRegistry::build(&content).unwrap();
    let a = registry.state_of(&key("base:glass_slab"), &[0]).unwrap();
    let b = registry.state_of(&key("base:glass_slab"), &[1]).unwrap();
    assert_eq!(registry.render_shape(a), registry.render_shape(b));
    assert_eq!(registry.collision_shape(a), registry.collision_shape(b));
    assert_ne!(registry.face_support(a), registry.face_support(b));
}

#[test]
fn world_query_requires_the_cell_grant_and_deduplicates_observations() {
    let mut world = world(false);
    let pos = BlockPos::new(2, 2, 2);
    let (_, allowed, _) = states(&world);
    world.store_mut().edit(pos, allowed).unwrap();
    let mut plan = tx(1);
    let denied = ActorContext::new(key("test:actor"));
    assert_eq!(
        denied.view(&world).face_support(pos, &mut plan),
        Err(RejectCode::PermissionDenied)
    );
    assert!(plan.preconditions.is_empty());
    let actor = denied.target(ActionTarget::Block(pos));
    for _ in 0..2 {
        assert!(
            actor
                .view(&world)
                .face_support(pos, &mut plan)
                .unwrap()
                .supports(Face::PosY, SupportCategory::Attachment)
        );
    }
    assert_eq!(plan.preconditions.len(), 1);
    assert_eq!(plan.preconditions[0].expected, allowed);
    assert!(plan.mutations.is_empty());
    assert_eq!(world.store().query(pos), TerrainQuery::Ready(allowed));
}

#[test]
fn missing_closed_and_known_empty_exterior_stay_distinct_and_never_support() {
    for open_below in [false, true] {
        let world = world(open_below);
        let missing = BlockPos::new(2, 18, 2);
        let exterior = BlockPos::new(2, -1, 2);
        let empty = BlockPos::new(2, 2, 2);
        let actor = ActorContext::new(key("test:actor"))
            .target(ActionTarget::Block(missing))
            .target(ActionTarget::Block(exterior))
            .target(ActionTarget::Block(empty));
        let view = actor.view(&world);
        let mut plan = tx(1);
        let unavailable = view.face_support(missing, &mut plan).unwrap();
        assert_eq!(unavailable, FaceSupportQuery::NotLoaded);
        let outside = view.face_support(exterior, &mut plan).unwrap();
        assert_eq!(
            outside,
            if open_below {
                FaceSupportQuery::Exterior
            } else {
                FaceSupportQuery::OutOfBounds
            }
        );
        let air = view.face_support(empty, &mut plan).unwrap();
        assert_eq!(air, FaceSupportQuery::Loaded(&FaceSupport::NONE));
        for query in [unavailable, outside, air] {
            for face in Face::ALL {
                assert!(!query.supports(face, SupportCategory::Attachment));
                assert!(!query.supports(face, SupportCategory::Structural));
            }
        }
        assert_eq!(plan.preconditions.len(), 1);
        assert_eq!(plan.preconditions[0].expected, BlockStateId::EMPTY);
    }
}

#[test]
fn a_support_change_invalidates_both_planning_and_prepared_publication() {
    for after_prepare in [false, true] {
        let mut world = world(false);
        let pos = BlockPos::new(2, 2, 2);
        let destination = BlockPos::new(3, 2, 2);
        let (denied, allowed, _) = states(&world);
        world.store_mut().edit(pos, allowed).unwrap();
        let actor = ActorContext::new(key("test:actor")).target(ActionTarget::Block(pos));
        let mut plan = tx(1);
        assert!(
            actor
                .view(&world)
                .face_support(pos, &mut plan)
                .unwrap()
                .supports(Face::PosY, SupportCategory::Attachment)
        );
        plan.write_block(destination, allowed);
        let prepared = after_prepare.then(|| world.prepare(&plan).unwrap());
        world.store_mut().edit(pos, denied).unwrap();
        if let Some(prepared) = prepared {
            assert!(world.publish(prepared).is_err());
        } else {
            assert!(world.prepare(&plan).is_err());
        }
        assert_eq!(
            world.store().query(destination),
            TerrainQuery::Ready(BlockStateId::EMPTY)
        );
    }
}

#[test]
fn rereading_changed_support_cannot_replace_an_existing_expectation() {
    let mut world = world(false);
    let pos = BlockPos::new(2, 2, 2);
    let (denied, allowed, _) = states(&world);
    world.store_mut().edit(pos, allowed).unwrap();
    let actor = ActorContext::new(key("test:actor")).target(ActionTarget::Block(pos));
    let mut plan = tx(1);
    actor.view(&world).face_support(pos, &mut plan).unwrap();
    world.store_mut().edit(pos, denied).unwrap();
    assert_eq!(
        actor.view(&world).face_support(pos, &mut plan),
        Err(RejectCode::Conflict)
    );
    assert_eq!(plan.preconditions.len(), 1);
    assert_eq!(plan.preconditions[0].expected, allowed);
}

#[test]
fn grouped_and_frozen_support_reads_survive_until_publication() {
    use torse_sim::TransactionGroup;
    use torse_sim::durable::StoredTransactionGroup;
    for frozen in [false, true] {
        for after_prepare in [false, true] {
            let mut world = world(false);
            let pos = BlockPos::new(2, 2, 2);
            let destination = BlockPos::new(3, 2, 2);
            let second = BlockPos::new(4, 2, 2);
            let (denied, allowed, _) = states(&world);
            world.store_mut().edit(pos, allowed).unwrap();
            let actor = ActorContext::new(key("test:actor")).target(ActionTarget::Block(pos));
            let mut plan = tx(1);
            actor.view(&world).face_support(pos, &mut plan).unwrap();
            plan.write_block(destination, allowed);
            let mut sibling = tx(2);
            sibling.source = key("test:sibling");
            sibling.write_block(second, allowed);
            let group = TransactionGroup::new(TransactionId(3), plan.ordering, vec![plan, sibling])
                .unwrap();
            let group = if frozen {
                StoredTransactionGroup::capture(&world, &group)
                    .unwrap()
                    .restore(&world)
                    .unwrap()
            } else {
                group
            };
            let prepared = after_prepare.then(|| world.prepare_group(&group).unwrap());
            world.store_mut().edit(pos, denied).unwrap();
            if let Some(prepared) = prepared {
                assert!(world.publish(prepared).is_err());
            } else {
                assert!(world.prepare_group(&group).is_err());
            }
            for cell in [destination, second] {
                assert_eq!(
                    world.store().query(cell),
                    TerrainQuery::Ready(BlockStateId::EMPTY)
                );
            }
        }
    }
}

#[test]
fn support_changes_reject_old_authority_recovery_without_changing_presentation() {
    let mut pack = m0_fixture_pack();
    let original = assemble(&compile(pack.clone()).unwrap());
    let snapshot = original
        .capture_snapshot(SnapshotMeta {
            cursor: torse_types::CommitCursor {
                world: original.world(),
                seq: original.seq(),
            },
            compatibility: original.coordinator().compatibility(),
            resume_at: original.coordinator().resume_point(),
        })
        .unwrap();
    original
        .begin_restore()
        .restore_snapshot(&snapshot)
        .unwrap();
    pack.blocks[0].value.face_support.attachment = 8;
    let changed = assemble(&compile(pack).unwrap());
    let before = original.coordinator().compatibility();
    let after = changed.coordinator().compatibility();
    assert_ne!(before.rules_digest, after.rules_digest);
    assert_eq!(before.content_digest, after.content_digest);
    assert!(changed.begin_restore().restore_snapshot(&snapshot).is_err());
}
