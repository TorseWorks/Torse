//! Dense snapshots shared by mirror regressions and release measurements.
use torse_net::ReplicaMirror;
use torse_sim::replica::*;
use torse_types::*;
use torse_world::*;

pub fn seed(sections: usize) -> WorldReplicaSeed {
    let binding = Binding {
        world: WorldId(1),
        epoch: WorldEpoch(1),
        compatibility: Compatibility {
            authority_schema: 1,
            rules_digest: Digest([0; 32]),
            content_digest: Digest([0; 32]),
        },
    };
    WorldReplicaSeed {
        binding,
        subscription: SubscriptionId(1),
        baseline: CommitCursor::initial(binding.world),
        state: ReplicaSnapshot {
            side: 16,
            bounds: WorldBounds::new(0, 32).unwrap(),
            states: [1, 2]
                .map(|id| StateEntry {
                    id: BlockStateId(id),
                    block: ContentKey::parse(&format!("test:block_{id}")).unwrap(),
                    values: Vec::new(),
                })
                .into(),
            sections: (0..sections)
                .map(|x| SnapshotSection {
                    pos: SectionPos::new(x as i64, 0, 0),
                    cells: SnapshotCells::Dense(vec![
                        BlockStateId(if x % 2 == 0 { 1 } else { 2 });
                        4096
                    ]),
                })
                .collect(),
            entities: Vec::new(),
        },
    }
}
pub fn changes(mirror: &ReplicaMirror) -> ReplicaChanges {
    ReplicaChanges {
        presentation: Vec::new(),
        states: mirror.state().states.clone(),
        sections: Vec::new(),
        removed_sections: Vec::new(),
        entities: Vec::new(),
        removed_entities: Vec::new(),
    }
}
pub fn batch(mirror: &ReplicaMirror, changes: ReplicaChanges) -> WorldReplicaBatch {
    WorldReplicaBatch {
        binding: mirror.binding,
        subscription: mirror.subscription,
        through: mirror.through,
        seq: ReplicaSeq(mirror.seq.0 + 1),
        changes,
    }
}
pub fn pointers(mirror: &ReplicaMirror) -> Vec<usize> {
    mirror
        .state()
        .sections
        .iter()
        .map(|s| match &s.cells {
            SnapshotCells::Dense(v) => v.as_ptr() as usize,
            _ => 0,
        })
        .collect()
}
