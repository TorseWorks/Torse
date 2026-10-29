//! Explicit versioned field encodings for durable execution and network payloads.
//! The field lists below are the format, not Rust memory layout. No native ABI,
//! serde defaults, unchecked collection allocations or trailing bytes are accepted.
use crate::value::*;
use crate::{CodecLimits, Decoder, EncodeResult, Encoder, StorageCodec};
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::Arc;
use torse_sim::durable::*;
use torse_sim::replica::*;
use torse_types::*;
use torse_world::*;

pub const EXECUTION_VERSION: u16 = 1;
pub const EXECUTION_COMMIT_VERSION: u16 = 3;
pub const EXECUTION_SNAPSHOT_VERSION: u16 = 2;
pub trait Wire: Sized {
    fn write(&self, out: &mut Encoder<'_>) -> EncodeResult<()>;
    fn read(input: &mut Decoder<'_>) -> EncodeResult<Self>;
}
impl Wire for bool {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        u8::from(*self).write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        match u8::read(d)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid("invalid boolean")),
        }
    }
}

/// Cost of a field body, excluding its root envelope. Uses the actual bounded
/// encoder and decoder so allocation/node accounting stays identical to replay.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WireCost {
    pub encoded_bytes: u64,
    pub owned_bytes: u64,
    pub items: u32,
}

