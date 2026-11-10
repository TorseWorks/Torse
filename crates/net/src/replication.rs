//! Atomic application after complete frame decode. No partial mutation on errors.
use std::collections::BTreeSet;
use torse_sim::replica::*;
use torse_types::*;
use torse_world::SnapshotCells;
#[derive(Clone)]
pub struct ReplicaMirror {
    pub binding: Binding,
    pub subscription: SubscriptionId,
    pub through: CommitCursor,
    pub seq: ReplicaSeq,
    state: ReplicaSnapshot,
}
fn invalid() -> Fault {
    Fault::new(FaultCode::ResetRequired, "invalid or discontinuous replica")
}
fn sorted<T: Ord>(values: impl IntoIterator<Item = T>) -> bool {
    let mut old = None;
    for value in values {
        if old.as_ref().is_some_and(|x| x >= &value) {
            return false;
        }
        old = Some(value);
    }
    true
}
fn validate(state: &ReplicaSnapshot) -> Result<(), Fault> {
    if state.side == 0
        || state.side > 64
        || state.bounds.min_y >= state.bounds.max_y
        || !sorted(state.states.iter().map(|s| s.id))
        || !sorted(state.sections.iter().map(|s| s.pos))
        || !sorted(state.entities.iter().map(|e| e.id))
    {
        return Err(invalid());
    }
    let ids: BTreeSet<_> = state.states.iter().map(|s| s.id.0).collect();
    if ids.contains(&0) {
        return Err(invalid());
    }
    let cells = state.side as usize * state.side as usize * state.side as usize;
    for section in &state.sections {
        let valid = |s: &torse_world::BlockStateId| s.0 == 0 || ids.contains(&s.0);
        match &section.cells {
            SnapshotCells::Uniform(s) if valid(s) => {}
            SnapshotCells::Dense(v) if v.len() == cells && v.iter().all(valid) => {}
            _ => return Err(invalid()),
        }
    }
    if state.entities.iter().any(|e| {
        e.id.0 == 0
            || e.position_bits
                .iter()
                .any(|x| !f64::from_bits(*x).is_finite())
    }) {
        return Err(invalid());
    }
    Ok(())
}
impl ReplicaMirror {
    /// Read the last fully validated state. Mutation goes through ordered
    /// batches so retained terrain remains valid without repeatedly scanning it.
    pub fn state(&self) -> &ReplicaSnapshot {
        &self.state
    }
    pub fn into_state(self) -> ReplicaSnapshot {
        self.state
    }
    pub fn from_seed(seed: WorldReplicaSeed) -> Result<Self, Fault> {
        if seed.subscription.0 == 0
            || seed.binding.world != seed.baseline.world
            || seed.binding.epoch.0 == 0
        {
            return Err(invalid());
        }
        validate(&seed.state)?;
        Ok(Self {
            binding: seed.binding,
            subscription: seed.subscription,
            through: seed.baseline,
            seq: ReplicaSeq(0),
            state: seed.state,
        })
    }
    pub fn apply(&mut self, batch: WorldReplicaBatch) -> Result<(), Fault> {
        self.apply_with_presentation(batch).map(|_| ())
    }

    /// Apply state and release this commit's bounded presentation events once.
    /// Duplicate, stale or invalid batches return no events and change nothing.
    /// There is no historical queue: dropping this result drops its effects.
    pub fn apply_with_presentation(
        &mut self,
        batch: WorldReplicaBatch,
    ) -> Result<Vec<torse_world::PresentationEvent>, Fault> {
        self.apply_ordered(batch, false)
    }

    /// Scope changes advance ReplicaSeq even at an unchanged world cursor.
    /// They never replay presentation effects or relax ordinary edit ordering.
    pub fn apply_interest(&mut self, batch: WorldReplicaBatch) -> Result<(), Fault> {
        if !batch.changes.presentation.is_empty() {
            return Err(invalid());
        }
        self.apply_ordered(batch, true).map(|_| ())
    }

