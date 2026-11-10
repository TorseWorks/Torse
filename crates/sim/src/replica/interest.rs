//! Interest cuts reuse retained terrain ownership; only entering terrain is captured.
use super::*;
use torse_world::BlockStateId;

fn remove(counts: &mut BTreeMap<BlockStateId, u64>, id: BlockStateId, n: u64) -> Result<(), Fault> {
    if id.is_empty() {
        return Ok(());
    }
    let count = counts
        .get_mut(&id)
        .ok_or_else(|| Fault::invalid_data("interest state count missing"))?;
    *count = count
        .checked_sub(n)
        .ok_or_else(|| Fault::invalid_data("interest state count underflow"))?;
    if *count == 0 {
        counts.remove(&id);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn change(
    world: &WorldAuthority,
    binding: Binding,
    limits: ReplicaLimits,
    id: SubscriptionId,
    through: CommitCursor,
    sub: &mut Subscription,
    actor: ActorContext,
    scope: ReplicaScope,
    budget: OutputBudget,
) -> Result<WorldReplicaBatch, Fault> {
    let previous = sub.state.as_ref().ok_or_else(reset_required)?;
    if !sub.cache.valid {
        sub.cache = ProjectionCache::new(previous)?;
    }
    if sub.next_seq == u64::MAX {
        return Err(Fault::resource_limit("interest sequence exhausted"));
    }
    let mut retained = Cost::default();
    let mut counts = sub.cache.counts.clone();
    let mut removed_sections = Vec::new();
    let volume = u64::from(previous.side)
        .checked_pow(3)
        .ok_or_else(|| Fault::resource_limit("interest volume"))?;
    for section in &previous.sections {
        if scope.sections.contains(&section.pos) && readable(world, section.pos) {
            retained.sections(std::slice::from_ref(section))?;
        } else {
            removed_sections.push(section.pos);
            match &section.cells {
                SnapshotCells::Uniform(id) => remove(&mut counts, *id, volume)?,
                SnapshotCells::Dense(cells) => {
                    for id in cells {
                        remove(&mut counts, *id, 1)?;
                    }
                }
            }
        }
    }
    let entering = scope
        .sections
        .iter()
        .filter(|pos| {
            previous
                .sections
                .binary_search_by_key(*pos, |s| s.pos)
                .is_err()
        })
        .copied()
        .collect();
    let available = OutputBudget {
        max_items: limits
            .snapshot
            .max_items
            .saturating_sub(u32::try_from(retained.items).unwrap_or(u32::MAX)),
        max_bytes: limits.snapshot.max_bytes.saturating_sub(retained.bytes),
    };
    // Project all entities against the new grants, but capture entering terrain only.
    let mut next = project_sections(world, &actor, &scope, &entering, available)?;
    let incoming = ProjectionCache::new(&next)?;
    for (id, n) in incoming.counts {
        let count = counts.entry(id).or_default();
        *count = count
            .checked_add(n)
            .ok_or_else(|| Fault::resource_limit("interest state count overflow"))?;
    }
    let mut state_cost = Cost::default();
    for id in counts.keys() {
        state_cost.entries::<StateEntry>(1)?;
        state_cost.key(
            world
                .registry()
                .definition_key(*id)
                .ok_or_else(|| Fault::invalid_data("interest state missing"))?,
        )?;
        state_cost.bytes(std::mem::size_of_val(
            world.registry().state_values(*id).unwrap_or(&[]),
        ))?;
    }
    let mut cost = retained;
    cost.entries::<WorldReplicaSeed>(1)?;
    cost.sections(&next.sections)?;
    cost = cost.add(state_cost)?;
    for entity in &next.entities {
        cost.entity(entity)?;
    }
    if !cost.fits(limits.snapshot) {
        return Err(Fault::resource_limit("interest snapshot budget"));
    }
    let states: Vec<_> = counts
        .keys()
        .map(|id| StateEntry {
            id: *id,
            block: world
                .registry()
                .definition_key(*id)
                .expect("preflight resolved")
                .clone(),
            values: world.registry().state_values(*id).unwrap_or(&[]).to_vec(),
        })
        .collect();
    let changed: Vec<_> = next
        .entities
        .iter()
        .filter(|entity| {
            previous
                .entities
                .binary_search_by_key(&entity.id, |e| e.id)
                .map_or(true, |i| previous.entities[i] != **entity)
        })
        .collect();
    let removed_entities: Vec<_> = previous
        .entities
        .iter()
        .filter(|entity| {
            next.entities
                .binary_search_by_key(&entity.id, |e| e.id)
                .is_err()
        })
        .map(|e| e.id)
        .collect();
    let mut batch_cost = Cost::default();
    batch_cost.entries::<WorldReplicaBatch>(1)?;
    batch_cost.states(&states)?;
    batch_cost.sections(&next.sections)?;
    batch_cost.entries::<SectionPos>(removed_sections.len())?;
    batch_cost.entries::<EntityId>(removed_entities.len())?;
    for entity in &changed {
        batch_cost.entity(entity)?;
    }
    if !batch_cost.fits(smaller(budget, limits.queue)) {
        return Err(Fault::resource_limit("interest output budget"));
    }
    let changes = ReplicaChanges {
        presentation: Vec::new(),
        states: states.clone(),
        sections: next.sections.clone(),
        removed_sections,
        entities: changed.into_iter().cloned().collect(),
        removed_entities,
    };
    let previous = sub.state.as_mut().expect("checked live projection");
    previous
        .sections
        .retain(|section| !changes.removed_sections.contains(&section.pos));
    previous.sections.append(&mut next.sections);
    previous.sections.sort_by_key(|section| section.pos);
    previous.entities = next.entities;
    previous.states = states;
    sub.cache = ProjectionCache {
        valid: true,
        cost,
        states: state_cost,
        counts,
    };
    sub.actor = actor;
    sub.scope = scope;
    let batch = WorldReplicaBatch {
        binding,
        subscription: id,
        seq: ReplicaSeq(sub.next_seq),
        through,
        changes,
    };
    sub.next_seq += 1;
    Ok(batch)
}
