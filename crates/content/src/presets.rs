//! Optional content-building templates; the compiler never applies them implicitly.
use crate::{AlphaMode, BlockDefinition, BoxBounds, Shape};

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
