//! Explicit checkpoint archive migration. Active directories are never modified here.

use std::collections::BTreeMap;
use torse_codec::wire::Wire;
use torse_codec::{CodecLimits, wire};
use torse_content::CompiledContent;
use torse_sim::{
    SystemSpec,
    durable::{ExecutionArchive, ExecutionSnapshot},
};
use torse_types::{CommitCursor, Fault, OutputBudget, WorldId, WorldSnapshot};
use torse_world::{MigrationPlan, SnapshotArchive};

use crate::{
    HostAuthority, HostCheckpointStore, RecoveryTarget, Server,
    execution::{ExecutionHost, ExecutionModel, ExecutionRecovery},
};

/// A bounded full-world export that includes pages previously parked on disk.
/// Generator identity is preserved separately from content/schema migration semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResidencyArchive {
    pub execution: ExecutionArchive,
    pub generation: Option<crate::GenerationIdentity>,
}
impl Wire for ResidencyArchive {
    fn write(&self, e: &mut torse_codec::Encoder<'_>) -> Result<(), Fault> {
        1u16.write(e)?;
        self.execution.write(e)?;
        self.generation.write(e)
    }
    fn read(d: &mut torse_codec::Decoder<'_>) -> Result<Self, Fault> {
        if u16::read(d)? != 1 {
            return Err(Fault::incompatible("residency archive version"));
        }
        Ok(Self {
            execution: Wire::read(d)?,
            generation: Wire::read(d)?,
        })
    }
}

impl<J: torse_storage::CommitJournal<ExecutionModel>, S: HostCheckpointStore<ExecutionModel>>
    Server<ExecutionHost, J, S>
{
    /// Export only a healthy, fully durable tick boundary, without writing files.
    pub fn capture_archive(
        &mut self,
        content: &CompiledContent,
        budget: OutputBudget,
    ) -> Result<ExecutionArchive, Fault> {
        self.authority()?;
        if self.durability().through.0 != self.authority.cursor().seq.0 {
            return Err(Fault::invalid_data(
                "archive export requires a durable boundary",
            ));
        }
        self.authority.capture_archive(content, budget)
    }

    /// Export resident and parked data at one durable tick boundary. Aggregate budgets
    /// are checked before each merge; oversized worlds require a separate archive strategy.
    /// No page is loaded into the running authority and the source directory stays intact.
    pub fn capture_residency_archive(
        &mut self,
        content: &CompiledContent,
        pages: &crate::section_pages::SectionPages,
        budget: OutputBudget,
        limits: CodecLimits,
    ) -> Result<ResidencyArchive, Fault> {
        self.authority()?;
        if self.durability().through.0 != self.authority.cursor().seq.0 {
            return Err(Fault::invalid_data(
                "archive export requires a durable boundary",
            ));
        }
        let mut snapshot = self.authority.capture_snapshot(budget)?.state;
        let mut cost = wire::measure_inner(&snapshot, budget, limits)?;
        let mut states: BTreeMap<_, _> = std::mem::take(&mut snapshot.world.states)
            .into_iter()
            .map(|s| (s.id, s))
            .collect();
        let mut sections: BTreeMap<_, _> = std::mem::take(&mut snapshot.world.sections)
            .into_iter()
            .map(|s| (s.pos, s))
            .collect();
        let mut entities: BTreeMap<_, _> = std::mem::take(&mut snapshot.world.entities)
            .into_iter()
            .map(|e| (e.id, e))
            .collect();
        for (pos, reference) in &snapshot.parked {
            let raw = pages.get(reference.digest)?;
            let page = match crate::execution::streaming::bind_parked_page(&raw, reference)? {
                std::borrow::Cow::Borrowed(_) => raw,
                std::borrow::Cow::Owned(page) => page,
            };
            if page.sections[0].pos != *pos
                || page.entities.iter().map(|e| e.id).collect::<Vec<_>>() != reference.entities
                || page.meta.cursor.world != snapshot.world.meta.cursor.world
                || page.meta.compatibility != snapshot.world.meta.compatibility
                || page.meta.cursor.seq > snapshot.world.meta.cursor.seq
                || page.next_entity_id > snapshot.world.next_entity_id
            {
                return Err(Fault::invalid_data("parked archive page identity mismatch"));
            }
            let addition = wire::measure_inner(&page, budget, limits)?;
            cost.items = cost
                .items
                .checked_add(addition.items)
                .ok_or_else(|| Fault::resource_limit("archive item overflow"))?;
            cost.owned_bytes = cost
                .owned_bytes
                .checked_add(addition.owned_bytes)
                .ok_or_else(|| Fault::resource_limit("archive byte overflow"))?;
            cost.encoded_bytes = cost
                .encoded_bytes
                .checked_add(addition.encoded_bytes)
                .ok_or_else(|| Fault::resource_limit("archive byte overflow"))?;
            if !budget.allows(cost.items, cost.owned_bytes.max(cost.encoded_bytes)) {
                return Err(Fault::resource_limit(
                    "aggregate residency archive budget exceeded",
                ));
            }
            for state in page.states {
                if states.get(&state.id).is_some_and(|old| old != &state) {
                    return Err(Fault::invalid_data("archive state identity conflict"));
                }
                states.insert(state.id, state);
            }
            for section in page.sections {
                if sections.insert(section.pos, section).is_some() {
                    return Err(Fault::invalid_data("duplicate archive section"));
                }
            }
            for entity in page.entities {
                if entities.insert(entity.id, entity).is_some() {
                    return Err(Fault::invalid_data("duplicate archive entity"));
                }
            }
        }
        snapshot.world.states = states.into_values().collect();
        snapshot.world.sections = sections.into_values().collect();
        snapshot.world.entities = entities.into_values().collect();
        snapshot.parked.clear();
        let archive = ResidencyArchive {
            execution: capture(snapshot, content, budget, limits)?,
            generation: crate::GenerationIdentity::inspect(self.storage_root())?,
        };
        wire::measure_inner(&archive, budget, limits)?;
        Ok(archive)
    }
}