pub fn measure_inner<T: Wire>(
    value: &T,
    budget: OutputBudget,
    limits: CodecLimits,
) -> EncodeResult<WireCost> {
    let encoded_budget = OutputBudget {
        max_bytes: budget.max_bytes.min(u64::from(limits.max_frame_bytes)),
        ..budget
    };
    let mut bytes = Vec::new();
    let mut e = Encoder::new(&mut bytes, encoded_budget, limits);
    value.write(&mut e)?;
    e.finish();
    let mut d = Decoder::new(&bytes, budget, limits);
    let _value = T::read(&mut d)?;
    if !d.is_complete() {
        return Err(invalid("incomplete cost validation"));
    }
    Ok(WireCost {
        encoded_bytes: bytes.len() as u64,
        owned_bytes: d.guard().used_bytes(),
        items: d.guard().used_items(),
    })
}
pub fn encode<T: Wire>(
    value: &T,
    out: &mut Vec<u8>,
    budget: OutputBudget,
    limits: CodecLimits,
) -> EncodeResult<()> {
    let budget = OutputBudget {
        max_bytes: budget.max_bytes.min(u64::from(limits.max_frame_bytes)),
        ..budget
    };
    let mut e = Encoder::new(out, budget, limits);
    e.count_items(1)?;
    e.write_u16(EXECUTION_VERSION)?;
    value.write(&mut e)?;
    e.finish();
    Ok(())
}
pub fn decode<T: Wire>(input: &[u8], budget: OutputBudget, limits: CodecLimits) -> EncodeResult<T> {
    if input.len() > limits.max_frame_bytes as usize {
        return Err(Fault::resource_limit("wire frame too large"));
    }
    let mut d = Decoder::new(input, budget, limits);
    d.charge_items(1)?;
    d.charge_bytes(size_of::<T>() as u64)?;
    if d.read_u16()? != EXECUTION_VERSION {
        return Err(Fault::incompatible("execution/wire version"));
    }
    let value = T::read(&mut d)?;
    if !d.is_complete() {
        return Err(Fault::invalid_data("trailing wire bytes"));
    }
    Ok(value)
}
macro_rules! number {
    ($ty:ty,$write:ident,$read:ident) => {
        impl Wire for $ty {
            fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
                e.$write(*self)
            }
            fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
                d.$read()
            }
        }
    };
}
number!(u8, write_u8, read_u8);
number!(u16, write_u16, read_u16);
number!(u32, write_u32, read_u32);
number!(u64, write_u64, read_u64);
number!(u128, write_u128, read_u128);
number!(i64, write_i64, read_i64);
impl Wire for i32 {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        e.write_u32(*self as u32)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok(d.read_u32()? as i32)
    }
}
macro_rules! wrapped { ($($ty:ty => $inner:ty),*) => {$(impl Wire for $ty {
    fn write(&self,e:&mut Encoder<'_>)->EncodeResult<()> { self.0.write(e) }
    fn read(d:&mut Decoder<'_>)->EncodeResult<Self> { Ok(Self(<$inner>::read(d)?)) }
})*}; }
wrapped!(WorldId=>u128,WorldEpoch=>u128,SessionId=>u128,ActorContextId=>u128,SubscriptionId=>u128,
    EntityId=>u128,EventId=>u128,TransactionId=>u128,CommitSeq=>u64,ReplicaSeq=>u64,Tick=>u64,BlockStateId=>u16);
impl Wire for ContentKey {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        write_key(e, self)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        read_key(d)
    }
}
impl Wire for ComponentValue {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        write_component_value(e, self, 0)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        read_component_value(d)
    }
}
impl<T: Wire> Wire for Vec<T> {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        write_items(e, self.len(), "wire collection")?;
        for item in self {
            item.write(e)?;
        }
        Ok(())
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        let n = read_items(d, 1, "wire collection")?;
        charge_vec::<T>(d, n)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(T::read(d)?);
        }
        Ok(out)
    }
}
impl<T: Wire> Wire for Arc<T> {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        self.as_ref().write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        let control = (2 * size_of::<usize>()).next_multiple_of(std::mem::align_of::<T>());
        d.charge_bytes((size_of::<T>() + control) as u64)?;
        Ok(Arc::new(T::read(d)?))
    }
}
impl<T: Wire> Wire for Option<T> {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        match self {
            None => 0u8.write(e),
            Some(v) => {
                1u8.write(e)?;
                v.write(e)
            }
        }
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        match u8::read(d)? {
            0 => Ok(None),
            1 => Ok(Some(T::read(d)?)),
            _ => Err(invalid("invalid option tag")),
        }
    }
}
impl<K: Wire + Ord, V: Wire> Wire for BTreeMap<K, V> {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        write_items(e, self.len(), "wire map")?;
        for (k, v) in self {
            k.write(e)?;
            v.write(e)?;
        }
        Ok(())
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        let n = read_items(d, 2, "wire map")?;
        charge_vec::<(K, V)>(d, n)?;
        let mut out = BTreeMap::new();
        for _ in 0..n {
            let key = K::read(d)?;
            ascending(out.last_key_value().map(|(k, _)| k), &key, "wire map")?;
            out.insert(key, V::read(d)?);
        }
        Ok(out)
    }
}
impl Wire for [u64; 3] {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        for x in self {
            x.write(e)?;
        }
        Ok(())
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok([u64::read(d)?, u64::read(d)?, u64::read(d)?])
    }
}
impl Wire for Digest {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        e.write(&self.0)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        let mut v = [0; 32];
        v.copy_from_slice(d.read(32)?);
        Ok(Self(v))
    }
}
/// Lists fields in their exact wire order. Root/container accounting is handled
/// by encode/decode and Vec/Map before values are allocated.
macro_rules! fields { ($ty:ty { $($field:ident),* }) => { impl Wire for $ty {
    fn write(&self,e:&mut Encoder<'_>)->EncodeResult<()> { $(self.$field.write(e)?;)* Ok(()) }
    fn read(d:&mut Decoder<'_>)->EncodeResult<Self> {Ok(Self { $($field:Wire::read(d)?),* })}
} }; }
fields!(BlockPos { x, y, z });
fields!(SectionPos { x, y, z });
fields!(WorldBounds { min_y, max_y });
fields!(Compatibility {
    authority_schema,
    rules_digest,
    content_digest
});
fields!(Binding {
    world,
    epoch,
    compatibility
});
fields!(CommitCursor { world, seq });
fields!(ResumePoint {
    tick,
    next_phase,
    next_step
});
fields!(SnapshotMeta {
    cursor,
    compatibility,
    resume_at
});
fields!(CommitMeta {
    cursor,
    previous,
    compatibility,
    resume_at
});
fields!(RequestId {
    world,
    epoch,
    session,
    seq
});
wrapped!(RequestSeq=>u64);
fields!(Viewer { session, actor });
impl<S: Wire> Wire for WorldSnapshot<S> {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        self.meta.write(e)?;
        self.state.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok(Self {
            meta: Wire::read(d)?,
            state: Wire::read(d)?,
        })
    }
}
impl<S: Wire> Wire for CommitRecord<S> {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        self.meta.write(e)?;
        self.changes.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok(Self {
            meta: Wire::read(d)?,
            changes: Wire::read(d)?,
        })
    }
}
impl<S: Wire> Wire for torse_types::ReplicaSeed<S> {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        self.binding.write(e)?;
        self.subscription.write(e)?;
        self.baseline.write(e)?;
        self.state.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok(Self {
            binding: Wire::read(d)?,
            subscription: Wire::read(d)?,
            baseline: Wire::read(d)?,
            state: Wire::read(d)?,
        })
    }
}
impl<S: Wire> Wire for torse_types::ReplicaBatch<S> {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        self.binding.write(e)?;
        self.subscription.write(e)?;
        self.seq.write(e)?;
        self.through.write(e)?;
        self.changes.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok(Self {
            binding: Wire::read(d)?,
            subscription: Wire::read(d)?,
            seq: Wire::read(d)?,
            through: Wire::read(d)?,
            changes: Wire::read(d)?,
        })
    }
}
impl Wire for Snapshot {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        crate::snapshot::write_snapshot(e, self)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        crate::snapshot::read_snapshot(d)
    }
}
fields!(BlockStamp {
    definition_version,
    schema,
    schema_version
});
fields!(ComponentStamp { owner, version });
fields!(ArchiveManifest { blocks, components });
impl Wire for SnapshotArchive {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        e.write(b"TORSEAR1")?;
        self.origin.write(e)?;
        self.manifest.write(e)?;
        self.snapshot.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        if d.read(8)? != b"TORSEAR1" {
            return Err(Fault::incompatible("world archive version"));
        }
        Ok(Self {
            origin: Wire::read(d)?,
            manifest: Wire::read(d)?,
            snapshot: Wire::read(d)?,
        })
    }
}
impl Wire for ExecutionArchive {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        e.write(b"TORSEEX1")?;
        self.world.write(e)?;
        self.runtime.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        if d.read(8)? != b"TORSEEX1" {
            return Err(Fault::incompatible("execution archive version"));
        }
        Ok(Self {
            world: Wire::read(d)?,
            runtime: Wire::read(d)?,
        })
    }
}
impl Wire for ChangeSet {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        crate::journal::write_changes(e, self)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        crate::journal::read_changes(d, crate::WORLD_FORMAT_VERSION)
    }
}
impl Wire for torse_sim::Phase {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        self.index_u16().write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Self::from_index(u16::read(d)?).ok_or_else(|| invalid("unknown phase"))
    }
}
impl Wire for torse_sim::action::ActionTarget {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        match self {
            Self::Block(p) => {
                0u8.write(e)?;
                p.write(e)
            }
            Self::Entity(id) => {
                1u8.write(e)?;
                id.write(e)
            }
        }
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        match u8::read(d)? {
            0 => Ok(Self::Block(Wire::read(d)?)),
            1 => Ok(Self::Entity(Wire::read(d)?)),
            _ => Err(invalid("unknown target")),
        }
    }
}
impl Wire for RejectCode {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        let tag = match self {
            Self::Conflict => 0,
            Self::NotLoaded => 1,
            Self::MissingTarget => 2,
            Self::PermissionDenied => 3,
            Self::QuotaExceeded => 4,
            Self::InvalidData => 5,
            Self::StaleSession => 6,
            Self::DuplicateMismatch => 7,
            Self::SequenceExpired => 8,
            Self::Stopping => 9,
        };
        e.write_u8(tag)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok(match d.read_u8()? {
            0 => Self::Conflict,
            1 => Self::NotLoaded,
            2 => Self::MissingTarget,
            3 => Self::PermissionDenied,
            4 => Self::QuotaExceeded,
            5 => Self::InvalidData,
            6 => Self::StaleSession,
            7 => Self::DuplicateMismatch,
            8 => Self::SequenceExpired,
            9 => Self::Stopping,
            _ => return Err(invalid("unknown rejection")),
        })
    }
}
impl Wire for ReplyStatus {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        match self {
            Self::Committed => 0u8.write(e),
            Self::ReadOnly => 1u8.write(e),
            Self::Rejected(r) => {
                2u8.write(e)?;
                r.write(e)
            }
        }
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        match u8::read(d)? {
            0 => Ok(Self::Committed),
            1 => Ok(Self::ReadOnly),
            2 => Ok(Self::Rejected(Wire::read(d)?)),
            _ => Err(invalid("unknown reply")),
        }
    }
}
impl Wire for CellContent {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        match self {
            Self::Empty => 0u8.write(e),
            Self::Block { block, values } => {
                1u8.write(e)?;
                block.write(e)?;
                values.write(e)
            }
        }
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        match u8::read(d)? {
            0 => Ok(Self::Empty),
            1 => Ok(Self::Block {
                block: Wire::read(d)?,
                values: Wire::read(d)?,
            }),
            _ => Err(invalid("unknown cell")),
        }
    }
}
impl Wire for SnapshotCells {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        match self {
            Self::Uniform(v) => {
                0u8.write(e)?;
                v.write(e)
            }
            Self::Dense(v) => {
                1u8.write(e)?;
                v.write(e)
            }
        }
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        match u8::read(d)? {
            0 => Ok(Self::Uniform(Wire::read(d)?)),
            1 => Ok(Self::Dense(Wire::read(d)?)),
            _ => Err(invalid("unknown cells")),
        }
    }
}
fields!(Call {
    action,
    version,
    target,
    parameters
});
fields!(Reply { status, fields, at });
fields!(Receipt { id, input, reply });
fields!(Window {
    epoch,
    session,
    high,
    receipts
});
fields!(EventDraft {
    tick,
    rule,
    payload
});
fields!(TimedEvent { id, event });
fields!(StableCell { pos, content });
fields!(StableMove {
    id,
    previous,
    delta,
    half
});
fields!(StableRelocation {
    id,
    previous,
    destination,
    policy
});
fields!(StableSpawn {
    position,
    components
});
fields!(ComponentEdit {
    id,
    schema,
    previous,
    fields
});
fields!(EntityState {
    id,
    position_bits,
    components
});
fn write_legacy_transaction(t: &StoredTransaction, e: &mut Encoder<'_>) -> EncodeResult<()> {
    t.id.write(e)?;
    t.source.write(e)?;
    t.phase.write(e)?;
    t.priority.write(e)?;
    t.task_key.write(e)?;
    t.local.write(e)?;
    t.reads.write(e)?;
    t.writes.write(e)?;
    t.moves.write(e)?;
    t.components.write(e)?;
    t.spawns.write(e)?;
    t.despawns.write(e)
}
impl Wire for StoredTransaction {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        if !self.clearance.is_empty() {
            return Err(invalid(
                "placement conditions require frozen transaction tag 3",
            ));
        }
        if !self.relocations.is_empty() {
            return Err(invalid("relocations require frozen transaction tag 4"));
        }
        write_legacy_transaction(self, e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok(Self {
            id: Wire::read(d)?,
            source: Wire::read(d)?,
            phase: Wire::read(d)?,
            priority: Wire::read(d)?,
            task_key: Wire::read(d)?,
            local: Wire::read(d)?,
            reads: Wire::read(d)?,
            writes: Wire::read(d)?,
            clearance: Vec::new(),
            moves: Wire::read(d)?,
            relocations: Vec::new(),
            components: Wire::read(d)?,
            spawns: Wire::read(d)?,
            despawns: Wire::read(d)?,
        })
    }
}
fields!(Accepted { id, input });
impl Wire for FrozenTask {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        match &self.transaction {
            None => 0u8.write(e)?,
            Some(group) => {
                group.validate()?;
                if group
                    .members
                    .iter()
                    .any(|member| !member.relocations.is_empty())
                {
                    4u8.write(e)?;
                    group.id.write(e)?;
                    write_items(e, group.members.len(), "frozen group")?;
                    for member in &group.members {
                        write_legacy_transaction(member, e)?;
                        member.clearance.write(e)?;
                        member.relocations.write(e)?;
                    }
                } else if group.members.iter().any(|m| !m.clearance.is_empty()) {
                    3u8.write(e)?;
                    group.id.write(e)?;
                    write_items(e, group.members.len(), "frozen group")?;
                    for member in &group.members {
                        write_legacy_transaction(member, e)?;
                        member.clearance.write(e)?;
                    }
                } else if group.members.len() == 1 && group.id == group.members[0].id {
                    1u8.write(e)?;
                    group.members[0].write(e)?;
                } else {
                    2u8.write(e)?;
                    group.id.write(e)?;
                    group.members.write(e)?;
                }
            }
        }
        self.request.write(e)?;
        self.consume.write(e)?;
        self.schedule.write(e)?;
        self.cancel.write(e)?;
        self.status.write(e)?;
        self.output.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        let tag = u8::read(d)?;
        let transaction = match tag {
            0 => None,
            1 => {
                charge_vec::<StoredTransaction>(d, 1)?;
                let member = StoredTransaction::read(d)?;
                Some(StoredTransactionGroup {
                    id: member.id,
                    members: vec![member],
                })
            }
            2..=4 => {
                let id = TransactionId::read(d)?;
                // Check the group bound before allocating its member vector.
                let n = read_items(d, 1, "frozen group")?;
                if n == 0 || n > torse_sim::transaction::MAX_GROUP_MEMBERS {
                    return Err(invalid("invalid frozen group size"));
                }
                charge_vec::<StoredTransaction>(d, n)?;
                let mut members = Vec::with_capacity(n);
                let mut remaining_checks = torse_sim::transaction::MAX_CLEARANCE_CHECKS;
                for _ in 0..n {
                    let mut member = StoredTransaction::read(d)?;
                    if tag == 3 || tag == 4 {
                        let checks = read_items(d, 1, "placement conditions")?;
                        remaining_checks = remaining_checks
                            .checked_sub(checks)
                            .ok_or_else(|| invalid("too many placement conditions"))?;
                        charge_vec::<StableCell>(d, checks)?;
                        member.clearance = Vec::with_capacity(checks);
                        for _ in 0..checks {
                            member.clearance.push(StableCell::read(d)?);
                        }
                    }
                    if tag == 4 {
                        let count = read_items(d, 1, "relocations")?;
                        if count > 1 {
                            return Err(invalid("too many relocations"));
                        }
                        charge_vec::<StableRelocation>(d, count)?;
                        member.relocations = Vec::with_capacity(count);
                        for _ in 0..count {
                            member.relocations.push(StableRelocation::read(d)?);
                        }
                    }
                    members.push(member);
                }
                Some(StoredTransactionGroup { id, members })
            }
            _ => return Err(invalid("invalid frozen transaction tag")),
        };
        if let Some(group) = &transaction {
            group.validate()?;
        }
        Ok(Self {
            transaction,
            request: Wire::read(d)?,
            consume: Wire::read(d)?,
            schedule: Wire::read(d)?,
            cancel: Wire::read(d)?,
            status: Wire::read(d)?,
            output: Wire::read(d)?,
        })
    }
}
fields!(PhaseWork { tasks, next });
fields!(RuntimeState {
    rules,
    next_event,
    events,
    windows,
    work
});
fields!(ExecutionSnapshot {
    world,
    runtime,
    parked
});
fields!(ParkedSection { digest, entities });
fields!(SectionChange {
    page,
    load,
    fresh,
    reference
});
fields!(RuntimeProgress {
    next,
    next_event,
    events,
    windows
});
impl Wire for RuntimeUpdate {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        match self {
            Self::Full(state) => {
                0u8.write(e)?;
                state.write(e)
            }
            Self::Progress(progress) => {
                1u8.write(e)?;
                progress.write(e)
            }
        }
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        match u8::read(d)? {
            0 => Ok(Self::Full(Wire::read(d)?)),
            1 => Ok(Self::Progress(Wire::read(d)?)),
            _ => Err(invalid("unknown runtime update")),
        }
    }
}
fields!(ExecutionChanges {
    world,
    runtime,
    outcome,
    cancelled,
    section
});
fields!(torse_sim::SystemSpec {
    id,
    phase,
    rule_priority,
    before,
    after
});
fields!(StateEntry { id, block, values });
fields!(SnapshotSection { pos, cells });
fields!(ReplicaEntity {
    id,
    position_bits,
    components
});
fields!(ReplicaSnapshot {
    side,
    bounds,
    states,
    sections,
    entities
});
fields!(ReplicaChanges {
    states,
    sections,
    removed_sections,
    entities,
    removed_entities
});

