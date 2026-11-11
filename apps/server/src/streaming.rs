//! Durable residency assembly for the execution host. File I/O precedes pure preparation.
use super::*;
use crate::{
    CommitStep, HostCheckpointStore, InvalidationSink, Server, section_pages::SectionPages,
};
use torse_storage::CommitJournal;
use torse_world::Snapshot;

/// Last atomic installation attempt, including preparation and durable wait time.
#[derive(Clone, Debug, Default)]
pub struct SectionInstallDiagnostics {
    pub sections: usize,
    pub elapsed: std::time::Duration,
    pub page_preparation: std::time::Duration,
    pub validation: std::time::Duration,
    /// Journal staging/encoding, publication and invalidation, excluding flush wait.
    pub commit: std::time::Duration,
    pub durability_wait: std::time::Duration,
    pub failure: Option<String>,
}

const MAX_RESIDENT: usize = 4096;

// The record appends exactly the already measured Vec<SectionChange>. Its
// container prefix is present in both measurements; subtract it once. No
// structure or value depth is added by the surrounding wire field structs.
fn section_record_cost(
    record: &mut CommitRecord<ExecutionChanges>,
    sections_cost: wire::WireCost,
    budget: OutputBudget,
    codec: torse_codec::CodecLimits,
) -> Result<wire::WireCost, Fault> {
    let sections = std::mem::take(&mut record.changes.sections);
    let envelope = wire::measure_inner(record, budget, codec);
    record.changes.sections = sections;
    let envelope = envelope?;
    let prefix = wire::measure_inner(&Vec::<SectionChange>::new(), budget, codec)?;
    let overflow = || Fault::resource_limit("section record cost overflow");
    let cost = wire::WireCost {
        encoded_bytes: envelope
            .encoded_bytes
            .checked_sub(prefix.encoded_bytes)
            .and_then(|n| n.checked_add(sections_cost.encoded_bytes))
            .ok_or_else(overflow)?,
        owned_bytes: envelope
            .owned_bytes
            .checked_sub(prefix.owned_bytes)
            .and_then(|n| n.checked_add(sections_cost.owned_bytes))
            .ok_or_else(overflow)?,
        items: envelope
            .items
            .checked_sub(prefix.items)
            .and_then(|n| n.checked_add(sections_cost.items))
            .ok_or_else(overflow)?,
    };
    if cost.encoded_bytes > budget.max_bytes.min(u64::from(codec.max_frame_bytes))
        || cost.owned_bytes > budget.max_bytes
        || cost.items > budget.max_items
    {
        return Err(Fault::resource_limit(
            "section record exceeds aggregate budget",
        ));
    }
    Ok(cost)
}

