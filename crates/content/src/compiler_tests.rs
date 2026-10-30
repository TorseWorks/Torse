use crate::{
    AlphaMode, BoxBounds, Face, FaceOverlay, Shape, SourceRef, Sourced, compile, m0_fixture_pack,
};

#[test]
fn builtin_fixture_compiles() {
    // The fixture is the minimum proof that all four contracts can coexist and
    // that authority and presentation summaries are independent values.
    let compiled = compile(m0_fixture_pack()).unwrap();
    assert_eq!(compiled.blocks.len(), 4);
    assert!(compiled.blocks.contains_key(&key("base:stone")));
    assert!(compiled.blocks.contains_key(&key("base:glass")));
    // The upper slab is a state of base:glass_slab, not its own block.
    assert!(compiled.blocks.contains_key(&key("base:glass_slab")));
    assert_ne!(compiled.authority_digest, compiled.presentation_digest);
}

fn visual_quad() -> crate::RenderQuad {
    crate::RenderQuad {
        positions: [
            [0.0, 0.0, 1.0],
            [1.0, 0.0, 1.0],
            [1.0, 1.0, 1.0],
            [0.0, 1.0, 1.0],
        ],
        uvs: [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
        normal: [0.0, 0.0, 1.0],
        material: key("sample:tile"),
        alpha_mode: AlphaMode::Opaque,
        overlay: None,
        cull_face: Some(Face::PosZ),
        tint_channel: None,
        shade: true,
        shade_direction: None,
        double_sided: false,
    }
}

#[test]
fn weighted_parts_are_bounded_independent_and_have_complete_identity() {
    use crate::{ModelChoice, ModelPart, RenderModel};
    let mut left = visual_quad();
    left.material = key("sample:left");
    let mut right = left.clone();
    right.material = key("sample:right");
    let part = ModelPart {
        choices: vec![
            ModelChoice {
                weight: 2,
                quads: vec![left].into(),
            },
            ModelChoice {
                weight: 3,
                quads: vec![right].into(),
            },
        ],
    };
    let mut model = RenderModel {
        quads: Vec::new(),
        parts: vec![part.clone(), part],
    };
    model.validate().unwrap();
    assert_eq!(model.occlusion_mask(), 1 << Face::PosZ.index());
    assert!((0..100).any(|seed| {
        let choices: Vec<_> = model.selected_quads(seed).map(|q| &q.material).collect();
        choices[0] != choices[1]
    }));
    let mut pack = m0_fixture_pack();
    pack.blocks[0].value.render_profile.shape = Shape::Model(model.clone().into());
    pack.blocks[0].value.occlusion_profile.face_mask = 0;
    let before = compile(pack.clone()).unwrap();
    model.parts[0].choices[0].weight = 7;
    pack.blocks[0].value.render_profile.shape = Shape::Model(model.clone().into());
    let after = compile(pack).unwrap();
    assert_eq!(before.authority_digest, after.authority_digest);
    assert_ne!(before.presentation_digest, after.presentation_digest);
    std::sync::Arc::make_mut(&mut model.parts[0].choices[0].quads)[0].alpha_mode = AlphaMode::Mask;
    model.parts[1].choices.clear();
    assert!(model.validate().is_err());
    assert_eq!(model.occlusion_mask(), 0);
    model.parts = vec![ModelPart {
        choices: vec![ModelChoice {
            weight: 0,
            quads: vec![visual_quad()].into(),
        }],
    }];
    assert!(model.validate().is_err());
}

#[test]
fn model_uv_changes_only_presentation_and_physics_rejects_visual_models() {
    let mut pack = m0_fixture_pack();
    let block = &mut pack.blocks[0].value;
    block.render_profile.shape = Shape::Model(
        crate::RenderModel {
            quads: vec![visual_quad()],
            parts: Vec::new(),
        }
        .into(),
    );
    block.occlusion_profile.face_mask = 0;
    let before = compile(pack.clone()).unwrap();
    let Shape::Model(model) = &mut pack.blocks[0].value.render_profile.shape else {
        unreachable!()
    };
    std::sync::Arc::make_mut(model).quads[0].uvs[0][0] = 0.25;
    let after = compile(pack.clone()).unwrap();
    assert_eq!(before.authority_digest, after.authority_digest);
    assert_ne!(before.presentation_digest, after.presentation_digest);
    pack.blocks[0].value.collision_profile.shape =
        Some(pack.blocks[0].value.render_profile.shape.clone());
    assert!(
        compile(pack)
            .unwrap_err()
            .iter()
            .any(|error| error.message.contains("visual models"))
    );
}

#[test]
fn model_limits_finite_coordinates_winding_and_independent_compounds_are_validated() {
    let mut model = crate::RenderModel {
        quads: vec![visual_quad()],
        parts: Vec::new(),
    };
    assert!(model.validate().is_ok());
    model.quads[0].positions[0][0] = f32::NAN;
    assert!(model.validate().is_err());
    model.quads[0] = visual_quad();
    model.quads[0].positions.swap(0, 1);
    assert!(model.validate().is_err());
    model.quads = vec![visual_quad(); crate::MAX_MODEL_QUADS + 1];
    assert!(model.validate().is_err());
    let bounds = BoxBounds {
        min: [0.0; 3],
        max: [1.0, 0.5, 1.0],
    };
    assert!(Shape::Compound(vec![bounds; 2]).validate().is_ok());
    assert!(
        Shape::Compound(vec![bounds; crate::MAX_COMPOUND_BOXES + 1])
            .validate()
            .is_err()
    );
}

#[test]
fn state_profile_geometry_is_validated_hashed_and_order_independent() {
    let mut pack = m0_fixture_pack();
    let index = pack
        .blocks
        .iter()
        .position(|block| !block.value.state_overrides.is_empty())
        .unwrap();
    let before = compile(pack.clone()).unwrap();
    pack.blocks[index].value.state_overrides.reverse();
    assert_eq!(
        before.authority_digest,
        compile(pack.clone()).unwrap().authority_digest
    );
    let profile = &mut pack.blocks[index].value.state_overrides[0];
    profile.collision_shape = Some(Shape::Compound(Vec::new()));
    let after = compile(pack.clone()).unwrap();
    assert_ne!(before.authority_digest, after.authority_digest);
    assert_eq!(before.presentation_digest, after.presentation_digest);
    pack.blocks[index].value.state_overrides[0].values = vec![65535];
    assert!(
        compile(pack)
            .unwrap_err()
            .iter()
            .any(|error| error.field == "state_overrides")
    );
}

#[test]
fn block_instance_schema_is_reserved_and_defaults_must_be_unique() {
    let mut pack = m0_fixture_pack();
    let component = pack
        .state_schemas
        .iter_mut()
        .flat_map(|s| &mut s.value.component_schemas)
        .next()
        .unwrap();
    component.id = key("core:schema/block_instance");
    assert!(
        compile(pack)
            .unwrap_err()
            .iter()
            .any(|d| d.message.contains("reserved"))
    );
    let mut pack = m0_fixture_pack();
    let component = pack
        .state_schemas
        .iter()
        .flat_map(|s| &s.value.component_schemas)
        .next()
        .unwrap()
        .id
        .clone();
    pack.blocks[0].value.component_defaults = vec![component.clone(), component];
    assert!(
        compile(pack)
            .unwrap_err()
            .iter()
            .any(|d| d.field == "component_defaults")
    );
}

#[test]
fn pack_order_does_not_change_digest() {
    // Reversing top-level vectors and reordering legal finite values must not
    // affect compiled identity. This protects content hashes from filesystem and
    // loader enumeration order.
    let mut first = m0_fixture_pack();
    let mut second = m0_fixture_pack();
    first.state_schemas[1].value.finite_fields[0]
        .values
        .reverse();
    first.blocks.reverse();
    first.state_schemas.reverse();
    first.actions.reverse();
    second.blocks.rotate_left(1);
    second.state_schemas.rotate_left(1);
    second.actions.rotate_left(1);

    let first = compile(first).unwrap();
    let second = compile(second).unwrap();
    assert_eq!(first.authority_digest, second.authority_digest);
    assert_eq!(first.presentation_digest, second.presentation_digest);
}

#[test]
fn invalid_shape_is_reported_with_content_location() {
    // Geometry failures must identify the owning content and field so an author
    // can fix the source without searching the entire pack.
    let mut pack = m0_fixture_pack();
    let block = pack
        .blocks
        .iter_mut()
        .find(|block| block.value.id.to_string() == "base:stone")
        .unwrap();
    block.value.render_profile.shape = Shape::Box(BoxBounds {
        min: [0.0, 0.0, 0.0],
        max: [1.5, 1.0, 1.0],
    });

    let errors = compile(pack).unwrap_err();
    assert!(errors.iter().any(|error| {
        error
            .content_id
            .as_ref()
            .is_some_and(|id| id.to_string() == "base:stone")
            && error.field == "render_profile.shape"
            && error.source.source == "fixtures/m0/stone.json"
    }));
}

#[test]
fn duplicate_definition_is_rejected() {
    // Duplicate stable IDs are never resolved by load order; the second source
    // location must be reported explicitly.
    let mut pack = m0_fixture_pack();
    let duplicate = pack.blocks[0].clone();
    pack.blocks.push(Sourced::new(
        SourceRef::new("fixtures/m0/duplicate.json", 7),
        duplicate.value,
    ));
    let errors = compile(pack).unwrap_err();
    assert!(errors.iter().any(|error| {
        error.field == "id"
            && error.message.contains("duplicate")
            && error.source.source == "fixtures/m0/duplicate.json"
    }));
}

#[test]
fn missing_state_schema_is_rejected() {
    // A block cannot compile against an undeclared state model.
    let mut pack = m0_fixture_pack();
    pack.blocks[0].value.state_schema = key("missing:state");
    let errors = compile(pack).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.field == "state_schema" && error.message.contains("missing:state"))
    );
}

