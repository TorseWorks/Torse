#[path = "support/replica.rs"]
mod fixture;
use fixture::*;
use torse_net::ReplicaMirror;
use torse_sim::replica::*;
use torse_types::*;
use torse_world::*;

fn reject(mirror: &mut ReplicaMirror, update: WorldReplicaBatch) {
    let before = mirror.clone();
    let buffers = pointers(mirror);
    assert_eq!(
        mirror.apply_interest(update).unwrap_err().code,
        FaultCode::ResetRequired
    );
    assert_eq!(mirror.state(), before.state());
    assert_eq!(
        (
            mirror.seq,
            mirror.through,
            mirror.binding,
            mirror.subscription
        ),
        (
            before.seq,
            before.through,
            before.binding,
            before.subscription
        )
    );
    assert_eq!(pointers(mirror), buffers);
}

#[test]
fn empty_and_entity_only_interest_reuse_every_dense_buffer() {
    let mut mirror = ReplicaMirror::from_seed(seed(64)).unwrap();
    let buffers = pointers(&mirror);
    let mut expected = mirror.state().clone();
    for _ in 0..10 {
        mirror
            .apply_interest(batch(&mirror, changes(&mirror)))
            .unwrap();
    }
    let mut update = changes(&mirror);
    let entity = ReplicaEntity {
        id: EntityId(1),
        position_bits: [0; 3],
        components: Default::default(),
    };
    update.entities.push(entity.clone());
    expected.entities.push(entity);
    mirror.apply_interest(batch(&mirror, update)).unwrap();
    assert_eq!(mirror.state(), &expected);
    assert_eq!(pointers(&mirror), buffers);
}

#[test]
fn removed_symbol_referenced_by_retained_terrain_is_atomically_rejected() {
    let mut mirror = ReplicaMirror::from_seed(seed(2)).unwrap();
    let mut update = changes(&mirror);
    update.states.pop();
    reject(
        &mut mirror,
        batch(&ReplicaMirror::from_seed(seed(2)).unwrap(), update),
    );
}

#[test]
fn symbol_removal_with_section_removal_or_replacement_matches_full_state() {
    for replacement in [false, true] {
        let mut mirror = ReplicaMirror::from_seed(seed(2)).unwrap();
        let first = pointers(&mirror)[0];
        let mut expected = mirror.state().clone();
        expected.states.pop();
        let mut update = changes(&mirror);
        update.states.pop();
        if replacement {
            let section = SnapshotSection {
                pos: SectionPos::new(1, 0, 0),
                cells: SnapshotCells::Uniform(BlockStateId(1)),
            };
            expected.sections[1] = section.clone();
            update.sections.push(section);
        } else {
            expected.sections.pop();
            update.removed_sections.push(SectionPos::new(1, 0, 0));
        }
        mirror.apply_interest(batch(&mirror, update)).unwrap();
        assert_eq!(mirror.state(), &expected);
        assert_eq!(pointers(&mirror)[0], first);
    }
}

#[test]
fn malformed_changes_and_sequence_errors_leave_the_entire_mirror_unchanged() {
    let mut mirror = ReplicaMirror::from_seed(seed(2)).unwrap();
    for case in 0..8 {
        let mut update = batch(&mirror, changes(&mirror));
        match case {
            0 => update.seq.0 += 1,
            1 => update.seq = mirror.seq,
            2 => update.binding.epoch.0 += 1,
            3 => update.changes.states.reverse(),
            4 => update.changes.sections.push(SnapshotSection {
                pos: SectionPos::ZERO,
                cells: SnapshotCells::Dense(vec![BlockStateId(1); 3]),
            }),
            5 => update.changes.sections.push(SnapshotSection {
                pos: SectionPos::ZERO,
                cells: SnapshotCells::Uniform(BlockStateId(99)),
            }),
            6 => update.changes.entities.push(ReplicaEntity {
                id: EntityId(1),
                position_bits: [f64::NAN.to_bits(), 0, 0],
                components: Default::default(),
            }),
            _ => {
                update
                    .changes
                    .sections
                    .push(mirror.state().sections[0].clone());
                update.changes.removed_sections.push(SectionPos::ZERO);
            }
        }
        reject(&mut mirror, update);
    }
}
