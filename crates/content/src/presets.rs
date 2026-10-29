//! Optional content-building templates; the compiler never applies them implicitly.
use crate::{AlphaMode, BlockDefinition, BoxBounds, FiniteField, Shape, StateProfile, StateSchema};
use torse_types::ContentKey;

/// Four vertical half volumes: west, east, north and south.
pub fn vertical_slab_schema(id: ContentKey) -> StateSchema {
    StateSchema {
        id,
        schema_version: 1,
        finite_fields: vec![FiniteField {
            id: ContentKey::parse("base:field/half").expect("static field"),
            default_value: 0,
            values: vec![0, 1, 2, 3],
        }],
        component_schemas: Vec::new(),
    }
}

/// Configure caller-owned material and identity as a completable vertical slab.
pub fn vertical_slab(block: &mut BlockDefinition, schema: ContentKey) {
    block.state_schema = schema;
    let tag = ContentKey::parse("world:tag/vertical_slab").expect("static tag");
    if !block.tags.contains(&tag) {
        block.tags.push(tag);
    }
    block.state_overrides = (0..4)
        .map(|side| {
            let mut bounds = BoxBounds {
                min: [0.0; 3],
                max: [1.0; 3],
            };
            let axis = if side < 2 { 0 } else { 2 };
            if side % 2 == 0 {
                bounds.max[axis] = 0.5;
            } else {
                bounds.min[axis] = 0.5;
            }
            StateProfile {
                values: vec![side],
                render_shape: Some(Shape::Box(bounds)),
                collision_shape: Some(Shape::Box(bounds)),
                selection_shape: Some(Shape::Box(bounds)),
                occlusion: Some(0),
            }
        })
        .collect();
    block.render_profile.shape = block.state_overrides[0]
        .render_shape
        .clone()
        .expect("first state");
    block.collision_profile.shape = Some(block.render_profile.shape.clone());
    block.selection_profile.shape = Some(block.render_profile.shape.clone());
    block.occlusion_profile.face_mask = 0;
}

/// Configure a non-solid plant with crossed cutout quads and a tight selection box.
///
/// Material, identity, finite fields, components and behavior remain caller-owned.
/// Callers choose the definition version when changing an existing content entry.
pub fn crossed_plant(block: &mut BlockDefinition) {
    let selection = Shape::Box(BoxBounds {
        min: [0.125, 0.0, 0.125],
        max: [0.875, 0.875, 0.875],
    });
    block.render_profile.shape = Shape::Cross;
    block.render_profile.alpha_mode = AlphaMode::Mask;
    block.collision_profile.shape = None;
    block.selection_profile.shape = Some(selection.clone());
    block.occlusion_profile.face_mask = 0;
    block.light_transport.absorption = 0.0;
    for state in &mut block.state_overrides {
        state.render_shape = Some(Shape::Cross);
        state.collision_shape = None;
        state.selection_shape = Some(selection.clone());
        state.occlusion = Some(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertical_template_keeps_the_material_and_exposes_four_distinct_half_volumes() {
        let pack = crate::m0_fixture_pack();
        let block = &pack
            .blocks
            .iter()
            .find(|b| b.value.id.path() == "vertical_glass_slab")
            .unwrap()
            .value;
        assert_eq!(
            block.render_profile.material.to_string(),
            "base:material/glass"
        );
        assert_eq!(
            block
                .interaction_profile
                .completes_to
                .as_ref()
                .unwrap()
                .to_string(),
            "base:glass"
        );
        for (side, state) in block.state_overrides.iter().enumerate() {
            let Some(Shape::Box(bounds)) = &state.render_shape else {
                panic!("half box");
            };
            assert_eq!(state.collision_shape, state.render_shape);
            assert_eq!(state.selection_shape, state.render_shape);
            let axis = if side < 2 { 0 } else { 2 };
            assert_eq!(bounds.max[axis] - bounds.min[axis], 0.5);
            assert_eq!(bounds.max[1] - bounds.min[1], 1.0);
        }
        assert!(crate::compile(pack).is_ok());
    }
    #[test]
    fn preset_preserves_identity_and_finite_state_while_replacing_geometry() {
        let mut pack = crate::m0_fixture_pack();
        let slab = pack
            .blocks
            .iter_mut()
            .find(|block| block.value.id.to_string() == "base:glass_slab")
            .unwrap();
        let before = slab.value.clone();
        crossed_plant(&mut slab.value);
        assert_eq!(slab.value.id, before.id);
        assert_eq!(slab.value.state_schema, before.state_schema);
        assert_eq!(
            slab.value.render_profile.material,
            before.render_profile.material
        );
        assert_eq!(
            slab.value
                .state_overrides
                .iter()
                .map(|state| &state.values)
                .collect::<Vec<_>>(),
            before
                .state_overrides
                .iter()
                .map(|state| &state.values)
                .collect::<Vec<_>>()
        );
        assert!(
            slab.value
                .state_overrides
                .iter()
                .all(|state| state.render_shape == Some(Shape::Cross)
                    && state.collision_shape.is_none()
                    && state.occlusion == Some(0))
        );
        assert!(crate::compile(pack).is_ok());
    }
    #[test]
    fn changed_fixture_geometry_has_a_new_explicit_content_identity() {
        let current = crate::compile(crate::m0_fixture_pack()).unwrap();
        let mut previous = crate::m0_fixture_pack();
        let plant = previous
            .blocks
            .iter_mut()
            .find(|block| block.value.id.to_string() == "base:plant")
            .unwrap();
        plant.value.definition_version = 1;
        plant.value.render_profile.shape = Shape::Panel;
        plant.value.selection_profile.shape = Some(Shape::Panel);
        let previous = crate::compile(previous).unwrap();
        assert_ne!(previous.authority_digest, current.authority_digest);
        assert_ne!(previous.presentation_digest, current.presentation_digest);
    }
}