#[test]
fn finite_state_explosion_is_rejected() {
    // Two individually small fields can produce a large Cartesian product, so
    // the compiler must enforce the combined state limit.
    let mut pack = m0_fixture_pack();
    let schema = pack
        .state_schemas
        .iter_mut()
        .find(|schema| schema.value.id.to_string() == "base:state/plant_growth")
        .unwrap();
    schema.value.finite_fields[0].values = (0..100).collect();
    schema.value.finite_fields.push(crate::FiniteField {
        id: key("base:field/age"),
        default_value: 0,
        values: (0..100).collect(),
    });
    let errors = compile(pack).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.field == "finite_fields" && error.message.contains("exceed limit"))
    );
}

#[test]
fn module_action_references_are_resolved() {
    // Module registration cannot claim an action that is absent from the pack.
    let mut pack = m0_fixture_pack();
    pack.modules[0]
        .value
        .registered_actions
        .push(key("base:action/missing"));
    let errors = compile(pack).unwrap_err();
    assert!(errors.iter().any(|error| {
        error.field == "registered_actions" && error.message.contains("base:action/missing")
    }));
}

#[test]
fn partial_occlusion_claims_are_rejected() {
    // Mesh culling removes whole neighbour faces, so a slab claiming a side
    // boundary would punch a see-through hole above its midline.
    let mut pack = m0_fixture_pack();
    let slab = pack
        .blocks
        .iter_mut()
        .find(|block| block.value.id.to_string() == "base:glass_slab")
        .unwrap();
    slab.value.occlusion_profile.face_mask = 0b110100;
    let errors = compile(pack).unwrap_err();
    assert!(errors.iter().any(|error| {
        error.field == "occlusion_profile.face_mask"
            && error.message.contains("does not fully cover")
    }));
}

