//! Content-driven placement intent shared by input adapters.
//!
//! This resolver chooses a target and half; the action service still checks
//! permissions, occupancy, world bounds and the final collision volume.

use torse_content::Shape;
use torse_types::{BlockPos, ContentKey};
use torse_world::{BlockStateId, BlockStateRegistry};

/// A selection hit with its height relative to the hit cell's bottom.
#[derive(Clone, Copy, Debug)]
pub struct PlacementHit {
    pub block: BlockPos,
    pub face: [i32; 3],
    pub local_height: f32,
}

/// Resolved arguments for the existing native place action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlacementIntent {
    pub target: BlockPos,
    /// Lower half is zero; upper half is one.
    pub half: u32,
}

/// Attach to the hit face, or complete an identical slab at its inner face.
///
/// Side hits at the exact midline select the lower half. A slab's outer
/// horizontal face and its occupied side place into the adjacent cell.
/// Occupied adjacent cells remain subject to the action's completion rule.
pub fn resolve(
    registry: &BlockStateRegistry,
    held: &ContentKey,
    current: BlockStateId,
    hit: PlacementHit,
) -> Option<PlacementIntent> {
    resolve_at(registry, held, current, hit, [0.5, hit.local_height, 0.5])
}

/// Resolve a placement using the full cell-local hit position.
pub fn resolve_at(
    registry: &BlockStateRegistry,
    held: &ContentKey,
    current: BlockStateId,
    hit: PlacementHit,
    local: [f32; 3],
) -> Option<PlacementIntent> {
    if !matches!(
        hit.face,
        [-1, 0, 0] | [1, 0, 0] | [0, -1, 0] | [0, 1, 0] | [0, 0, -1] | [0, 0, 1]
    ) || !hit.local_height.is_finite()
        || !(0.0..=1.0).contains(&hit.local_height)
    {
        return None;
    }
    if local
        .iter()
        .any(|v| !v.is_finite() || !(0.0..=1.0).contains(v))
    {
        return None;
    }
    let vertical = registry
        .default_state(held)
        .and_then(|s| registry.definition(s))
        .is_some_and(|d| {
            d.tags
                .iter()
                .any(|t| t.to_string() == "world:tag/vertical_slab")
        });
    if vertical {
        let half = match hit.face {
            [-1, 0, 0] => 1,
            [1, 0, 0] => 0,
            [0, 0, -1] => 3,
            [0, 0, 1] => 2,
            _ if (local[0] - 0.5).abs() >= (local[2] - 0.5).abs() => u32::from(local[0] > 0.5),
            _ => 2 + u32::from(local[2] > 0.5),
        };
        if registry.definition(current).is_some_and(|d| &d.id == held)
            && let Some(Shape::Box(bounds)) = registry.selection_shape_ref(current)
        {
            let axis = if bounds.max[0] - bounds.min[0] == 0.5 {
                0
            } else {
                2
            };
            let side = u32::from(bounds.min[axis] == 0.5) + if axis == 0 { 0 } else { 2 };
            let inner = if side % 2 == 0 { 1 } else { -1 };
            if hit.face[axis] == inner {
                return Some(PlacementIntent {
                    target: hit.block,
                    half: side ^ 1,
                });
            }
        }
        return Some(PlacementIntent {
            target: BlockPos::new(
                hit.block.x.checked_add(i64::from(hit.face[0]))?,
                hit.block.y.checked_add(i64::from(hit.face[1]))?,
                hit.block.z.checked_add(i64::from(hit.face[2]))?,
            ),
            half,
        });
    }
    let half = match hit.face {
        [0, -1, 0] => 1,
        [0, 1, 0] => 0,
        _ => u32::from(hit.local_height > 0.5),
    };
    let same_completable = registry.definition(current).is_some_and(|definition| {
        &definition.id == held
            && definition
                .interaction_profile
                .completes_to
                .as_ref()
                .is_some_and(|target| registry.default_state(target).is_some())
    });
    if same_completable {
        let slab_half = match registry.selection_shape_ref(current) {
            Some(Shape::LowerSlab) => Some(0),
            Some(Shape::Box(bounds)) if bounds.min == [0.0; 3] && bounds.max == [1.0, 0.5, 1.0] => {
                Some(0)
            }
            Some(Shape::Box(bounds)) if bounds.min == [0.0, 0.5, 0.0] && bounds.max == [1.0; 3] => {
                Some(1)
            }
            _ => None,
        };
        let side = hit.face[1] == 0;
        let completes = match slab_half {
            Some(0) => hit.face == [0, 1, 0] || (side && hit.local_height > 0.5),
            Some(1) => hit.face == [0, -1, 0] || (side && hit.local_height <= 0.5),
            _ => false,
        };
        if completes {
            return Some(PlacementIntent {
                target: hit.block,
                half: 1 - slab_half?,
            });
        }
    }
    Some(PlacementIntent {
        target: BlockPos::new(
            hit.block.x.checked_add(i64::from(hit.face[0]))?,
            hit.block.y.checked_add(i64::from(hit.face[1]))?,
            hit.block.z.checked_add(i64::from(hit.face[2]))?,
        ),
        half,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(text: &str) -> ContentKey {
        ContentKey::parse(text).unwrap()
    }

    fn registry() -> BlockStateRegistry {
        let content = torse_content::compile(torse_content::m0_fixture_pack()).unwrap();
        BlockStateRegistry::build(&content).unwrap()
    }

    #[test]
    fn vertical_glass_uses_all_four_sides_and_only_completes_the_inner_face() {
        let registry = registry();
        let held = key("base:vertical_glass_slab");
        for (side, face) in [
            (0, [1, 0, 0]),
            (1, [-1, 0, 0]),
            (2, [0, 0, 1]),
            (3, [0, 0, -1]),
        ] {
            let state = registry.state_of(&held, &[side]).unwrap();
            let hit = PlacementHit {
                block: BlockPos::ZERO,
                face,
                local_height: 0.7,
            };
            let inner = resolve_at(&registry, &held, state, hit, [0.5, 0.7, 0.5]).unwrap();
            assert_eq!(
                inner,
                PlacementIntent {
                    target: BlockPos::ZERO,
                    half: u32::from(side ^ 1)
                }
            );
            let outer_face = face.map(|v| -v);
            let outer = resolve_at(
                &registry,
                &held,
                state,
                PlacementHit {
                    face: outer_face,
                    ..hit
                },
                [0.5, 0.7, 0.5],
            )
            .unwrap();
            assert_ne!(outer.target, BlockPos::ZERO);
        }
        for (local, half) in [
            ([0.1, 1.0, 0.5], 0),
            ([0.9, 1.0, 0.5], 1),
            ([0.5, 1.0, 0.1], 2),
            ([0.5, 1.0, 0.9], 3),
        ] {
            let hit = PlacementHit {
                block: BlockPos::ZERO,
                face: [0, 1, 0],
                local_height: 1.0,
            };
            assert_eq!(
                resolve_at(&registry, &held, BlockStateId::EMPTY, hit, local)
                    .unwrap()
                    .half,
                half
            );
        }
    }

    #[test]
    fn faces_and_side_midline_choose_the_expected_half() {
        let registry = registry();
        let held = key("base:glass_slab");
        let current = registry.default_state(&key("base:stone")).unwrap();
        for (face, height, half) in [
            ([0, 1, 0], 1.0, 0),
            ([0, -1, 0], 0.0, 1),
            ([-1, 0, 0], 0.25, 0),
            ([1, 0, 0], 0.5, 0),
            ([0, 0, -1], 0.5001, 1),
            ([0, 0, 1], 0.75, 1),
        ] {
            let hit = PlacementHit {
                block: BlockPos::new(-3, -2, -1),
                face,
                local_height: height,
            };
            let intent = resolve(&registry, &held, current, hit).unwrap();
            assert_eq!(intent.half, half);
            assert_eq!(
                intent.target,
                BlockPos::new(
                    -3 + i64::from(face[0]),
                    -2 + i64::from(face[1]),
                    -1 + i64::from(face[2])
                )
            );
        }
    }

    #[test]
    fn only_matching_slabs_complete_from_the_unoccupied_half() {
        let registry = registry();
        let slab = key("base:glass_slab");
        let pos = BlockPos::new(2, 3, 4);
        for (state_half, face, height, merges) in [
            (0, [0, 1, 0], 0.5, true),
            (0, [0, -1, 0], 0.0, false),
            (0, [1, 0, 0], 0.25, false),
            (0, [1, 0, 0], 0.5, false),
            (1, [0, -1, 0], 0.5, true),
            (1, [0, 1, 0], 1.0, false),
            (1, [1, 0, 0], 0.75, false),
            (1, [1, 0, 0], 0.5, true),
        ] {
            let current = registry.state_of(&slab, &[state_half]).unwrap();
            let hit = PlacementHit {
                block: pos,
                face,
                local_height: height,
            };
            for held in [&slab, &key("base:stone"), &key("base:plant")] {
                let intent = resolve(&registry, held, current, hit).unwrap();
                assert_eq!(
                    intent.target == pos,
                    merges && held == &slab,
                    "state={state_half}, face={face:?}, height={height}, held={held}"
                );
                if intent.target == pos {
                    assert_eq!(intent.half, 1 - u32::from(state_half));
                }
            }
        }
    }

    #[test]
    fn invalid_hits_and_coordinate_overflow_are_rejected() {
        let registry = registry();
        let held = key("base:glass_slab");
        for (face, height) in [
            ([0, 0, 0], 0.5),
            ([1, 1, 0], 0.5),
            ([1, 0, 0], f32::NAN),
            ([1, 0, 0], f32::INFINITY),
            ([1, 0, 0], -0.1),
            ([1, 0, 0], 1.1),
        ] {
            assert!(
                resolve(
                    &registry,
                    &held,
                    BlockStateId::EMPTY,
                    PlacementHit {
                        block: BlockPos::new(0, 0, 0),
                        face,
                        local_height: height
                    }
                )
                .is_none()
            );
        }
        assert!(
            resolve(
                &registry,
                &held,
                BlockStateId::EMPTY,
                PlacementHit {
                    block: BlockPos::new(i64::MAX, 0, 0),
                    face: [1, 0, 0],
                    local_height: 0.5
                }
            )
            .is_none()
        );
    }

    #[test]
    fn independently_named_slab_presets_keep_their_completion_identity() {
        let mut pack = torse_content::m0_fixture_pack();
        let mut other = pack
            .blocks
            .iter()
            .find(|block| block.value.id == key("base:glass_slab"))
            .unwrap()
            .clone();
        other.value.id = key("test:stone_slab");
        other.value.interaction_profile.completes_to = Some(key("base:stone"));
        pack.blocks.push(other);
        let registry = BlockStateRegistry::build(&torse_content::compile(pack).unwrap()).unwrap();
        let hit = PlacementHit {
            block: BlockPos::new(0, 0, 0),
            face: [0, 1, 0],
            local_height: 0.5,
        };
        let current = registry.default_state(&key("test:stone_slab")).unwrap();
        assert_eq!(
            resolve(&registry, &key("test:stone_slab"), current, hit)
                .unwrap()
                .target,
            hit.block
        );
        assert_eq!(
            resolve(&registry, &key("base:glass_slab"), current, hit)
                .unwrap()
                .target,
            BlockPos::new(0, 1, 0)
        );
    }
}