#[cfg(test)]
mod cost_tests {
    use super::*;
    use crate::bootstrap as base;
    use torse_world::{BlockStateId, Position, SnapshotCells};
    #[test]
    fn composed_section_record_cost_matches_full_wire_measurement_and_denials() {
        for dense in [false, true] {
            for entities in [false, true] {
                for count in [1, 4] {
                    let (world, actions) = base::world().unwrap();
                    let template = world
                        .capture_section(SectionPos::ZERO, base::budget())
                        .unwrap();
                    let binding = Binding {
                        world: base::WORLD,
                        epoch: WorldEpoch(1),
                        compatibility: world.coordinator().compatibility(),
                    };
                    let first = world.entities().next_entity_id();
                    let mut changes = Vec::new();
                    for index in 0..count {
                        let mut page = template.clone();
                        let pos = SectionPos::new(2 + index as i64, 0, 0);
                        page.sections[0].pos = pos;
                        if dense {
                            let stone = world
                                .registry()
                                .default_state(&base::key("base:stone"))
                                .unwrap();
                            page.states = vec![torse_world::StateEntry {
                                id: stone,
                                block: base::key("base:stone"),
                                values: vec![],
                            }];
                            page.sections[0].cells = SnapshotCells::Dense(
                                (0..4096)
                                    .map(|i| {
                                        if i % 3 == 0 {
                                            stone
                                        } else {
                                            BlockStateId::EMPTY
                                        }
                                    })
                                    .collect(),
                            );
                        }
                        page.entities.truncate(usize::from(entities));
                        let next = first + if entities { index as u128 } else { 0 };
                        if entities {
                            page.entities[0].id = EntityId(next);
                            page.entities[0].position_bits =
                                Position::new(pos.x as f64 * 16. + 1.5, 1.5, 1.5).to_bits();
                        }
                        page.next_entity_id = next + u128::from(entities);
                        changes.push(SectionChange {
                            reference: reference(&page, Digest([0; 32])),
                            page,
                            load: true,
                            fresh: true,
                        });
                    }
                    let host = ExecutionHost::new(
                        binding,
                        world,
                        actions,
                        vec![],
                        base::execution_limits(),
                    )
                    .unwrap();
                    let mut prepared = host.prepare_section_batch(changes).unwrap();
                    let sections = wire::measure_inner(
                        &prepared.record.changes.sections,
                        base::budget(),
                        base::codec_limits(),
                    )
                    .unwrap();
                    let expected =
                        wire::measure_inner(&prepared.record, base::budget(), base::codec_limits())
                            .unwrap();
                    assert_eq!(
                        section_record_cost(
                            &mut prepared.record,
                            sections,
                            base::budget(),
                            base::codec_limits()
                        )
                        .unwrap(),
                        expected
                    );
                    let tiny = OutputBudget {
                        max_items: expected.items - 1,
                        ..base::budget()
                    };
                    assert!(
                        wire::measure_inner(&prepared.record, tiny, base::codec_limits()).is_err()
                    );
                    assert!(
                        section_record_cost(
                            &mut prepared.record,
                            sections,
                            tiny,
                            base::codec_limits()
                        )
                        .is_err()
                    );
                    let tiny = OutputBudget {
                        max_bytes: expected.encoded_bytes.max(expected.owned_bytes) - 1,
                        ..base::budget()
                    };
                    assert!(
                        wire::measure_inner(&prepared.record, tiny, base::codec_limits()).is_err()
                    );
                    assert!(
                        section_record_cost(
                            &mut prepared.record,
                            sections,
                            tiny,
                            base::codec_limits()
                        )
                        .is_err()
                    );
                    let codec = torse_codec::CodecLimits {
                        max_frame_bytes: expected.encoded_bytes as u32 - 1,
                        ..base::codec_limits()
                    };
                    assert!(wire::measure_inner(&prepared.record, base::budget(), codec).is_err());
                    assert!(
                        section_record_cost(&mut prepared.record, sections, base::budget(), codec)
                            .is_err()
                    );
                    assert_eq!(prepared.record.changes.sections.len(), count);
                    assert_eq!(
                        wire::measure_inner(&prepared.record, base::budget(), base::codec_limits())
                            .unwrap(),
                        expected
                    );
                }
            }
        }
    }
}

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

    pub(crate) fn prepare_residency(
        &self,
        change: SectionChange,
    ) -> Result<ExecutionPrepared, Fault> {
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
        self.check_residency(&change)?;
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
                sections: None,
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
                    sections: Vec::new(),
                },
            },
        };
        wire::measure_inner(&prepared.record, self.limits.runtime, self.limits.codec)?;
        self.account_residency(&mut prepared)?;
        Ok(prepared)
    }
    fn check_residency(&self, change: &SectionChange) -> Result<(), Fault> {
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
        check_reference(change, &self.parked)?;
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
        Ok(())
    }

    fn prepare_section_batch(
        &self,
        changes: Vec<SectionChange>,
    ) -> Result<ExecutionPrepared, Fault> {
        if changes.is_empty() || changes.len() > self.section_install_limit {
            return Err(Fault::resource_limit(
                "generated batch exceeds section limit",
            ));
        }
        if self.world().store().len().saturating_add(changes.len()) > MAX_RESIDENT {
            return Err(Fault::resource_limit("resident section capacity"));
        }
        for change in &changes {
            self.check_residency(change)?;
        }
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
        // Bound the full input before the world decodes or clones individual pages.
        let sections_cost = wire::measure_inner(&changes, self.limits.runtime, self.limits.codec)?;
        let world = self.world().prepare_generated_sections_iter(
            changes.iter().map(|c| &c.page),
            self.limits.runtime,
        )?;
        let meta = world.meta();
        let empty = torse_world::ChangeSet::from_edits(meta, self.world().registry(), &[])
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        let mut prepared = ExecutionPrepared {
            memory: None,
            world: PreparedWorld {
                normal: None,
                section: None,
                sections: Some(Box::new(world)),
            },
            runtime: self.runtime.clone(),
            remove_pending: Vec::new(),
            record: CommitRecord {
                meta,
                changes: ExecutionChanges {
                    world: empty,
                    runtime: RuntimeUpdate::Full(self.runtime.clone()),
                    outcome: None,
                    cancelled: None,
                    section: None,
                    sections: changes,
                },
            },
        };
        section_record_cost(
            &mut prepared.record,
            sections_cost,
            self.limits.runtime,
            self.limits.codec,
        )?;
        self.account_residency(&mut prepared)?;
        Ok(prepared)
    }
}

