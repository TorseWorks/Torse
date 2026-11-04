use crate::*;

fn pack() -> ContentPack {
    let mut pack = m0_fixture_pack();
    let block = &mut pack
        .blocks
        .iter_mut()
        .find(|b| !b.value.state_overrides.is_empty())
        .unwrap()
        .value;
    block.light_transport = LightTransport {
        absorption: 0.5,
        emission: 0.75,
    };
    block.state_overrides[0].light_transport = Some(LightTransport {
        absorption: 0.0,
        emission: 0.0,
    });
    pack
}

#[test]
fn state_optics_inherit_replace_and_survive_cooked_bytes() {
    let compiled = compile(pack()).unwrap();
    let block = compiled
        .blocks
        .values()
        .find(|b| {
            b.state_overrides
                .iter()
                .any(|p| p.light_transport.is_some())
        })
        .unwrap();
    let key = block.id.clone();
    let tables = cook(&compiled, &[], &[]).unwrap();
    let rows: Vec<_> = tables.states.iter().filter(|s| s.block == key).collect();
    assert_eq!(
        rows[0].light_transport,
        LightTransport {
            absorption: 0.0,
            emission: 0.0
        }
    );
    assert_eq!(rows[1].light_transport, block.light_transport);
    let bytes = write_cooked(&tables).unwrap();
    assert_eq!(read_cooked(&bytes).unwrap(), tables);
    let mut old = bytes.clone();
    old[..8].copy_from_slice(b"TCOOK003");
    assert_eq!(read_cooked(&old), Err(CookError::Header));
    let mut invalid = tables.clone();
    invalid.states[0].light_transport.emission = f32::NAN;
    assert!(write_cooked(&invalid).is_err());
}

#[test]
fn state_optics_validate_values_and_only_change_authority() {
    let before = compile(m0_fixture_pack()).unwrap();
    let hex = |digest: torse_types::Digest| {
        digest
            .0
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    // Frozen compiler identities from the pre-extension fixture.
    assert_eq!(
        hex(before.authority_digest),
        "f82544c962f4729a46ff2640451bd82b7a762d394602f7900b5e38f95822a45a"
    );
    assert_eq!(
        hex(before.presentation_digest),
        "ce135979a1111f0d7832c8d01da62cce82067a6c521f04bc7c043cb1481d3d13"
    );

    let mut same = m0_fixture_pack();
    for b in &mut same.blocks {
        for p in &mut b.value.state_overrides {
            p.light_transport = None;
        }
    }
    assert_eq!(
        compile(same).unwrap().authority_digest,
        before.authority_digest
    );
    let original = pack();
    let compiled = compile(original.clone()).unwrap();
    let mut changed = original.clone();
    let profile = changed
        .blocks
        .iter_mut()
        .find(|b| !b.value.state_overrides.is_empty())
        .unwrap()
        .value
        .state_overrides
        .first_mut()
        .unwrap();
    profile.light_transport.as_mut().unwrap().emission = 0.5;
    let next = compile(changed).unwrap();
    assert_ne!(compiled.authority_digest, next.authority_digest);
    assert_eq!(compiled.presentation_digest, next.presentation_digest);
    for value in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
        for absorption in [true, false] {
            let mut bad = original.clone();
            let b = bad
                .blocks
                .iter_mut()
                .find(|b| !b.value.state_overrides.is_empty())
                .unwrap();
            let light = b.value.state_overrides[0].light_transport.as_mut().unwrap();
            if absorption {
                light.absorption = value
            } else {
                light.emission = value
            }
            let errors = compile(bad).unwrap_err();
            assert!(
                errors
                    .iter()
                    .any(|e| e.field.starts_with("state_overrides.light_transport"))
            );
        }
    }
}

#[test]
fn state_optics_require_exact_unique_keys_and_canonicalize_order() {
    let original = pack();
    let expected = compile(original.clone()).unwrap();
    let mut reversed = original.clone();
    for block in &mut reversed.blocks {
        block.value.state_overrides.reverse();
    }
    assert_eq!(
        compile(reversed).unwrap().authority_digest,
        expected.authority_digest
    );
    for duplicate in [true, false] {
        let mut bad = original.clone();
        let block = &mut bad
            .blocks
            .iter_mut()
            .find(|b| !b.value.state_overrides.is_empty())
            .unwrap()
            .value;
        if duplicate {
            block.state_overrides.push(block.state_overrides[0].clone());
        } else {
            block.state_overrides[0].values = vec![u16::MAX];
        }
        assert!(
            compile(bad)
                .unwrap_err()
                .iter()
                .any(|e| e.field == "state_overrides")
        );
    }
}

#[test]
fn state_optics_reject_invalid_encoded_values_before_publication() {
    let compiled = compile(pack()).unwrap();
    let bytes = write_cooked(&cook(&compiled, &[], &[]).unwrap()).unwrap();
    // Header: 8-byte magic, u32 version, four 32-byte identities, u32 count.
    let mut at = 144;
    let length = u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as usize;
    at += 2 + length + 2;
    let count = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
    at += 4 + count * 2 + 3;
    for value in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
        for field in [0, 4] {
            let mut bad = bytes.clone();
            bad[at + field..at + field + 4].copy_from_slice(&value.to_bits().to_le_bytes());
            assert!(read_cooked(&bad).is_err());
        }
    }
}
