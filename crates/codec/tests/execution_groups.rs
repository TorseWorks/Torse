//! Frozen multi-source task compatibility, corruption, and budget checks.
use torse_codec::{
    CodecLimits, Encoder,
    wire::{self, Wire},
};
use torse_sim::durable::{
    FrozenTask, ReplyStatus, StableCell, StableRelocation, StoredTransaction,
    StoredTransactionGroup,
};
use torse_types::{BlockPos, ContentKey, EntityId, OutputBudget, TransactionId};
use torse_world::{CellContent, ComponentFields};

const LIMITS: CodecLimits = CodecLimits {
    max_frame_bytes: 65536,
    max_field_bytes: 4096,
    max_depth: 32,
};
const BUDGET: OutputBudget = OutputBudget {
    max_items: 4096,
    max_bytes: 65536,
};

fn member(source: &str, x: i64) -> StoredTransaction {
    StoredTransaction {
        id: TransactionId(7),
        source: ContentKey::parse(source).unwrap(),
        phase: 3,
        priority: 0,
        task_key: 42,
        local: 0,
        reads: Vec::new(),
        writes: vec![StableCell {
            pos: BlockPos::new(x, 0, 0),
            content: CellContent::Empty,
        }],
        clearance: Vec::new(),
        moves: Vec::new(),
        relocations: Vec::new(),
        components: Vec::new(),
        spawns: Vec::new(),
        despawns: Vec::new(),
    }
}
fn task(members: Vec<StoredTransaction>) -> FrozenTask {
    FrozenTask {
        transaction: Some(StoredTransactionGroup {
            id: TransactionId(7),
            members,
        }),
        request: None,
        consume: None,
        schedule: Vec::new(),
        cancel: Vec::new(),
        status: ReplyStatus::Committed,
        output: ComponentFields::new(),
    }
}
fn raw_group(members: &[StoredTransaction]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut e = Encoder::new(&mut bytes, BUDGET, LIMITS);
    1u16.write(&mut e).unwrap();
    2u8.write(&mut e).unwrap();
    TransactionId(7).write(&mut e).unwrap();
    members.to_vec().write(&mut e).unwrap();
    // request, consume, schedule, cancel, status, output in their v1 order.
    0u8.write(&mut e).unwrap();
    0u8.write(&mut e).unwrap();
    0u32.write(&mut e).unwrap();
    0u32.write(&mut e).unwrap();
    0u8.write(&mut e).unwrap();
    0u32.write(&mut e).unwrap();
    e.finish();
    bytes
}

#[test]
fn legacy_single_member_bytes_remain_readable_and_unchanged() {
    let t = task(vec![member("machine:module", 0)]);
    let mut old = Vec::new();
    let mut e = Encoder::new(&mut old, BUDGET, LIMITS);
    1u16.write(&mut e).unwrap();
    1u8.write(&mut e).unwrap();
    t.transaction.as_ref().unwrap().members[0]
        .write(&mut e)
        .unwrap();
    0u8.write(&mut e).unwrap();
    0u8.write(&mut e).unwrap();
    0u32.write(&mut e).unwrap();
    0u32.write(&mut e).unwrap();
    0u8.write(&mut e).unwrap();
    0u32.write(&mut e).unwrap();
    e.finish();
    assert_eq!(wire::decode::<FrozenTask>(&old, BUDGET, LIMITS).unwrap(), t);
    let mut encoded = Vec::new();
    wire::encode(&t, &mut encoded, BUDGET, LIMITS).unwrap();
    assert_eq!(encoded, old);
}

#[test]
fn group_roundtrip_truncation_and_budget_rollback() {
    let t = task(vec![
        member("inventory:module", 0),
        member("machine:module", 1),
    ]);
    let mut bytes = Vec::new();
    wire::encode(&t, &mut bytes, BUDGET, LIMITS).unwrap();
    assert_eq!(bytes[2], 2);
    assert_eq!(
        wire::decode::<FrozenTask>(&bytes, BUDGET, LIMITS).unwrap(),
        t
    );
    for end in 0..bytes.len() {
        assert!(wire::decode::<FrozenTask>(&bytes[..end], BUDGET, LIMITS).is_err());
    }
    let mut out = vec![9];
    assert!(
        wire::encode(
            &t,
            &mut out,
            OutputBudget {
                max_bytes: 12,
                ..BUDGET
            },
            LIMITS
        )
        .is_err()
    );
    assert_eq!(out, [9]);
    assert!(
        wire::decode::<FrozenTask>(
            &bytes,
            OutputBudget {
                max_items: 1,
                ..BUDGET
            },
            LIMITS
        )
        .is_err()
    );
}

