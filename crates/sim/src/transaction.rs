//! Typed transactions and their deterministic arbitration order.
//!
//! A transaction records what it read and what it wants to change. The
//! coordinator checks the read set against the current authority before applying
//! anything, which is what prevents two systems from spending the same resource.
//!
//! Ordering is part of the contract, not an implementation detail. Transactions
//! are applied by `(phase, rule_priority, stable_task_key, local_sequence)` and,
//! for full totality, by transaction identity. None of those fields depends on a
//! thread number or on the order in which results happened to be produced.

use std::collections::BTreeMap;
use torse_types::{BlockPos, ContentKey, EntityHandle, EntityId, RejectCode, TransactionId};
use torse_world::{ComponentEdit, ComponentFields, Entity, EntityState, Position};

use crate::phase::Phase;

/// One cell a transaction read, with the value it observed.
///
/// The expectation is the block state rather than a global revision, so two
/// transactions touching different cells of the same section do not conflict.
/// Section-level revisions are coarser and can be added later for reads that
/// genuinely depend on a whole section.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockPrecondition {
    /// Cell that was read.
    pub pos: BlockPos,
    /// State observed at read time.
    pub expected: torse_world::BlockStateId,
}

/// One cell the transaction wants to write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mutation {
    /// Target cell.
    pub pos: BlockPos,
    /// State to write.
    pub state: torse_world::BlockStateId,
}

/// A requested translation whose final position is resolved during preparation.
///
/// The caller supplies an observed identity and position, never an unchecked
/// final position. Exact bits make signed zero and stale observations explicit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Movement {
    /// Runtime handle, including its generation.
    pub handle: EntityHandle,
    /// Persistent identity observed through that handle.
    pub id: EntityId,
    /// Source position observed by the producing system.
    pub previous_bits: [u64; 3],
    /// Requested translation, in world block units.
    pub delta_bits: [u64; 3],
    /// Positive collider half extents; the entity position is its center.
    pub half_bits: [u64; 3],
}

/// Creation request without a persistent identity; preparation proposes the next ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Spawn {
    pub position_bits: [u64; 3],
    pub components: BTreeMap<ContentKey, ComponentFields>,
}

/// Removal request with both runtime generation and the full observed stable state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Despawn {
    pub handle: EntityHandle,
    pub previous: EntityState,
}

/// How the destination of a relocation is checked.
///
/// This is not a movement sweep. Ordinary movement limits stay in force for
/// [`Movement`] requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DestinationPolicy {
    /// The body must be clear of loaded terrain.
    Clear,
    /// Every cell the body covers must be loaded. Overlap is allowed.
    Resident,
}

/// One exact relocation. Preparation writes the destination directly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Relocation {
    /// Persistent identity that must still be live.
    pub id: EntityId,
    /// Position observed by the planner, as IEEE-754 bits.
    pub previous_bits: [u64; 3],
    /// Finite destination inside the movement coordinate limit.
    pub destination_bits: [u64; 3],
    /// Destination residency rule.
    pub policy: DestinationPolicy,
}

/// Stable ordering key of one transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct OrderingKey {
    /// Phase that produced the transaction.
    pub phase: Phase,
    /// Rule priority inside the phase; lower commits first.
    pub rule_priority: i32,
    /// Caller-provided key derived from content and object identity.
    pub stable_task_key: u64,
    /// Sequence inside one task invocation.
    pub local_sequence: u32,
}

/// A prepared change that has not been committed yet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transaction {
    /// Stable transaction identity used for diagnostics and deduplication.
    pub id: TransactionId,
    /// Module or system that produced the changes.
    pub source: ContentKey,
    /// Arbitration order.
    pub ordering: OrderingKey,
    /// Reads that must still hold for the transaction to commit.
    pub preconditions: Vec<BlockPrecondition>,
    /// Writes applied atomically when every precondition holds.
    pub mutations: Vec<Mutation>,
    /// Final block shapes that must remain clear of ordinary entity occupancy.
    /// Each guard must match this transaction's final write at the same cell.
    pub clearance: Vec<Mutation>,
    /// Translations resolved against authoritative collision before publication.
    pub movements: Vec<Movement>,
    /// Schema-validated protected fields; only the schema owner may write them.
    pub components: Vec<ComponentEdit>,
    /// Creations in deterministic allocation order.
    pub spawns: Vec<Spawn>,
    /// Removals compare the complete observed state before publication.
    pub despawns: Vec<Despawn>,
    /// Exact relocations. They do not pass through movement sweep limits.
    pub relocations: Vec<Relocation>,
}

