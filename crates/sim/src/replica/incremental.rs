//! Derived projection bookkeeping. Authority mutation remains exclusive to the source.
use super::*;
use torse_world::{BlockStateId, Section};

#[derive(Default)]
pub(super) struct ProjectionCache {
    pub(super) valid: bool,
    pub(super) cost: Cost,
    pub(super) states: Cost,
    pub(super) counts: BTreeMap<BlockStateId, u64>,
}
impl ProjectionCache {
    pub(super) fn new(snapshot: &ReplicaSnapshot) -> Result<Self, Fault> {
        let mut counts = BTreeMap::new();
        let volume = u64::from(snapshot.side)
            .checked_pow(3)
            .ok_or_else(|| Fault::resource_limit("replica volume overflow"))?;
        for section in &snapshot.sections {
            match &section.cells {
                SnapshotCells::Uniform(id) => add_count(&mut counts, *id, volume)?,
                SnapshotCells::Dense(cells) => {
                    for id in cells {
                        add_count(&mut counts, *id, 1)?;
                    }
                }
            }
        }
        let mut states = Cost::default();
        states.states(&snapshot.states)?;
        let mut cost = states;
        cost.entries::<WorldReplicaSeed>(1)?;
        cost.sections(&snapshot.sections)?;
        for entity in &snapshot.entities {
            cost.entity(entity)?;
        }
        Ok(Self {
            valid: true,
            cost,
            states,
            counts,
        })
    }
}
fn add_count(
    counts: &mut BTreeMap<BlockStateId, u64>,
    id: BlockStateId,
    count: u64,
) -> Result<(), Fault> {
    if !id.is_empty() {
        let value = counts.entry(id).or_default();
        *value = value
            .checked_add(count)
            .ok_or_else(|| Fault::resource_limit("replica count overflow"))?;
    }
    Ok(())
}

