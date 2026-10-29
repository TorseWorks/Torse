//! Explicit section residency changes. Preparation and publication perform no I/O.
use torse_types::{Fault, OutputBudget, SectionPos, SnapshotMeta};
use torse_world::{Entity, Section, SectionPhase, Snapshot};

use crate::{CommitFault, PreparedChange, WorldAuthority};

pub struct PreparedSection {
    pub(crate) barrier: PreparedChange,
    pub(crate) page: Snapshot,
    pub(crate) load: bool,
    pub(crate) section: Option<Section>,
    pub(crate) entities: Vec<Entity>,
    before: Option<Snapshot>,
    watermark: u128,
    budget: OutputBudget,
    revision: Option<torse_types::Revision>,
    retired: Option<torse_types::Revision>,
}

impl PreparedSection {
    pub fn meta(&self) -> torse_types::CommitMeta {
        self.barrier.meta()
    }
    pub fn position(&self) -> SectionPos {
        self.page.sections[0].pos
    }

    pub(crate) fn validate(&self, world: &WorldAuthority) -> Result<(), CommitFault> {
        let invalid = || CommitFault::Invariant("section preparation is stale");
        if world.entities().next_entity_id() != self.watermark {
            return Err(invalid());
        }
        let pos = self.position();
        if world.store().revision(pos) != self.revision
            || world.store().retired_revision(pos) != self.retired
        {
            return Err(invalid());
        }
        if self.load {
            if world.store().phase(pos).is_some()
                || self
                    .entities
                    .iter()
                    .any(|e| world.entities().handle_of(e.id).is_some())
            {
                return Err(invalid());
            }
        } else {
            if world.store().phase(pos) != Some(SectionPhase::Ready) {
                return Err(invalid());
            }
            let current = Snapshot::capture_section_with_budget(
                self.page.meta,
                world.store(),
                world.entities(),
                world.registry(),
                None,
                pos,
                self.budget,
            )
            .map_err(|_| invalid())?;
            if self.before.as_ref() != Some(&current) {
                return Err(invalid());
            }
        }
        Ok(())
    }
}

pub(crate) fn prepare(
    world: &WorldAuthority,
    page: &Snapshot,
    load: bool,
    generated: bool,
    budget: OutputBudget,
) -> Result<PreparedSection, Fault> {
    if page.sections.len() != 1
        || page.side != world.store().side()
        || page.bounds != world.store().bounds()
        || page.meta.cursor.world != world.world()
        || page.meta.compatibility != world.coordinator().compatibility()
        || page.meta.cursor.seq > world.seq()
        || (!generated && page.next_entity_id > world.entities().next_entity_id())
    {
        return Err(Fault::invalid_data(
            "section page does not match this world",
        ));
    }
    if generated {
        let first = world.entities().next_entity_id();
        let end = first
            .checked_add(page.entities.len() as u128)
            .ok_or_else(|| Fault::resource_limit("generated entity allocator exhausted"))?;
        if !load
            || page.meta.cursor.seq != world.seq()
            || page.next_entity_id != end
            || page
                .entities
                .iter()
                .enumerate()
                .any(|(index, entity)| entity.id.0 != first + index as u128)
        {
            return Err(Fault::invalid_data(
                "generated identities must extend the current allocator",
            ));
        }
    }
    // Budget and canonical structure are checked before cloning the page.
    let checked = torse_world::SnapshotArchive::import_legacy(
        page,
        torse_world::ArchiveManifest::default(),
        budget,
    )?;
    let pos = page.sections[0].pos;
    let persistent = page.persistent_view(world.schemas()).map_err(fault)?;
    if persistent.as_ref() != page {
        return Err(Fault::invalid_data("page contains transient components"));
    }
    let restored = checked
        .snapshot
        .restore(world.registry(), world.schemas())
        .map_err(fault)?;
    crate::block_instances::validate(&restored.store, &restored.entities, world.registry())
        .map_err(fault)?;
    let entities: Vec<_> = restored.entities.iter().map(|(_, e)| e.clone()).collect();
    if entities
        .iter()
        .any(|e| e.position.section(page.side) != Some(pos))
    {
        return Err(Fault::invalid_data("page entity lies outside its section"));
    }
    let before = if load {
        world
            .store()
            .validate_install_revision(pos, restored.store.section(pos).expect("restored page"))
            .map_err(fault)?;
        if world.store().phase(pos).is_some()
            || entities
                .iter()
                .any(|e| world.entities().handle_of(e.id).is_some())
        {
            return Err(Fault::invalid_data("section or entity already resident"));
        }
        None
    } else {
        if world.store().phase(pos) != Some(SectionPhase::Ready) {
            return Err(Fault::invalid_data("section is absent or held by tickets"));
        }
        let current = Snapshot::capture_section_with_budget(
            page.meta,
            world.store(),
            world.entities(),
            world.registry(),
            None,
            pos,
            budget,
        )?;
        if !same_page(
            current
                .persistent_view(world.schemas())
                .map_err(fault)?
                .as_ref(),
            page,
        ) {
            return Err(Fault::invalid_data(
                "unload page differs from resident data",
            ));
        }
        Some(current)
    };
    Ok(PreparedSection {
        barrier: world.prepare_barrier().map_err(fault)?,
        page: checked.snapshot,
        load,
        section: restored.store.section(pos).cloned(),
        entities,
        before,
        watermark: world.entities().next_entity_id(),
        budget,
        revision: world.store().revision(pos),
        retired: world.store().retired_revision(pos),
    })
}

// Legacy writers could leave an all-equal section in Dense form. Reclaiming its
// runtime buffer must not invalidate an old durable unload record.
fn same_page(a: &Snapshot, b: &Snapshot) -> bool {
    a.meta == b.meta
        && a.side == b.side
        && a.bounds == b.bounds
        && a.states == b.states
        && a.entities == b.entities
        && a.next_entity_id == b.next_entity_id
        && a.sections.len() == b.sections.len()
        && a.sections
            .iter()
            .zip(&b.sections)
            .all(|(a_section, b_section)| {
                use torse_world::SnapshotCells;
                a_section.pos == b_section.pos
                    && match (&a_section.cells, &b_section.cells) {
                        (SnapshotCells::Uniform(state), SnapshotCells::Dense(cells))
                        | (SnapshotCells::Dense(cells), SnapshotCells::Uniform(state)) => {
                            cells.len() == (a.side as usize).pow(3)
                                && cells.iter().all(|v| v == state)
                        }
                        (a, b) => a == b,
                    }
            })
}

pub(crate) fn fault(error: impl std::fmt::Display) -> Fault {
    Fault::invalid_data(error.to_string())
}

pub fn page_meta(world: &WorldAuthority) -> SnapshotMeta {
    SnapshotMeta {
        cursor: torse_types::CommitCursor {
            world: world.world(),
            seq: world.seq(),
        },
        compatibility: world.coordinator().compatibility(),
        resume_at: world.coordinator().resume_point(),
    }
}