/// Upper bound on members in one atomic group.
pub const MAX_GROUP_MEMBERS: usize = 4;

/// Maximum placement occupancy conditions in one transaction or atomic group.
pub const MAX_CLEARANCE_CHECKS: usize = 256;

/// One atomic task contributed by several module sources.
///
/// A group is the unit of preparation, publication, and persistence: one
/// prepared record, one commit sequence number, one journal entry, one merged
/// change set. Members keep their own `source` so component ownership stays
/// per module; they share the task's ordering key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransactionGroup {
    /// Stable identity of the whole task, for diagnostics.
    pub id: TransactionId,
    /// Arbitration order shared by every member.
    pub ordering: OrderingKey,
    /// Members sorted by source; sources are unique.
    pub members: Vec<Transaction>,
}

impl TransactionGroup {
    /// Sort members by source and enforce the group rules.
    ///
    /// Members adopt the group's ordering key. Rejected groups are not
    /// partially constructed: `Err` leaves the caller's vector untouched.
    pub fn new(
        id: TransactionId,
        ordering: OrderingKey,
        mut members: Vec<Transaction>,
    ) -> Result<Self, RejectCode> {
        if members.is_empty() || members.len() > MAX_GROUP_MEMBERS {
            return Err(RejectCode::QuotaExceeded);
        }
        members.sort_by(|left, right| left.source.cmp(&right.source));
        for pair in members.windows(2) {
            if pair[0].source == pair[1].source {
                return Err(RejectCode::InvalidData);
            }
        }
        // Parallel-read semantics need disjoint writes across members:
        // one cell and one (entity, schema) may be written by one member only,
        // and an entity may not cross lifecycle and movement or edits at all.
        let mut cells = std::collections::BTreeSet::new();
        let mut component_targets = std::collections::BTreeSet::new();
        let mut lifecycle_entities = std::collections::BTreeSet::new();
        let mut touched_entities = std::collections::BTreeSet::new();
        let mut moved_entities = std::collections::BTreeSet::new();
        let mut lifecycle_members = 0;
        for member in &members {
            for mutation in &member.mutations {
                if !cells.insert(mutation.pos) {
                    return Err(RejectCode::Conflict);
                }
            }
            for edit in &member.components {
                if !component_targets.insert((edit.id, edit.schema.clone())) {
                    return Err(RejectCode::Conflict);
                }
                touched_entities.insert(edit.id);
            }
            for movement in &member.movements {
                // One entity may be moved by one member only: the merged
                // change set forbids duplicate identities, and a duplicate
                // would otherwise surface as an invariant stop at publish.
                if !moved_entities.insert(movement.id) {
                    return Err(RejectCode::Conflict);
                }
                touched_entities.insert(movement.id);
            }
            for relocation in &member.relocations {
                if !moved_entities.insert(relocation.id) {
                    return Err(RejectCode::Conflict);
                }
                touched_entities.insert(relocation.id);
            }
            if !member.spawns.is_empty() || !member.despawns.is_empty() {
                lifecycle_members += 1;
                for despawn in &member.despawns {
                    lifecycle_entities.insert(despawn.previous.id);
                }
            }
        }
        if lifecycle_members > 1 {
            return Err(RejectCode::InvalidData);
        }
        if lifecycle_entities
            .iter()
            .any(|id| touched_entities.contains(id))
        {
            return Err(RejectCode::Conflict);
        }
        for member in &mut members {
            member.ordering = ordering;
        }
        Ok(Self {
            id,
            ordering,
            members,
        })
    }
}

impl Transaction {
    /// Start an empty transaction.
    pub fn new(id: TransactionId, source: ContentKey, ordering: OrderingKey) -> Self {
        Self {
            id,
            source,
            ordering,
            preconditions: Vec::new(),
            mutations: Vec::new(),
            clearance: Vec::new(),
            movements: Vec::new(),
            components: Vec::new(),
            spawns: Vec::new(),
            despawns: Vec::new(),
            relocations: Vec::new(),
        }
    }

