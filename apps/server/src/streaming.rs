//! Durable residency assembly for the execution host. File I/O precedes pure preparation.
use super::*;
use crate::{
    CommitStep, HostCheckpointStore, InvalidationSink, Server, section_pages::SectionPages,
};
use torse_storage::CommitJournal;
use torse_world::Snapshot;

const MAX_RESIDENT: usize = 4096;

impl ExecutionHost {
    pub fn parked_sections(&self) -> &BTreeMap<SectionPos, ParkedSection> {
        &self.parked
    }

    /// Resolve entity dependencies against live positions or parked identity indexes.
    /// This is derived host state and never invokes a rule while restoring a world.
    pub fn event_tickets(
        &self,
        due_only: bool,
        max_sections: usize,
    ) -> Result<EventTickets, Fault> {
        let mut tickets = EventTickets::default();
        for event in &self.runtime.events {
            if due_only && event.event.tick > self.resume_point().tick {
                continue;
            }
            match event.event.dependencies()? {
                EventDependencies::Global => tickets.global = true,
                EventDependencies::Scoped(dependencies) => {
                    for dependency in dependencies {
                        let section = match dependency {
                            EventDependency::Section(pos) => Some(pos),
                            EventDependency::Entity(id) => self
                                .world()
                                .entities()
                                .handle_of(id)
                                .and_then(|handle| self.world().entities().get(handle))
                                .and_then(|entity| {
                                    entity.position.section(self.world().store().side())
                                })
                                .or_else(|| {
                                    self.parked.iter().find_map(|(pos, page)| {
                                        page.entities.contains(&id).then_some(*pos)
                                    })
                                }),
                        };
                        if let Some(pos) = section {
                            if tickets.sections.len() >= max_sections
                                && !tickets.sections.contains(&pos)
                            {
                                return Err(Fault::resource_limit(
                                    "event residency capacity exceeded",
                                ));
                            }
                            tickets.sections.insert(pos);
                        }
                    }
                }
            }
        }
        Ok(tickets)
    }

    pub fn waiting_for_event_sections(&self) -> Result<bool, Fault> {
        Ok(self
            .event_tickets(true, MAX_RESIDENT)?
            .sections
            .iter()
            .any(|pos| self.world().store().section(*pos).is_none()))
    }

    pub fn section_is_event_pinned(&self, pos: SectionPos) -> Result<bool, Fault> {
        let tickets = self.event_tickets(false, MAX_RESIDENT)?;
        Ok(tickets.global || tickets.sections.contains(&pos))
    }

    /// Residency is changed only between ticks and after admitted work settles.
    pub fn residency_ready(&self, unload: bool) -> bool {
        self.at_tick_boundary()
            && (!unload || self.pending.is_empty())
            && self.frozen.is_none()
            && self.stopping.is_none()
            && self.section_pending.is_none()
            && (!unload
                || self
                    .runtime
                    .events
                    .iter()
                    .all(|e| matches!(e.event.dependencies(), Ok(EventDependencies::Scoped(_)))))
    }

    fn prepare_residency(&self, change: SectionChange) -> Result<ExecutionPrepared, Fault> {
        let _codec = self
            .memory_pool()
            .map(|pool| {
                pool.reserve(
                    MemoryClass::Codec,
                    self.limits.runtime.max_bytes.saturating_mul(4),
                    EvictionRank::Pinned,
                )
            })
            .transpose()?;
        if !self.at_tick_boundary()
            || (!change.load && !self.pending.is_empty())
            || self.frozen.is_some()
            || self.stopping.is_some()
            || self.section_pending.is_some()
        {
            return Err(Fault::invalid_data(
                "section transition requires an idle tick boundary",
            ));
        }
        check_reference(&change, &self.parked)?;
        if !change.load
            && self.runtime.events.iter().any(|event| {
                event
                    .event
                    .pins_section(change.page.sections[0].pos, |id| {
                        change.reference.entities.contains(&id)
                    })
                    .unwrap_or(true)
            })
        {
            return Err(Fault::invalid_data("pending event pins this section"));
        }
        if change.load && self.world().store().len() >= MAX_RESIDENT {
            return Err(Fault::resource_limit("resident section capacity"));
        }
        if !change.load {
            let mut candidate = self.parked.clone();
            candidate.insert(change.page.sections[0].pos, change.reference.clone());
            wire::measure_inner(&candidate, self.limits.runtime, self.limits.codec)?;
        }
        let world = if change.fresh {
            self.world()
                .prepare_generated_section(&change.page, self.limits.runtime)?
        } else {
            self.world()
                .prepare_section(&change.page, change.load, self.limits.runtime)?
        };
        let meta = world.meta();
        let changes = torse_world::ChangeSet::from_edits(meta, self.world().registry(), &[])
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        let mut prepared = ExecutionPrepared {
            memory: None,
            world: PreparedWorld {
                normal: None,
                section: Some(Box::new(world)),
            },
            runtime: self.runtime.clone(),
            remove_pending: Vec::new(),
            record: CommitRecord {
                meta,
                changes: ExecutionChanges {
                    world: changes,
                    runtime: RuntimeUpdate::Full(self.runtime.clone()),
                    outcome: None,
                    cancelled: None,
                    section: Some(change),
                },
            },
        };
        wire::measure_inner(&prepared.record, self.limits.runtime, self.limits.codec)?;
        self.account_residency(&mut prepared)?;
        Ok(prepared)
    }
}

