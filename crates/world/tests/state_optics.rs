use torse_content::{LightTransport, compile, m0_fixture_pack};
use torse_world::{BlockStateId, BlockStateRegistry};

#[test]
fn effective_optics_match_cooked_rows_without_changing_state_ids() {
    let original = m0_fixture_pack();
    let before = BlockStateRegistry::build(&compile(original.clone()).unwrap()).unwrap();
    let mut pack = original;
    let block = &mut pack
        .blocks
        .iter_mut()
        .find(|b| !b.value.state_overrides.is_empty())
        .unwrap()
        .value;
    block.light_transport = LightTransport {
        absorption: 0.5,
        emission: 1.0,
    };
    block.state_overrides[0].light_transport = Some(LightTransport {
        absorption: 0.0,
        emission: 0.0,
    });
    let compiled = compile(pack).unwrap();
    let after = BlockStateRegistry::build(&compiled).unwrap();
    assert_eq!(before.state_count(), after.state_count());
    for id in 1..before.state_count() {
        let state = BlockStateId(id as u16);
        assert_eq!(before.definition_key(state), after.definition_key(state));
        assert_eq!(before.state_values(state), after.state_values(state));
    }
    let cooked = torse_content::cook(&compiled, &[], &[]).unwrap();
    for row in cooked.states {
        let state = after.state_of(&row.block, &row.values).unwrap();
        assert_eq!(after.light_transport(state), Some(row.light_transport));
    }
    assert_eq!(after.light_transport(BlockStateId::EMPTY), None);
    assert_eq!(after.light_transport(BlockStateId(u16::MAX)), None);
}