pub(crate) fn reference(page: &Snapshot, digest: Digest) -> ParkedSection {
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
    /// Hold the mutable host borrow until every published page is durable. Invalid
    /// input ends this batch after confirming its earlier prefix; storage errors
    /// fail the host and require recovery, just like the single-record barrier.
    pub(crate) fn apply_residency_batch(
        &mut self,
        pages: &SectionPages,
        completed: Vec<crate::residency::Completed>,
        sink: &mut impl InvalidationSink,
    ) -> Result<
        (
            crate::residency::ResidencyBatchReport,
            Vec<crate::residency::Completed>,
        ),
        Fault,
    > {
        use crate::coordinator::PublishedStep;
        use crate::residency::ResidencyBatchReport;
        self.authority()?;
        let start = self.authority.cursor();
        let allocator = self.authority.world().entities().next_entity_id();
        let mut report = ResidencyBatchReport {
            committed: Vec::new(),
            durability: self.journal.durability(),
            blocked: None,
            failure: None,
        };
        let mut pending = completed.into_iter();
        let mut retry = Vec::new();
        while let Some(mut completed) = pending.next() {
            let pos = completed.page.sections[0].pos;
            if completed.fresh {
                if completed.page.meta.cursor != start
                    || completed.page.next_entity_id != allocator
                    || !completed.page.entities.is_empty()
                {
                    report.failure = Some((pos, Fault::invalid_data("stale generated batch page")));
                    break;
                }
                // The worker generated terrain-only pages at the same starting
                // cursor. Rebase only across this call's own accepted prefix;
                // external changes above still force fresh generation.
                completed.page.meta = torse_sim::streaming::page_meta(self.authority.world());
                match pages.digest(&completed.page) {
                    Ok(digest) => completed.digest = digest,
                    Err(error) => {
                        report.failure = Some((pos, error));
                        break;
                    }
                }
            }
            let change = SectionChange {
                reference: reference(&completed.page, completed.digest),
                page: completed.page,
                load: completed.load,
                fresh: completed.fresh,
            };
            match self.authority.prepare_residency(change) {
                Ok(prepared) => self.authority.section_pending = Some(prepared),
                Err(error) => {
                    report.failure = Some((pos, error));
                    break;
                }
            }
            match self.publish_one(sink, None)? {
                PublishedStep::Published(_) => report.committed.push(pos),
                PublishedStep::Blocked(error, prepared) => {
                    let change = prepared.record.changes.section.expect("residency record");
                    retry.push(crate::residency::Completed {
                        page: change.page,
                        digest: change.reference.digest,
                        load: change.load,
                        fresh: change.fresh,
                    });
                    retry.extend(pending);
                    // Retain decoded pages under the existing I/O reservation.
                    // Only this call's own prefix may move their generation cut.
                    for page in &mut retry {
                        if page.fresh && page.page.meta.cursor == start {
                            page.page.meta =
                                torse_sim::streaming::page_meta(self.authority.world());
                        }
                    }
                    report.blocked = Some(error);
                    break;
                }
                PublishedStep::Idle => {
                    return Err(self.fail(Fault::invalid_data("residency preparation disappeared")));
                }
            }
        }
        if !report.committed.is_empty() {
            report.durability = self.flush_to(self.authority.cursor())?;
        }
        Ok((report, retry))
    }

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
    /// Set the atomic installation bound without changing storage or output budgets.
    pub fn set_section_install_limit(&mut self, max_sections: usize) -> Result<(), Fault> {
        self.authority()?;
        if !(1..=1024).contains(&max_sections) {
            return Err(Fault::invalid_data(
                "section install limit must be 1..=1024",
            ));
        }
        self.authority.section_install_limit = max_sections;
        Ok(())
    }
    pub fn section_install_diagnostics(&self) -> &SectionInstallDiagnostics {
        &self.authority.section_install_diagnostics
    }
    /// Install an ordered generated batch as one record, publication and durable barrier.
    /// Pages use the current generation cut and consecutive authority-allocated IDs.
    pub fn install_section_batch(
        &mut self,
        pages: &SectionPages,
        batch: Vec<Snapshot>,
        sink: &mut impl InvalidationSink,
    ) -> Result<CommitStep, Fault> {
        let start = std::time::Instant::now();
        let count = batch.len();
        let mut diagnostics = SectionInstallDiagnostics {
            sections: count,
            ..Default::default()
        };
        let result = (|| {
            self.authority()?;
            if count == 0 || count > self.authority.section_install_limit {
                return Err(Fault::resource_limit(
                    "generated batch exceeds section limit",
                ));
            }
            let stage = std::time::Instant::now();
            let changes = (|| {
                let mut changes = Vec::with_capacity(count);
                for page in batch {
                    let digest = pages.digest(&page)?;
                    changes.push(SectionChange {
                        reference: reference(&page, digest),
                        page,
                        load: true,
                        fresh: true,
                    });
                }
                Ok::<_, Fault>(changes)
            })();
            diagnostics.page_preparation = stage.elapsed();
            let changes = changes?;
            let stage = std::time::Instant::now();
            let prepared = self.authority.prepare_section_batch(changes);
            diagnostics.validation = stage.elapsed();
            self.authority.section_pending = Some(prepared?);
            let before = self.flush_metrics().total_wait;
            let stage = std::time::Instant::now();
            let result = self.drive_one(sink);
            diagnostics.durability_wait = self.flush_metrics().total_wait.saturating_sub(before);
            diagnostics.commit = stage.elapsed().saturating_sub(diagnostics.durability_wait);
            result
        })();
        let failure = match &result {
            Err(error) => Some(error.to_string()),
            Ok(CommitStep::Blocked(error)) => Some(format!("{error:?}")),
            _ => None,
        };
        diagnostics.elapsed = start.elapsed();
        diagnostics.failure = failure;
        self.authority.section_install_diagnostics = diagnostics;
        result
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

    pub(super) fn replay_section_batch(
        &mut self,
        record: CommitRecord<ExecutionChanges>,
    ) -> Result<(), Fault> {
        let changes = &record.changes;
        let before = self
            .runtime
            .as_ref()
            .ok_or_else(|| Fault::invalid_data("missing runtime"))?;
        let point = self
            .point
            .ok_or_else(|| Fault::invalid_data("missing resume point"))?;
        if changes.section.is_some()
            || changes.sections.len() > 1024
            || changes.sections.is_empty()
            || record.meta != changes.world.meta
            || record.meta.compatibility != self.binding.compatibility
            || record.meta.resume_at != point
            || point.next_phase != 0
            || point.next_step != 0
            || before.work.is_some()
            || changes.outcome.is_some()
            || changes.cancelled.is_some()
            || !matches!(&changes.runtime, RuntimeUpdate::Full(state) if state == before)
        {
            return Err(Fault::invalid_data(
                "invalid generated batch execution semantics",
            ));
        }
        wire::measure_inner(&record, self.limits.runtime, self.limits.codec)?;
        let store = self
            .section_pages
            .as_ref()
            .ok_or_else(|| Fault::invalid_data("missing section pages"))?;
        for change in &changes.sections {
            check_reference(change, &self.parked)?;
            if !change.fresh
                || !change.load
                || store.digest(&change.page)? != change.reference.digest
            {
                return Err(Fault::invalid_data("invalid generated batch page"));
            }
        }
        let pages: Vec<_> = changes.sections.iter().map(|c| c.page.clone()).collect();
        self.recovery
            .replay_generated_sections(&changes.world, &pages, self.limits.runtime)
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