fn reference(page: &Snapshot, digest: Digest) -> ParkedSection {
    ParkedSection {
        digest,
        entities: page.entities.iter().map(|e| e.id).collect(),
    }
}
fn check_reference(
    change: &SectionChange,
    parked: &BTreeMap<SectionPos, ParkedSection>,
) -> Result<(), Fault> {
    if change.page.sections.len() != 1
        || change.reference != reference(&change.page, change.reference.digest)
    {
        return Err(Fault::invalid_data("section reference differs from page"));
    }
    let pos = change.page.sections[0].pos;
    let valid = if change.fresh {
        change.load && !parked.contains_key(&pos)
    } else if change.load {
        parked.get(&pos) == Some(&change.reference)
    } else {
        !parked.contains_key(&pos)
    };
    if !valid {
        return Err(Fault::invalid_data("invalid section residency history"));
    }
    Ok(())
}

impl<J: CommitJournal<ExecutionModel>, S: HostCheckpointStore<ExecutionModel>>
    Server<ExecutionHost, J, S>
{
    pub(crate) fn apply_residency_page(
        &mut self,
        page: Snapshot,
        digest: Digest,
        load: bool,
        fresh: bool,
        sink: &mut impl InvalidationSink,
    ) -> Result<CommitStep, Fault> {
        self.authority()?;
        let change = SectionChange {
            reference: reference(&page, digest),
            page,
            load,
            fresh,
        };
        self.authority.section_pending = Some(self.authority.prepare_residency(change)?);
        self.drive_one(sink)
    }
    pub fn unload_section(
        &mut self,
        pages: &SectionPages,
        pos: SectionPos,
        sink: &mut impl InvalidationSink,
    ) -> Result<CommitStep, Fault> {
        if self
            .residency
            .as_ref()
            .is_some_and(|scheduler| !scheduler.is_idle())
        {
            return Err(Fault::invalid_data("page I/O is in flight"));
        }
        let host = self.authority()?;
        let page = host.world().capture_section(pos, host.limits.runtime)?;
        let digest = pages.digest(&page)?;
        let change = SectionChange {
            reference: reference(&page, digest),
            page,
            load: false,
            fresh: false,
        };
        let prepared = self.authority.prepare_residency(change)?;
        pages.put(
            &prepared
                .record
                .changes
                .section
                .as_ref()
                .expect("section record")
                .page,
        )?;
        self.authority.section_pending = Some(prepared);
        self.drive_one(sink)
    }
    pub fn reload_section(
        &mut self,
        pages: &SectionPages,
        pos: SectionPos,
        sink: &mut impl InvalidationSink,
    ) -> Result<CommitStep, Fault> {
        let old = self
            .authority()?
            .parked
            .get(&pos)
            .ok_or_else(|| Fault::invalid_data("section has no parked page"))?
            .clone();
        let page = pages.get(old.digest)?;
        let change = SectionChange {
            page,
            load: true,
            fresh: false,
            reference: old,
        };
        self.authority.section_pending = Some(self.authority.prepare_residency(change)?);
        self.drive_one(sink)
    }
    /// Install trusted generated terrain and freshly allocated attached state atomically.
    pub fn install_section(
        &mut self,
        pages: &SectionPages,
        page: Snapshot,
        sink: &mut impl InvalidationSink,
    ) -> Result<CommitStep, Fault> {
        self.authority()?;
        let digest = pages.digest(&page)?;
        let change = SectionChange {
            reference: reference(&page, digest),
            page,
            load: true,
            fresh: true,
        };
        self.authority.section_pending = Some(self.authority.prepare_residency(change)?);
        self.drive_one(sink)
    }
}