#[test]
fn transparent_materials_may_not_claim_occlusion() {
    // Neighbour faces stay visible through cutout and blended blocks, so
    // claiming any occlusion would delete a face the player can still see.
    let mut pack = m0_fixture_pack();
    let glass = pack
        .blocks
        .iter_mut()
        .find(|block| block.value.id.to_string() == "base:glass")
        .unwrap();
    glass.value.occlusion_profile.face_mask = 0b11_1111;
    let errors = compile(pack).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|error| error.field == "occlusion_profile.face_mask"
                && error.message.contains("may not claim occlusion")),
        "got {errors:?}"
    );
}

#[test]
fn completion_target_must_be_a_registered_block() {
    let mut pack = m0_fixture_pack();
    let slab = pack
        .blocks
        .iter_mut()
        .find(|block| block.value.id.to_string() == "base:glass_slab")
        .unwrap();
    slab.value.interaction_profile.completes_to = Some(key("base:missing"));
    let errors = compile(pack).unwrap_err();
    assert!(errors.iter().any(|error| {
        error.field == "interaction_profile.completes_to" && error.message.contains("base:missing")
    }));
}

#[test]
fn capability_binding_and_host_permission_are_independent() {
    // Binding a capability to an object proves only that the interface exists;
    // the module's requested host permission remains a separate declaration.
    let mut pack = m0_fixture_pack();
    pack.blocks[0]
        .value
        .capability_bindings
        .push(crate::CapabilityBinding {
            capability: key("base:capability/item_port"),
            face_mask: 0b11_1111,
        });
    let compiled = compile(pack).unwrap();
    let stone = &compiled.blocks[&key("base:stone")];
    assert_eq!(stone.capability_bindings.len(), 1);
    assert_eq!(
        compiled.modules[&key("base:core")]
            .requested_permissions
            .len(),
        1
    );

    let mut invalid = m0_fixture_pack();
    invalid.blocks[0]
        .value
        .capability_bindings
        .push(crate::CapabilityBinding {
            capability: key("missing:capability"),
            face_mask: 0,
        });
    let errors = compile(invalid).unwrap_err();
    assert!(errors.iter().any(|error| {
        error.field == "capability_bindings" && error.message.contains("missing:capability")
    }));
}