    /// Record a read that the commit must revalidate.
    pub fn read_block(&mut self, pos: BlockPos, expected: torse_world::BlockStateId) -> &mut Self {
        self.preconditions.push(BlockPrecondition { pos, expected });
        self
    }

    /// Record a write.
    pub fn write_block(&mut self, pos: BlockPos, state: torse_world::BlockStateId) -> &mut Self {
        self.mutations.push(Mutation { pos, state });
        self
    }

    /// Keep placement occupancy checks across planning, freezing and preparation.
    pub fn require_block_clear(
        &mut self,
        pos: BlockPos,
        state: torse_world::BlockStateId,
    ) -> &mut Self {
        self.clearance.push(Mutation { pos, state });
        self
    }

    /// Request creation; component ownership is checked against the transaction source.
    pub fn spawn_entity(
        &mut self,
        position: Position,
        components: BTreeMap<ContentKey, ComponentFields>,
    ) -> &mut Self {
        self.spawns.push(Spawn {
            position_bits: position.to_bits(),
            components,
        });
        self
    }

    /// Request removal from a read-only entity observation.
    pub fn despawn_entity(&mut self, handle: EntityHandle, previous: &Entity) -> &mut Self {
        self.despawns.push(Despawn {
            handle,
            previous: EntityState::capture(previous),
        });
        self
    }

    /// Request one collision-resolved move with an optimistic source position.
    pub fn move_entity(
        &mut self,
        handle: EntityHandle,
        id: EntityId,
        previous: Position,
        delta: [f64; 3],
        half: [f64; 3],
    ) -> &mut Self {
        self.movements.push(Movement {
            handle,
            id,
            previous_bits: previous.to_bits(),
            delta_bits: delta.map(f64::to_bits),
            half_bits: half.map(f64::to_bits),
        });
        self
    }

    /// Request an exact relocation. The destination is not a movement delta.
    pub fn relocate_entity(
        &mut self,
        id: EntityId,
        previous: Position,
        destination: Position,
        policy: DestinationPolicy,
    ) -> &mut Self {
        self.relocations.push(Relocation {
            id,
            previous_bits: previous.to_bits(),
            destination_bits: destination.to_bits(),
            policy,
        });
        self
    }

    /// Sort transactions into commit order.
    pub fn order(mut transactions: Vec<Self>) -> Vec<Self> {
        transactions.sort_by(|left, right| {
            left.ordering
                .cmp(&right.ordering)
                .then(left.id.0.cmp(&right.id.0))
        });
        transactions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: &str) -> ContentKey {
        ContentKey::parse(value).expect("test key is valid")
    }

    fn tx(id: u128, priority: i32, stable: u64, local: u32) -> Transaction {
        Transaction::new(
            TransactionId(id),
            key("base:test"),
            OrderingKey {
                phase: Phase::Simulation,
                rule_priority: priority,
                stable_task_key: stable,
                local_sequence: local,
            },
        )
    }

    #[test]
    fn order_follows_priority_key_and_sequence() {
        let ordered = Transaction::order(vec![
            tx(2, 5, 1, 0),
            tx(3, 1, 9, 0),
            tx(1, 5, 0, 2),
            tx(4, 5, 0, 1),
        ]);
        let ids: Vec<u128> = ordered.iter().map(|value| value.id.0).collect();
        assert_eq!(ids, vec![3, 4, 1, 2]);
    }

    #[test]
    fn identical_keys_fall_back_to_transaction_identity() {
        let ordered = Transaction::order(vec![tx(9, 0, 0, 0), tx(4, 0, 0, 0)]);
        let ids: Vec<u128> = ordered.iter().map(|value| value.id.0).collect();
        assert_eq!(ids, vec![4, 9]);
    }

    #[test]
    fn phase_sorts_before_priority() {
        let mut first = tx(1, 9, 0, 0);
        first.ordering.phase = Phase::Input;
        let mut second = tx(2, 0, 0, 0);
        second.ordering.phase = Phase::Publish;
        let ordered = Transaction::order(vec![second, first]);
        assert_eq!(ordered[0].ordering.phase, Phase::Input);
    }
}