impl ExecutionRecovery<'_> {
    pub fn with_section_pages(mut self, pages: SectionPages) -> Self {
        self.section_pages = Some(pages);
        self
    }

    pub(super) fn replay_section(
        &mut self,
        record: CommitRecord<ExecutionChanges>,
    ) -> Result<(), Fault> {
        let change = record.changes.section.as_ref().expect("section replay");
        let before = self
            .runtime
            .as_ref()
            .ok_or_else(|| Fault::invalid_data("missing execution snapshot"))?;
        let point = self
            .point
            .ok_or_else(|| Fault::invalid_data("missing execution point"))?;
        if record.meta != record.changes.world.meta
            || record.meta.compatibility != self.binding.compatibility
            || record.meta.resume_at != point
            || point.next_phase != 0
            || point.next_step != 0
            || before.work.is_some()
            || record.changes.outcome.is_some()
            || record.changes.cancelled.is_some()
            || !matches!(&record.changes.runtime, RuntimeUpdate::Full(state) if state == before)
        {
            return Err(Fault::invalid_data(
                "section record changes execution semantics",
            ));
        }
        check_reference(change, &self.parked)?;
        if !change.load {
            for event in &before.events {
                if event
                    .event
                    .pins_section(change.page.sections[0].pos, |id| {
                        change.reference.entities.contains(&id)
                    })?
                {
                    return Err(Fault::invalid_data(
                        "section record unloads an event dependency",
                    ));
                }
            }
        }
        if !change.load {
            let mut candidate = self.parked.clone();
            candidate.insert(change.page.sections[0].pos, change.reference.clone());
            wire::measure_inner(&candidate, self.limits.runtime, self.limits.codec)?;
        }
        let pages = self
            .section_pages
            .as_ref()
            .ok_or_else(|| Fault::invalid_data("section recovery requires page store"))?;
        if pages.digest(&change.page)? != change.reference.digest {
            return Err(Fault::invalid_data("journal page digest mismatch"));
        }
        if change.fresh {
            self.recovery.replay_generated_section(
                &record.changes.world,
                &change.page,
                self.limits.runtime,
            )?;
        } else {
            self.recovery.replay_section(
                &record.changes.world,
                &change.page,
                change.load,
                self.limits.runtime,
            )?;
        }
        let pos = change.page.sections[0].pos;
        if change.load {
            self.parked.remove(&pos);
        } else {
            self.parked.insert(pos, change.reference.clone());
        }
        Ok(())
    }

    pub(super) fn validate_parked(&self) -> Result<(), Fault> {
        if self.parked.is_empty() {
            return Ok(());
        }
        // Persistent indexes are bounded by encoded bytes and items, rather
        // than imposing a fixed number of explored sections on every host.
        wire::measure_inner(&self.parked, self.limits.runtime, self.limits.codec)?;
        let pages = self
            .section_pages
            .as_ref()
            .ok_or_else(|| Fault::invalid_data("parked pages require a section store"))?;
        let mut ids = std::collections::BTreeSet::new();
        for (pos, expected) in &self.parked {
            let page = pages.get(expected.digest)?;
            if page.sections[0].pos != *pos
                || reference(&page, expected.digest) != *expected
                || expected.entities.iter().any(|id| !ids.insert(*id))
            {
                return Err(Fault::invalid_data("invalid parked page index"));
            }
            self.recovery.parked_page_valid(&page)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parked_index_accepts_more_than_4096_sections_within_its_budget() {
        let (world, _) = crate::bootstrap::world().unwrap();
        let page = world
            .capture_section(SectionPos::ZERO, crate::bootstrap::budget())
            .unwrap();
        let digest = Digest([0; 32]);
        let parked: BTreeMap<_, _> = (1..=4097)
            .map(|x| {
                (
                    SectionPos::new(x, 0, 0),
                    ParkedSection {
                        digest,
                        entities: Vec::new(),
                    },
                )
            })
            .collect();
        let change = SectionChange {
            reference: reference(&page, digest),
            page,
            load: false,
            fresh: false,
        };
        check_reference(&change, &parked).unwrap();
        wire::measure_inner(
            &parked,
            crate::bootstrap::budget(),
            crate::bootstrap::codec_limits(),
        )
        .unwrap();
        let tiny = OutputBudget {
            max_items: 64,
            max_bytes: 1024,
        };
        assert!(wire::measure_inner(&parked, tiny, crate::bootstrap::codec_limits()).is_err());
    }
}
