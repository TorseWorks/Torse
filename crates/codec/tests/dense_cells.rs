use torse_codec::{
    CodecLimits, Decoder, EncodeResult, Encoder,
    wire::{self, Wire},
};
use torse_types::{Fault, OutputBudget};
use torse_world::{BlockStateId, SnapshotCells};
struct Legacy(Vec<BlockStateId>);
impl Wire for Legacy {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        1u8.write(e)?;
        self.0.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        if u8::read(d)? != 1 {
            return Err(Fault::invalid_data("not dense"));
        }
        Ok(Self(Wire::read(d)?))
    }
}
const BUDGET: OutputBudget = OutputBudget {
    max_items: 100_000,
    max_bytes: 1_000_000,
};
const LIMITS: CodecLimits = CodecLimits {
    max_frame_bytes: 1_000_000,
    max_field_bytes: 1_000_000,
    max_depth: 16,
};
#[test]
fn bulk_dense_cells_keep_legacy_bytes_and_owned_budget_costs() {
    for length in [0, 1, 127, 128, 129, 4096, 65537] {
        let ids: Vec<_> = (0..length).map(|i| BlockStateId((i * 17) as u16)).collect();
        let dense = SnapshotCells::Dense(ids.clone());
        let legacy = Legacy(ids);
        let mut a = vec![];
        let mut b = vec![];
        wire::encode(&dense, &mut a, BUDGET, LIMITS).unwrap();
        wire::encode(&legacy, &mut b, BUDGET, LIMITS).unwrap();
        assert_eq!(a, b);
        assert_eq!(
            wire::decode::<SnapshotCells>(&b, BUDGET, LIMITS).unwrap(),
            dense
        );
        assert_eq!(
            wire::measure_inner(&dense, BUDGET, LIMITS).unwrap(),
            wire::measure_inner(&legacy, BUDGET, LIMITS).unwrap()
        );
    }
}
#[test]
fn malformed_dense_lengths_and_budget_failures_remain_transactional() {
    let dense = SnapshotCells::Dense(vec![BlockStateId(0x1234); 4096]);
    let mut bytes = vec![];
    wire::encode(&dense, &mut bytes, BUDGET, LIMITS).unwrap();
    for end in [3, 6, 7, bytes.len() - 1] {
        assert!(wire::decode::<SnapshotCells>(&bytes[..end], BUDGET, LIMITS).is_err());
    }
    let tiny = OutputBudget {
        max_items: 4095,
        ..BUDGET
    };
    assert!(wire::decode::<SnapshotCells>(&bytes, tiny, LIMITS).is_err());
    let mut output = vec![7, 8, 9];
    assert!(
        wire::encode(
            &dense,
            &mut output,
            OutputBudget {
                max_bytes: 128,
                ..BUDGET
            },
            LIMITS
        )
        .is_err()
    );
    assert_eq!(output, vec![7, 8, 9]);
}