#[test]
fn face_materials_affect_presentation_only() {
    // A per-face surface change is a presentation decision: clients may see a
    // different image while the authoritative content identity stays stable.
    let base = compile(m0_fixture_pack()).unwrap();

    let mut pack = m0_fixture_pack();
    let stone = pack
        .blocks
        .iter_mut()
        .find(|block| block.value.id.to_string() == "base:stone")
        .unwrap();
    stone
        .value
        .render_profile
        .face_materials
        .set(Face::PosY, key("base:material/stone_top"));

    let changed = compile(pack).unwrap();
    assert_eq!(base.authority_digest, changed.authority_digest);
    assert_ne!(base.presentation_digest, changed.presentation_digest);

    let stone = &changed.blocks[&key("base:stone")];
    assert_eq!(
        stone
            .render_profile
            .face_materials
            .get(Face::PosY)
            .map(|material| material.path()),
        Some("material/stone_top")
    );
    assert_eq!(
        stone
            .render_profile
            .face_materials
            .resolve(Face::NegY, &stone.render_profile.material)
            .path(),
        "material/stone"
    );
}

#[test]
fn face_overlays_affect_presentation_only() {
    let base = compile(m0_fixture_pack()).unwrap();
    let mut pack = m0_fixture_pack();
    let stone = pack
        .blocks
        .iter_mut()
        .find(|block| block.value.id.to_string() == "base:stone")
        .unwrap();
    stone.value.render_profile.face_overlays.set(
        Face::NegX,
        FaceOverlay {
            material: key("base:material/stone_overlay"),
            alpha_mode: AlphaMode::Mask,
        },
    );

    let changed = compile(pack).unwrap();
    assert_eq!(base.authority_digest, changed.authority_digest);
    assert_ne!(base.presentation_digest, changed.presentation_digest);
}

#[test]
fn same_face_material_change_keeps_authority_digest() {
    // Setting an override equal to the block material must still be a
    // presentation change only, never an authority change.
    let base = compile(m0_fixture_pack()).unwrap();

    let mut pack = m0_fixture_pack();
    let stone = pack
        .blocks
        .iter_mut()
        .find(|block| block.value.id.to_string() == "base:stone")
        .unwrap();
    let material = stone.value.render_profile.material.clone();
    stone
        .value
        .render_profile
        .face_materials
        .set(Face::NegX, material);

    let changed = compile(pack).unwrap();
    assert_eq!(base.authority_digest, changed.authority_digest);
    assert_ne!(base.presentation_digest, changed.presentation_digest);
}

fn key(value: &str) -> torse_types::ContentKey {
    torse_types::ContentKey::parse(value).unwrap()
}
