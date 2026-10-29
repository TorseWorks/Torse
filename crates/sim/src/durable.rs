//! Stable, bounded execution vocabulary shared with the journal and protocol.
//! Frozen tasks contain stable content/identity references, never runtime handles.
use crate::action::ActionTarget;
pub use crate::event_dependencies::{EventDependencies, EventDependency, MAX_EVENT_DEPENDENCIES};
use crate::{OrderingKey, Phase, Transaction, WorldAuthority};
use std::collections::BTreeMap;
use std::sync::Arc;
use torse_types::*;
use torse_world::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Call {
    pub action: ContentKey,
    pub version: u32,
    pub target: ActionTarget,
    pub parameters: ComponentFields,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplyStatus {
    Committed,
    ReadOnly,
    Rejected(RejectCode),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reply {
    pub status: ReplyStatus,
    pub fields: ComponentFields,
    pub at: CommitCursor,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Receipt {
    pub id: RequestId,
    pub input: Call,
    pub reply: Reply,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Window {
    pub epoch: WorldEpoch,
    pub session: SessionId,
    pub high: u64,
    pub receipts: Vec<Receipt>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventDraft {
    pub tick: Tick,
    pub rule: ContentKey,
    pub payload: ComponentFields,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimedEvent {
    pub id: EventId,
    pub event: EventDraft,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StableCell {
    pub pos: BlockPos,
    pub content: CellContent,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StableMove {
    pub id: EntityId,
    pub previous: [u64; 3],
    pub delta: [u64; 3],
    pub half: [u64; 3],
}

/// Exact relocation stored with an unpublished durable task.
///
/// `policy` is 0 for a clear destination and 1 for a loaded destination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StableRelocation {
    pub id: EntityId,
    pub previous: [u64; 3],
    pub destination: [u64; 3],
    pub policy: u8,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StableSpawn {
    pub position: [u64; 3],
    pub components: BTreeMap<ContentKey, ComponentFields>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredTransaction {
    pub id: TransactionId,
    pub source: ContentKey,
    pub phase: u16,
    pub priority: i32,
    pub task_key: u64,
    pub local: u32,
    pub reads: Vec<StableCell>,
    pub writes: Vec<StableCell>,
    /// Occupancy requirements, resolved by stable block identity after restart.
    pub clearance: Vec<StableCell>,
    pub moves: Vec<StableMove>,
    pub relocations: Vec<StableRelocation>,
    pub components: Vec<ComponentEdit>,
    pub spawns: Vec<StableSpawn>,
    pub despawns: Vec<EntityState>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Accepted {
    pub id: RequestId,
    pub input: Call,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenTask {
    pub transaction: Option<StoredTransactionGroup>,
    pub request: Option<Accepted>,
    pub consume: Option<EventId>,
    pub schedule: Vec<EventDraft>,
    pub cancel: Vec<EventId>,
    pub status: ReplyStatus,
    pub output: ComponentFields,
}
/// Stable members of one atomic task, in strictly increasing source order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredTransactionGroup {
    pub id: TransactionId,
    pub members: Vec<StoredTransaction>,
}
impl StoredTransactionGroup {
    pub fn single(world: &WorldAuthority, tx: &Transaction) -> Result<Self, Fault> {
        let group = Self {
            id: tx.id,
            members: vec![StoredTransaction::capture(world, tx)?],
        };
        group.validate()?;
        Ok(group)
    }
    pub fn capture(world: &WorldAuthority, group: &crate::TransactionGroup) -> Result<Self, Fault> {
        if group.members.iter().any(|m| m.ordering != group.ordering) {
            return Err(Fault::invalid_data("frozen group ordering mismatch"));
        }
        let stored = Self {
            id: group.id,
            members: group
                .members
                .iter()
                .map(|tx| StoredTransaction::capture(world, tx))
                .collect::<Result<_, _>>()?,
        };
        stored.validate()?;
        Ok(stored)
    }
    pub fn validate(&self) -> Result<(), Fault> {
        if self.members.is_empty() || self.members.len() > crate::transaction::MAX_GROUP_MEMBERS {
            return Err(Fault::invalid_data("invalid frozen group size"));
        }
        let first = &self.members[0];
        if self.members.windows(2).any(|w| w[0].source >= w[1].source)
            || self.members.iter().any(|m| {
                (m.phase, m.priority, m.task_key, m.local)
                    != (first.phase, first.priority, first.task_key, first.local)
            })
        {
            return Err(Fault::invalid_data("noncanonical frozen group"));
        }
        if Phase::from_index(first.phase).is_none() {
            return Err(Fault::invalid_data("unknown frozen group phase"));
        }
        // Validate stable targets directly, without resolving runtime handles
        // or copying component payloads during decoding and recovery.
        let mut cells = std::collections::BTreeSet::new();
        let mut components = std::collections::BTreeSet::new();
        let mut touched = std::collections::BTreeSet::new();
        let mut despawned = std::collections::BTreeSet::new();
        let mut lifecycle_members = 0;
        let mut clearance_count = 0usize;
        for member in &self.members {
            clearance_count = clearance_count
                .checked_add(member.clearance.len())
                .filter(|n| *n <= crate::transaction::MAX_CLEARANCE_CHECKS)
                .ok_or_else(|| Fault::invalid_data("too many placement conditions"))?;
            let mut guarded = std::collections::BTreeSet::new();
            for check in &member.clearance {
                if check.content == CellContent::Empty
                    || !guarded.insert(check.pos)
                    || member.writes.iter().rev().find(|w| w.pos == check.pos) != Some(check)
                {
                    return Err(Fault::invalid_data("invalid frozen placement condition"));
                }
            }
            for write in &member.writes {
                if !cells.insert(write.pos) {
                    return Err(Fault::invalid_data("duplicate group block"));
                }
            }
            for edit in &member.components {
                if !components.insert((edit.id, &edit.schema)) {
                    return Err(Fault::invalid_data("duplicate group component"));
                }
                touched.insert(edit.id);
            }
            let mut moved = std::collections::BTreeSet::new();
            for id in member
                .moves
                .iter()
                .map(|movement| movement.id)
                .chain(member.relocations.iter().map(|relocation| relocation.id))
            {
                if !moved.insert(id) {
                    return Err(Fault::invalid_data("duplicate group move"));
                }
                touched.insert(id);
            }
            if member.relocations.len() > 1
                || member
                    .relocations
                    .iter()
                    .any(|relocation| relocation.policy > 1)
                || (!member.relocations.is_empty()
                    && member.source != crate::action::relocation_source())
            {
                return Err(Fault::invalid_data("invalid relocation"));
            }
            if !member.spawns.is_empty() || !member.despawns.is_empty() {
                lifecycle_members += 1;
            }
            for entity in &member.despawns {
                if !despawned.insert(entity.id) {
                    return Err(Fault::invalid_data("duplicate group despawn"));
                }
            }
        }
        if lifecycle_members > 1 || despawned.iter().any(|id| touched.contains(id)) {
            return Err(Fault::invalid_data("overlapping group lifecycle"));
        }
        Ok(())
    }
    pub fn restore(&self, world: &WorldAuthority) -> Result<crate::TransactionGroup, RejectCode> {
        self.validate().map_err(|_| RejectCode::InvalidData)?;
        let members = self
            .members
            .iter()
            .map(|m| m.restore(world))
            .collect::<Result<Vec<_>, _>>()?;
        crate::TransactionGroup::new(self.id, members[0].ordering, members)
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhaseWork {
    pub tasks: Arc<Vec<FrozenTask>>,
    pub next: u32,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeState {
    pub rules: Arc<Vec<crate::SystemSpec>>,
    pub next_event: u128,
    pub events: Vec<TimedEvent>,
    pub windows: Vec<Window>,
    pub work: Option<PhaseWork>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionSnapshot {
    pub world: Snapshot,
    pub runtime: RuntimeState,
    pub parked: std::collections::BTreeMap<torse_types::SectionPos, ParkedSection>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParkedSection {
    pub digest: torse_types::Digest,
    pub entities: Vec<torse_types::EntityId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SectionChange {
    pub page: Snapshot,
    pub load: bool,
    pub fresh: bool,
    pub reference: ParkedSection,
}

/// Portable checkpoint data with explicit source versions and untouched runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionArchive {
    pub world: SnapshotArchive,
    pub runtime: RuntimeState,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionChanges {
    pub world: ChangeSet,
    pub runtime: RuntimeUpdate,
    pub outcome: Option<ReplyStatus>,
    pub cancelled: Option<Accepted>,
    pub section: Option<SectionChange>,
}

/// Full state freezes/finishes a phase. Progress reuses its immutable rule and
/// task tables; replay must resolve it against the immediately preceding state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeUpdate {
    Full(RuntimeState),
    Progress(RuntimeProgress),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeProgress {
    pub next: u32,
    pub next_event: u128,
    pub events: Vec<TimedEvent>,
    pub windows: Vec<Window>,
}

impl RuntimeUpdate {
    pub fn resolve(&self, before: &RuntimeState) -> Result<RuntimeState, Fault> {
        match self {
            Self::Full(state) => Ok(state.clone()),
            Self::Progress(progress) => {
                let work = before
                    .work
                    .as_ref()
                    .ok_or_else(|| Fault::invalid_data("progress without frozen work"))?;
                if work.next.checked_add(1) != Some(progress.next)
                    || progress.next as usize > work.tasks.len()
                {
                    return Err(Fault::invalid_data("nonconsecutive task progress"));
                }
                Ok(RuntimeState {
                    rules: Arc::clone(&before.rules),
                    next_event: progress.next_event,
                    events: progress.events.clone(),
                    windows: progress.windows.clone(),
                    work: Some(PhaseWork {
                        tasks: Arc::clone(&work.tasks),
                        next: progress.next,
                    }),
                })
            }
        }
    }
}

fn stable(registry: &BlockStateRegistry, id: BlockStateId) -> Result<CellContent, Fault> {
    if id.is_empty() {
        return Ok(CellContent::Empty);
    }
    Ok(CellContent::Block {
        block: registry
            .definition_key(id)
            .ok_or_else(|| Fault::invalid_data("unknown task state"))?
            .clone(),
        values: registry.state_values(id).unwrap_or(&[]).to_vec(),
    })
}
fn resolve(registry: &BlockStateRegistry, value: &CellContent) -> Result<BlockStateId, RejectCode> {
    match value {
        CellContent::Empty => Ok(BlockStateId::EMPTY),
        CellContent::Block { block, values } => registry
            .state_of(block, values)
            .ok_or(RejectCode::InvalidData),
    }
}
impl StoredTransaction {
    pub fn capture(world: &WorldAuthority, tx: &Transaction) -> Result<Self, Fault> {
        Ok(Self {
            id: tx.id,
            source: tx.source.clone(),
            phase: tx.ordering.phase.index_u16(),
            priority: tx.ordering.rule_priority,
            task_key: tx.ordering.stable_task_key,
            local: tx.ordering.local_sequence,
            reads: tx
                .preconditions
                .iter()
                .map(|r| {
                    Ok(StableCell {
                        pos: r.pos,
                        content: stable(world.registry(), r.expected)?,
                    })
                })
                .collect::<Result<_, Fault>>()?,
            writes: tx
                .mutations
                .iter()
                .map(|r| {
                    Ok(StableCell {
                        pos: r.pos,
                        content: stable(world.registry(), r.state)?,
                    })
                })
                .collect::<Result<_, Fault>>()?,
            clearance: tx
                .clearance
                .iter()
                .map(|r| {
                    Ok(StableCell {
                        pos: r.pos,
                        content: stable(world.registry(), r.state)?,
                    })
                })
                .collect::<Result<_, Fault>>()?,
            moves: tx
                .movements
                .iter()
                .map(|m| StableMove {
                    id: m.id,
                    previous: m.previous_bits,
                    delta: m.delta_bits,
                    half: m.half_bits,
                })
                .collect(),
            relocations: tx
                .relocations
                .iter()
                .map(|relocation| StableRelocation {
                    id: relocation.id,
                    previous: relocation.previous_bits,
                    destination: relocation.destination_bits,
                    policy: match relocation.policy {
                        crate::DestinationPolicy::Clear => 0,
                        crate::DestinationPolicy::Resident => 1,
                    },
                })
                .collect(),
            components: tx.components.clone(),
            spawns: tx
                .spawns
                .iter()
                .map(|s| StableSpawn {
                    position: s.position_bits,
                    components: s.components.clone(),
                })
                .collect(),
            despawns: tx.despawns.iter().map(|s| s.previous.clone()).collect(),
        })
    }
    pub fn restore(&self, world: &WorldAuthority) -> Result<Transaction, RejectCode> {
        let mut tx = Transaction::new(
            self.id,
            self.source.clone(),
            OrderingKey {
                phase: Phase::from_index(self.phase).ok_or(RejectCode::InvalidData)?,
                rule_priority: self.priority,
                stable_task_key: self.task_key,
                local_sequence: self.local,
            },
        );
        for r in &self.reads {
            tx.read_block(r.pos, resolve(world.registry(), &r.content)?);
        }
        for r in &self.writes {
            tx.write_block(r.pos, resolve(world.registry(), &r.content)?);
        }
        for r in &self.clearance {
            tx.require_block_clear(r.pos, resolve(world.registry(), &r.content)?);
        }
        for m in &self.moves {
            let handle = world
                .entities()
                .handle_of(m.id)
                .ok_or(RejectCode::MissingTarget)?;
            tx.move_entity(
                handle,
                m.id,
                Position::from_bits(m.previous),
                m.delta.map(f64::from_bits),
                m.half.map(f64::from_bits),
            );
        }
        for relocation in &self.relocations {
            let policy = match relocation.policy {
                0 => crate::DestinationPolicy::Clear,
                1 => crate::DestinationPolicy::Resident,
                _ => return Err(RejectCode::InvalidData),
            };
            tx.relocate_entity(
                relocation.id,
                Position::from_bits(relocation.previous),
                Position::from_bits(relocation.destination),
                policy,
            );
        }
        tx.components = self.components.clone();
        for s in &self.spawns {
            tx.spawn_entity(Position::from_bits(s.position), s.components.clone());
        }
        for s in &self.despawns {
            let handle = world
                .entities()
                .handle_of(s.id)
                .ok_or(RejectCode::MissingTarget)?;
            tx.despawns.push(crate::transaction::Despawn {
                handle,
                previous: s.clone(),
            });
        }
        Ok(tx)
    }
}