#[test]
fn malformed_groups_reject_before_recovery() {
    let inventory = member("inventory:module", 0);
    let machine = member("machine:module", 1);
    let mut overlap = machine.clone();
    overlap.writes[0].pos = BlockPos::new(0, 0, 0);
    let mut ordering = machine.clone();
    ordering.local = 3;
    for members in [
        vec![],
        vec![inventory.clone(); 5],
        vec![machine, inventory.clone()],
        vec![inventory.clone(), inventory.clone()],
        vec![inventory.clone(), overlap],
        vec![inventory, ordering],
    ] {
        let bytes = raw_group(&members);
        assert!(wire::decode::<FrozenTask>(&bytes, BUDGET, LIMITS).is_err());
        let mut out = vec![9];
        assert!(wire::encode(&task(members), &mut out, BUDGET, LIMITS).is_err());
        assert_eq!(out, [9]);
    }
}

fn guarded_task() -> FrozenTask {
    let mut t = task(vec![member("world:module", 0)]);
    let m = &mut t.transaction.as_mut().unwrap().members[0];
    m.writes[0].content = CellContent::Block {
        block: ContentKey::parse("base:stone").unwrap(),
        values: vec![],
    };
    m.clearance = m.writes.clone();
    t
}

fn raw_guarded_task(t: &FrozenTask) -> Vec<u8> {
    let group = t.transaction.as_ref().unwrap();
    let mut bytes = vec![];
    let mut e = Encoder::new(&mut bytes, BUDGET, LIMITS);
    1u16.write(&mut e).unwrap();
    3u8.write(&mut e).unwrap();
    group.id.write(&mut e).unwrap();
    (group.members.len() as u32).write(&mut e).unwrap();
    for member in &group.members {
        let mut legacy = member.clone();
        legacy.clearance.clear();
        legacy.write(&mut e).unwrap();
        member.clearance.write(&mut e).unwrap();
    }
    t.request.write(&mut e).unwrap();
    t.consume.write(&mut e).unwrap();
    t.schedule.write(&mut e).unwrap();
    t.cancel.write(&mut e).unwrap();
    t.status.write(&mut e).unwrap();
    t.output.write(&mut e).unwrap();
    e.finish();
    bytes
}

#[test]
fn guarded_task_has_explicit_layout_roundtrip_truncation_and_budgets() {
    let t = guarded_task();
    let mut bytes = vec![];
    wire::encode(&t, &mut bytes, BUDGET, LIMITS).unwrap();
    assert_eq!(bytes, raw_guarded_task(&t));
    assert_eq!(
        wire::decode::<FrozenTask>(&bytes, BUDGET, LIMITS).unwrap(),
        t
    );
    for end in 0..bytes.len() {
        assert!(wire::decode::<FrozenTask>(&bytes[..end], BUDGET, LIMITS).is_err());
    }
    assert!(
        wire::decode::<FrozenTask>(
            &bytes,
            OutputBudget {
                max_items: 1,
                ..BUDGET
            },
            LIMITS
        )
        .is_err()
    );
    let mut out = vec![9];
    assert!(
        wire::encode(
            &t,
            &mut out,
            OutputBudget {
                max_bytes: 12,
                ..BUDGET
            },
            LIMITS
        )
        .is_err()
    );
    assert_eq!(out, [9]);
    // The legacy standalone writer must refuse instead of silently losing guards.
    assert!(wire::encode(&t.transaction.unwrap().members[0], &mut out, BUDGET, LIMITS).is_err());
    assert_eq!(out, [9]);
}

#[test]
fn malformed_placement_conditions_reject_during_decode() {
    let t = guarded_task();
    for mode in 0..4 {
        let mut invalid = t.clone();
        let member = &mut invalid.transaction.as_mut().unwrap().members[0];
        match mode {
            0 => member.clearance[0].pos.x += 1,
            1 => member.clearance[0].content = CellContent::Empty,
            2 => member.clearance.push(member.clearance[0].clone()),
            _ => member.clearance.resize(257, member.clearance[0].clone()),
        }
        let bytes = raw_guarded_task(&invalid);
        assert!(wire::decode::<FrozenTask>(&bytes, BUDGET, LIMITS).is_err());
        let mut out = vec![9];
        assert!(wire::encode(&invalid, &mut out, BUDGET, LIMITS).is_err());
        assert_eq!(out, [9]);
    }
}

#[test]
fn relocation_groups_roundtrip_on_their_own_tag() {
    let mut stored = member("world:module/relocation", 0);
    stored.writes.clear();
    stored.relocations.push(StableRelocation {
        id: EntityId(4),
        previous: [0; 3],
        destination: [1; 3],
        policy: 1,
    });
    let original = task(vec![stored]);
    let mut bytes = Vec::new();
    wire::encode(&original, &mut bytes, BUDGET, LIMITS).expect("encode");
    assert_eq!(bytes[2], 4, "relocation groups use tag 4");
    let decoded: FrozenTask = wire::decode(&bytes, BUDGET, LIMITS).expect("decode");
    assert_eq!(decoded, original);
}