pub struct ExecutionCodec<M> {
    pub limits: CodecLimits,
    model: PhantomData<fn() -> M>,
}
impl<M> Clone for ExecutionCodec<M> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<M> Copy for ExecutionCodec<M> {}
impl<M> ExecutionCodec<M> {
    pub fn new(limits: CodecLimits) -> Result<Self, Fault> {
        if limits.max_depth < crate::MIN_WORLD_DEPTH
            || limits.max_frame_bytes == 0
            || limits.max_field_bytes == 0
        {
            return Err(invalid("execution codec limits"));
        }
        Ok(Self {
            limits,
            model: PhantomData,
        })
    }
}
impl<M: AuthorityData<Snapshot = ExecutionSnapshot, Changes = ExecutionChanges>> StorageCodec<M>
    for ExecutionCodec<M>
{
    fn encode_snapshot(
        &self,
        v: &ModelSnapshot<M>,
        out: &mut Vec<u8>,
        b: OutputBudget,
    ) -> EncodeResult<()> {
        if v.meta != v.state.world.meta {
            return Err(invalid("execution snapshot metadata"));
        }
        let b = OutputBudget {
            max_bytes: b.max_bytes.min(u64::from(self.limits.max_frame_bytes)),
            ..b
        };
        let mut e = Encoder::new(out, b, self.limits);
        e.count_items(1)?;
        e.write_u16(EXECUTION_SNAPSHOT_VERSION)?;
        v.write(&mut e)?;
        e.finish();
        Ok(())
    }
    fn decode_snapshot(&self, v: &[u8], b: OutputBudget) -> EncodeResult<ModelSnapshot<M>> {
        if v.len() > self.limits.max_frame_bytes as usize {
            return Err(Fault::resource_limit("execution snapshot too large"));
        }
        let mut d = Decoder::new(v, b, self.limits);
        d.charge_items(1)?;
        d.charge_bytes(size_of::<ModelSnapshot<M>>() as u64)?;
        let v = match d.read_u16()? {
            1 => WorldSnapshot {
                meta: Wire::read(&mut d)?,
                state: ExecutionSnapshot {
                    world: Wire::read(&mut d)?,
                    runtime: Wire::read(&mut d)?,
                    parked: BTreeMap::new(),
                },
            },
            EXECUTION_SNAPSHOT_VERSION => ModelSnapshot::<M>::read(&mut d)?,
            _ => return Err(Fault::incompatible("execution snapshot version")),
        };
        if !d.is_complete() {
            return Err(invalid("trailing execution snapshot bytes"));
        }
        if v.meta != v.state.world.meta {
            return Err(invalid("execution snapshot metadata"));
        }
        Ok(v)
    }
    fn encode_commit(
        &self,
        v: &ModelCommit<M>,
        out: &mut Vec<u8>,
        b: OutputBudget,
    ) -> EncodeResult<()> {
        if v.meta != v.changes.world.meta {
            return Err(invalid("execution commit metadata"));
        }
        let b = OutputBudget {
            max_bytes: b.max_bytes.min(u64::from(self.limits.max_frame_bytes)),
            ..b
        };
        let mut encoder = Encoder::new(out, b, self.limits);
        encoder.count_items(1)?;
        encoder.write_u16(EXECUTION_COMMIT_VERSION)?;
        v.write(&mut encoder)?;
        encoder.finish();
        Ok(())
    }
    fn decode_commit(&self, v: &[u8], b: OutputBudget) -> EncodeResult<ModelCommit<M>> {
        if v.len() > self.limits.max_frame_bytes as usize {
            return Err(Fault::resource_limit("wire frame too large"));
        }
        let mut d = Decoder::new(v, b, self.limits);
        d.charge_items(1)?;
        d.charge_bytes(size_of::<ModelCommit<M>>() as u64)?;
        let v = match d.read_u16()? {
            EXECUTION_VERSION => CommitRecord {
                meta: Wire::read(&mut d)?,
                changes: ExecutionChanges {
                    world: Wire::read(&mut d)?,
                    runtime: RuntimeUpdate::Full(Wire::read(&mut d)?),
                    outcome: Wire::read(&mut d)?,
                    cancelled: Wire::read(&mut d)?,
                    section: None,
                },
            },
            2 => CommitRecord {
                meta: Wire::read(&mut d)?,
                changes: ExecutionChanges {
                    world: Wire::read(&mut d)?,
                    runtime: Wire::read(&mut d)?,
                    outcome: Wire::read(&mut d)?,
                    cancelled: Wire::read(&mut d)?,
                    section: None,
                },
            },
            EXECUTION_COMMIT_VERSION => ModelCommit::<M>::read(&mut d)?,
            _ => return Err(Fault::incompatible("execution commit version")),
        };
        if !d.is_complete() {
            return Err(invalid("trailing execution commit bytes"));
        }
        if v.meta != v.changes.world.meta {
            return Err(invalid("execution commit metadata"));
        }
        Ok(v)
    }
}
