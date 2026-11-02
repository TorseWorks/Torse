//! Conversion from prepared or committed edits to stable exchange payloads.
//!
//! A commit record holds runtime state identifiers because those are what the
//! store applies. The persistent payload must not: it carries stable content
//! identity instead, so a save file survives a registry rebuild, a content pack
//! update, or a different process. This module is the single place that
//! conversion happens.

use torse_world::{BlockStateRegistry, ChangeSet, ComponentSchemas, ExchangeError};

use crate::coordinator::{Committed, PreparedChange};

/// Build the durable projection while keeping the prepared in-memory change
/// intact for publication and replication. Empty projections still carry the
/// commit position so recovery never skips a sequence.
pub fn persistent_changes_from_prepared(
    registry: &BlockStateRegistry,
    schemas: &ComponentSchemas,
    prepared: &PreparedChange,
) -> Result<ChangeSet, ExchangeError> {
    let mut changes = changes_from_prepared(registry, prepared)?;
    changes.retain_persistent(schemas)?;
    Ok(changes)
}

/// Build a complete domain payload before publication, for journal reservation.
///
/// The caller owns the resulting payload independently of the preparation. A
/// rejected reservation can drop both values without changing world state.
/// Transient presentation drafts stay on the preparation for live replication;
/// world replay deliberately does not retain or redeliver committed effects.
pub fn changes_from_prepared(
    registry: &BlockStateRegistry,
    prepared: &PreparedChange,
) -> Result<ChangeSet, ExchangeError> {
    let mut changes = ChangeSet::from_edits(prepared.meta(), registry, prepared.edits())?;
    changes.entity_moves = prepared.entity_moves().to_vec();
    changes.component_edits = prepared.component_edits().to_vec();
    changes.entity_lifecycle = prepared.entity_lifecycle().cloned();
    Ok(changes)
}

/// Build the persistent change payload of a committed transaction.
pub fn changes_from_committed(
    registry: &BlockStateRegistry,
    committed: &Committed,
) -> Result<ChangeSet, ExchangeError> {
    let mut changes = ChangeSet::from_edits(committed.meta, registry, &committed.edits)?;
    changes.entity_moves = committed.entity_moves.clone();
    changes.component_edits = committed.component_edits.clone();
    changes.entity_lifecycle = committed.entity_lifecycle.clone();
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use torse_content::{compile, m0_fixture_pack};
    use torse_types::{
        BlockPos, CommitSeq, ContentKey, SectionPos, TransactionId, TransactionOutcome, WorldId,
    };
    use torse_world::{
        BlockStateId, BlockStateRegistry, ChangeEdit, Section, TerrainQuery, WorldBounds,
    };

    use super::*;
    use crate::coordinator::CommitCoordinator;
    use crate::phase::Phase;
    use crate::transaction::{OrderingKey, Transaction};
    use torse_world::SectionStore;

    fn key(value: &str) -> ContentKey {
        ContentKey::parse(value).expect("test key is valid")
    }

    fn setup() -> (BlockStateRegistry, SectionStore, BlockStateId) {
        let content = compile(m0_fixture_pack()).expect("fixture content compiles");
        let registry = BlockStateRegistry::build(&content).expect("registry builds");
        let stone = registry
            .default_state(&key("base:stone"))
            .expect("stone exists");
        let bounds = WorldBounds::new(0, 64).expect("bounds are valid");
        let mut store = SectionStore::new(16, bounds).expect("side is valid");
        store.request(SectionPos::ZERO).expect("request works");
        store
            .install(SectionPos::ZERO, Section::empty(16).expect("side is valid"))
            .expect("install works");
        store.finish_load(SectionPos::ZERO).expect("finish works");
        (registry, store, stone)
    }

    #[test]
    fn committed_edits_become_stable_content() {
        let (registry, mut store, stone) = setup();
        let mut coordinator = CommitCoordinator::new(WorldId(1));
        let mut transaction = Transaction::new(
            TransactionId(1),
            key("base:test"),
            OrderingKey {
                phase: Phase::Interaction,
                rule_priority: 0,
                stable_task_key: 0,
                local_sequence: 0,
            },
        );
        transaction.write_block(BlockPos::new(1, 1, 1), stone);
        let result = coordinator
            .commit(&mut store, &transaction)
            .expect("commit runs");
        let committed = match &result.outcome {
            TransactionOutcome::Committed { value, .. } => value,
            TransactionOutcome::Rejected(code) => panic!("unexpected rejection {code:?}"),
        };

        let changes = changes_from_committed(&registry, committed).expect("conversion works");
        assert_eq!(changes.meta, committed.meta);
        assert_eq!(changes.edits.len(), 1);
        let ChangeEdit { pos, content } = &changes.edits[0];
        assert_eq!(*pos, BlockPos::new(1, 1, 1));
        match content {
            torse_world::CellContent::Block { block, values } => {
                assert_eq!(block, &key("base:stone"));
                assert!(values.is_empty());
            }
            torse_world::CellContent::Empty => panic!("expected a block"),
        }
        assert_eq!(committed.meta.cursor.seq, CommitSeq(1));

        // Applying the payload to a fresh world reproduces the edit.
        let mut target =
            SectionStore::new(16, WorldBounds::new(0, 64).expect("bounds")).expect("side");
        target.request(SectionPos::ZERO).expect("request works");
        target
            .install(SectionPos::ZERO, Section::empty(16).expect("side"))
            .expect("install works");
        target.finish_load(SectionPos::ZERO).expect("finish works");
        changes.apply(&mut target, &registry).expect("apply works");
        assert_eq!(
            target.query(BlockPos::new(1, 1, 1)),
            TerrainQuery::Ready(stone)
        );
    }
}