pub(super) struct Dirty {
    sections: BTreeSet<SectionPos>,
    cells: Vec<(SectionPos, BlockStateId, BlockStateId)>,
    entities: BTreeSet<EntityId>,
}
impl Dirty {
    pub(super) fn capture(world: &WorldAuthority, prepared: &PreparedChange) -> Option<Self> {
        let mut seen = BTreeSet::new();
        let cells = prepared
            .edits()
            .iter()
            .map(|edit| {
                // Foreign/stale preparations are still adjudicated by publish.
                // Repeated targets require the full final-state reference path.
                if !seen.insert(edit.pos) {
                    return None;
                }
                Some((
                    edit.pos.to_section(world.store().side()).ok()?.0,
                    world.store().query(edit.pos).ready()?,
                    edit.state,
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        let mut entities: BTreeSet<_> = prepared
            .entity_moves()
            .iter()
            .map(|m| m.id)
            .chain(prepared.component_edits().iter().map(|e| e.id))
            .collect();
        if let Some(lifecycle) = prepared.entity_lifecycle() {
            entities.extend(
                lifecycle
                    .spawned
                    .iter()
                    .chain(&lifecycle.despawned)
                    .map(|e| e.id),
            );
        }
        Some(Self {
            sections: prepared.touched().iter().copied().collect(),
            cells,
            entities,
        })
    }
}

fn section_cost(section: &Section) -> Result<Cost, Fault> {
    let mut cost = Cost::default();
    cost.entries::<SnapshotSection>(1)?;
    if !matches!(section.storage(), SectionStorage::Uniform(_)) {
        cost.bytes(section.cell_count() * size_of::<u16>())?;
    }
    Ok(cost)
}
fn same_section(previous: &SnapshotCells, current: &Section) -> bool {
    match (previous, current.storage()) {
        (SnapshotCells::Uniform(a), SectionStorage::Uniform(b)) => a == b,
        (SnapshotCells::Dense(a), _) => a.iter().copied().eq(current.values()),
        _ => false,
    }
}
fn entity_cost(
    world: &WorldAuthority,
    actor: &ActorContext,
    entity: &Entity,
) -> Result<Cost, Fault> {
    let mut cost = Cost::default();
    cost.entries::<ReplicaEntity>(1)?;
    for key in entity
        .components
        .ids()
        .filter(|key| visible(world, actor, entity, key))
    {
        cost.entries::<(ContentKey, ComponentFields)>(1)?;
        cost.key(key)?;
        cost.fields(entity.components.component(key).expect("component"))?;
    }
    Ok(cost)
}
fn same_entity(
    world: &WorldAuthority,
    actor: &ActorContext,
    previous: &ReplicaEntity,
    current: &Entity,
) -> bool {
    let mut count = 0;
    let equal = previous.position_bits == current.position.to_bits()
        && current
            .components
            .ids()
            .filter(|key| visible(world, actor, current, key))
            .all(|key| {
                count += 1;
                previous.components.get(key) == current.components.component(key)
            });
    equal && count == previous.components.len()
}

pub(super) fn update(
    world: &WorldAuthority,
    binding: Binding,
    limits: ReplicaLimits,
    id: SubscriptionId,
    through: CommitCursor,
    sub: &mut Subscription,
    dirty: &Dirty,
) -> Result<(), Fault> {
    let previous = sub.state.as_ref().expect("active subscription");
    if !sub.cache.valid {
        sub.cache = ProjectionCache::new(previous)?;
    }
    let mut sections = vec![];
    let mut entities = vec![];
    let mut cost = sub.cache.cost;
    let mut changed_cost = Cost::default();
    let mut removed = 0;
    for pos in dirty
        .sections
        .iter()
        .filter(|p| sub.scope.sections.contains(p))
    {
        let old = previous.sections.binary_search_by_key(pos, |s| s.pos);
        // Runtime terrain loading is not exposed by ReplicaSource. Keep full
        // projection as fallback if that invariant is extended in the future.
        let (Ok(index), true) = (old, readable(world, *pos)) else {
            return full_projection(world, binding, limits, id, through, sub);
        };
        let section = world.store().section(*pos).expect("readable section");
        if same_section(&previous.sections[index].cells, section) {
            continue;
        }
        let mut before = Cost::default();
        before.sections(std::slice::from_ref(&previous.sections[index]))?;
        let after = section_cost(section)?;
        cost = cost.subtract(before)?.add(after)?;
        changed_cost = changed_cost.add(after)?;
        sections.push((index, section));
    }
    for id in &dirty.entities {
        let before = previous.entities.binary_search_by_key(id, |e| e.id).ok();
        let after = world
            .entities()
            .handle_of(*id)
            .and_then(|h| world.entities().get(h))
            .filter(|e| in_scope(world, &sub.scope, e));
        if match (before, after) {
            (Some(i), Some(e)) => same_entity(world, &sub.actor, &previous.entities[i], e),
            (None, None) => true,
            _ => false,
        } {
            continue;
        }
        if let Some(index) = before {
            let mut value = Cost::default();
            value.entity(&previous.entities[index])?;
            cost = cost.subtract(value)?;
        }
        if let Some(entity) = after {
            let value = entity_cost(world, &sub.actor, entity)?;
            cost = cost.add(value)?;
            changed_cost = changed_cost.add(value)?;
        } else {
            removed += 1;
        }
        entities.push((*id, after));
    }
    let mut affected = BTreeMap::new();
    for (pos, old, new) in &dirty.cells {
        if old == new || !sub.scope.sections.contains(pos) {
            continue;
        }
        for state in [old, new] {
            affected
                .entry(*state)
                .or_insert_with(|| sub.cache.counts.contains_key(state));
        }
        if !old.is_empty() {
            let count = sub
                .cache
                .counts
                .get_mut(old)
                .ok_or_else(|| Fault::invalid_data("missing replica state count"))?;
            *count = count
                .checked_sub(1)
                .ok_or_else(|| Fault::invalid_data("replica count underflow"))?;
            if *count == 0 {
                sub.cache.counts.remove(old);
            }
        }
        add_count(&mut sub.cache.counts, *new, 1)?;
    }
    let symbols_changed = affected
        .into_iter()
        .any(|(id, was)| sub.cache.counts.contains_key(&id) != was);
    let state_cost = if symbols_changed {
        let mut value = Cost::default();
        for &id in sub.cache.counts.keys() {
            value.entries::<StateEntry>(1)?;
            value.key(
                world
                    .registry()
                    .definition_key(id)
                    .ok_or_else(|| Fault::invalid_data("unregistered replica state"))?,
            )?;
            value.bytes(std::mem::size_of_val(
                world.registry().state_values(id).unwrap_or(&[]),
            ))?;
        }
        value
    } else {
        sub.cache.states
    };
    cost = cost.subtract(sub.cache.states)?.add(state_cost)?;
    if !cost.fits(limits.snapshot) {
        return Err(Fault::resource_limit("replica snapshot budget"));
    }
    if sections.is_empty() && entities.is_empty() && !symbols_changed {
        return Ok(());
    }
    // Both complete projection and queued replacement are checked while all
    // selected world values are still borrowed, before cloning owned payloads.
    let mut batch_cost = state_cost.add(changed_cost)?;
    batch_cost.entries::<WorldReplicaBatch>(1)?;
    batch_cost.entries::<EntityId>(removed)?;
    let total = sub.queued.add(batch_cost)?;
    if sub.queue.len() >= limits.max_queued_batches
        || !total.fits(limits.queue)
        || sub.next_seq == u64::MAX
    {
        return Err(Fault::resource_limit("replica queue exhausted"));
    }
    let states = if symbols_changed {
        sub.cache
            .counts
            .keys()
            .map(|&id| StateEntry {
                id,
                block: world
                    .registry()
                    .definition_key(id)
                    .expect("preflight resolved")
                    .clone(),
                values: world.registry().state_values(id).unwrap_or(&[]).to_vec(),
            })
            .collect()
    } else {
        previous.states.clone()
    };
    let changed_sections = sections
        .iter()
        .map(|(i, section)| SnapshotSection {
            pos: previous.sections[*i].pos,
            cells: SnapshotCells::capture(section),
        })
        .collect();
    let mut changed_entities = vec![];
    let mut removed_entities = vec![];
    for (id, entity) in entities {
        if let Some(entity) = entity {
            changed_entities.push(ReplicaEntity {
                id,
                position_bits: entity.position.to_bits(),
                components: entity
                    .components
                    .ids()
                    .filter(|key| visible(world, &sub.actor, entity, key))
                    .map(|key| {
                        (
                            key.clone(),
                            entity.components.component(key).unwrap().clone(),
                        )
                    })
                    .collect(),
            });
        } else {
            removed_entities.push(id);
        }
    }
    let changes = ReplicaChanges {
        presentation: Vec::new(),
        states,
        sections: changed_sections,
        removed_sections: vec![],
        entities: changed_entities,
        removed_entities,
    };
    let state = sub.state.as_mut().unwrap();
    if symbols_changed {
        state.states = changes.states.clone();
    }
    for ((index, _), section) in sections.iter().zip(&changes.sections) {
        state.sections[*index] = section.clone();
    }
    for entity in &changes.entities {
        match state.entities.binary_search_by_key(&entity.id, |e| e.id) {
            Ok(i) => state.entities[i] = entity.clone(),
            Err(i) => state.entities.insert(i, entity.clone()),
        }
    }
    for id in &changes.removed_entities {
        if let Ok(i) = state.entities.binary_search_by_key(id, |e| e.id) {
            state.entities.remove(i);
        }
    }
    sub.cache.cost = cost;
    sub.cache.states = state_cost;
    sub.queue.push_back((
        WorldReplicaBatch {
            binding,
            subscription: id,
            seq: ReplicaSeq(sub.next_seq),
            through,
            changes,
        },
        batch_cost,
    ));
    sub.queued = total;
    sub.next_seq += 1;
    Ok(())
}