    fn apply_ordered(
        &mut self,
        batch: WorldReplicaBatch,
        interest: bool,
    ) -> Result<Vec<torse_world::PresentationEvent>, Fault> {
        if batch.binding != self.binding
            || batch.subscription != self.subscription
            || batch.seq.0 != self.seq.0.checked_add(1).ok_or_else(invalid)?
            || batch.through.world != self.binding.world
            || batch.through.seq < self.through.seq
            || (!interest && batch.through.seq == self.through.seq)
        {
            return Err(invalid());
        }
        let c = batch.changes;
        torse_world::presentation_budget(
            c.presentation.iter().map(|e| &e.draft),
            torse_world::MAX_PRESENTATION_PER_COMMIT,
        )
        .map_err(|_| invalid())?;
        if !sorted(c.presentation.iter().map(|e| e.id.index))
            || c.presentation.iter().any(|event| {
                event.id.epoch != self.binding.epoch
                    || event.id.cursor != batch.through
                    || event.id.index as usize >= torse_world::MAX_PRESENTATION_PER_COMMIT
            })
        {
            return Err(invalid());
        }
        if !sorted(c.sections.iter().map(|s| s.pos))
            || !sorted(c.entities.iter().map(|e| e.id))
            || !sorted(c.removed_sections.iter().copied())
            || !sorted(c.removed_entities.iter().copied())
            || c.sections
                .iter()
                .any(|s| c.removed_sections.contains(&s.pos))
            || c.entities
                .iter()
                .any(|e| c.removed_entities.contains(&e.id))
        {
            return Err(invalid());
        }
        if !sorted(c.states.iter().map(|s| s.id)) || c.states.iter().any(|s| s.id.0 == 0) {
            return Err(invalid());
        }
        let valid_id = |id: &torse_world::BlockStateId| {
            id.0 == 0 || c.states.binary_search_by_key(id, |s| s.id).is_ok()
        };
        let cells = self.state.side as usize * self.state.side as usize * self.state.side as usize;
        let valid_section = |section: &torse_world::SnapshotSection| match &section.cells {
            SnapshotCells::Uniform(id) => valid_id(id),
            SnapshotCells::Dense(ids) => ids.len() == cells && ids.iter().all(valid_id),
        };
        if !c.sections.iter().all(valid_section)
            || c.entities.iter().any(|e| {
                e.id.0 == 0
                    || e.position_bits
                        .iter()
                        .any(|x| !f64::from_bits(*x).is_finite())
            })
        {
            return Err(invalid());
        }
        // With private snapshot ownership, retained cells already refer to the
        // old dictionary. Only deleting an old symbol can invalidate them.
        let removed_symbol = self.state.states.iter().any(|s| !valid_id(&s.id));
        if removed_symbol
            && self.state.sections.iter().any(|section| {
                c.removed_sections.binary_search(&section.pos).is_err()
                    && c.sections
                        .binary_search_by_key(&section.pos, |s| s.pos)
                        .is_err()
                    && !valid_section(section)
            })
        {
            return Err(invalid());
        }
        // Reserve before any semantic mutation; no subsequent step can return
        // an error. Existing dense buffers are moved or retained, never cloned.
        self.state
            .sections
            .try_reserve(c.sections.len())
            .map_err(|_| Fault::resource_limit("replica section capacity"))?;
        self.state
            .entities
            .try_reserve(c.entities.len())
            .map_err(|_| Fault::resource_limit("replica entity capacity"))?;
        self.state.states = c.states;
        if !c.sections.is_empty() || !c.removed_sections.is_empty() {
            self.state.sections.retain(|s| {
                c.removed_sections.binary_search(&s.pos).is_err()
                    && c.sections.binary_search_by_key(&s.pos, |n| n.pos).is_err()
            });
            self.state.sections.extend(c.sections);
            self.state.sections.sort_unstable_by_key(|s| s.pos);
        }
        if !c.entities.is_empty() || !c.removed_entities.is_empty() {
            self.state.entities.retain(|e| {
                c.removed_entities.binary_search(&e.id).is_err()
                    && c.entities.binary_search_by_key(&e.id, |n| n.id).is_err()
            });
            self.state.entities.extend(c.entities);
            self.state.entities.sort_unstable_by_key(|e| e.id);
        }
        self.seq = batch.seq;
        self.through = batch.through;
        Ok(c.presentation)
    }
}