pub(crate) fn capture(
    snapshot: ExecutionSnapshot,
    source: &CompiledContent,
    budget: OutputBudget,
    limits: CodecLimits,
) -> Result<ExecutionArchive, Fault> {
    if !snapshot.parked.is_empty() {
        return Err(Fault::incompatible(
            "portable archive requires all parked sections to be loaded",
        ));
    }
    let archive = ExecutionArchive {
        world: SnapshotArchive::capture(&snapshot.world, source, budget)?,
        runtime: snapshot.runtime,
    };
    wire::measure_inner(&archive, budget, limits)?;
    Ok(archive)
}

/// Migrate a quiescent checkpoint into a separate candidate. Pending events or
/// frozen work may encode old schema semantics and cannot be silently carried
/// across a compatibility transition. They remain preserved in the source.
pub fn migrate(
    source: &ExecutionArchive,
    target: &CompiledContent,
    target_rules: &[SystemSpec],
    plan: &MigrationPlan,
    budget: OutputBudget,
    limits: CodecLimits,
) -> Result<ExecutionArchive, Fault> {
    wire::measure_inner(source, budget, limits)?;
    if source.runtime.rules.as_ref() != target_rules
        || (plan.source != plan.target
            && (!source.runtime.events.is_empty() || source.runtime.work.is_some()))
    {
        return Err(Fault::incompatible(
            "archive execution requires explicit rule/event migration or quiescence",
        ));
    }
    let candidate = ExecutionArchive {
        world: source.world.migrate(target, plan, budget)?,
        runtime: source.runtime.clone(),
    };
    wire::measure_inner(&candidate, budget, limits)?;
    Ok(candidate)
}

/// Prepare an explicit new-world import, preserving entity identity allocation
/// and tick while starting a new commit/session namespace. The original archive
/// retains its receipts and history. Never reuse the original world identity.
pub fn fork_for_import(
    source: &ExecutionArchive,
    new_world: WorldId,
    target: &CompiledContent,
    budget: OutputBudget,
    limits: CodecLimits,
) -> Result<ExecutionArchive, Fault> {
    wire::measure_inner(source, budget, limits)?;
    source.world.validate_for_restore(target, budget)?;
    if new_world == source.world.snapshot.meta.cursor.world
        || source.world.snapshot.meta.resume_at.next_phase != 0
        || source.world.snapshot.meta.resume_at.next_step != 0
        || !source.runtime.events.is_empty()
        || source.runtime.work.is_some()
    {
        return Err(Fault::incompatible(
            "archive import requires a new world identity and quiescent tick boundary",
        ));
    }
    let mut candidate = source.clone();
    candidate.world.snapshot.meta.cursor = CommitCursor::initial(new_world);
    candidate.runtime.windows.clear();
    wire::measure_inner(&candidate, budget, limits)?;
    Ok(candidate)
}

impl ExecutionRecovery<'_> {
    pub fn restore_residency_archive(
        &mut self,
        archive: &ResidencyArchive,
        generation: Option<&crate::GenerationIdentity>,
        target: &CompiledContent,
        budget: OutputBudget,
        limits: CodecLimits,
    ) -> Result<(), Fault> {
        wire::measure_inner(archive, budget, limits)?;
        if archive.generation.as_ref() != generation {
            return Err(Fault::incompatible(
                "residency archive generation identity mismatch",
            ));
        }
        self.restore_archive(&archive.execution, target, budget, limits)
    }
    /// Restore an available, fully migrated archive through the normal isolated
    /// recovery builder. No archive can bypass execution or domain validation.
    pub fn restore_archive(
        &mut self,
        archive: &ExecutionArchive,
        target: &CompiledContent,
        budget: OutputBudget,
        limits: CodecLimits,
    ) -> Result<(), Fault> {
        wire::measure_inner(archive, budget, limits)?;
        archive.world.validate_for_restore(target, budget)?;
        self.restore_snapshot(WorldSnapshot {
            meta: archive.world.snapshot.meta,
            state: ExecutionSnapshot {
                world: archive.world.snapshot.clone(),
                runtime: archive.runtime.clone(),
                parked: Default::default(),
            },
        })
    }
}
