//! Bounded trusted imports. Provisional identities are local to each page.
use super::*;
use crate::{coordinator::PublishedStep, section_pages::SectionPages, *};
use std::collections::{BTreeSet, VecDeque};
use torse_storage::CommitJournal;
use torse_world::{Snapshot, SnapshotValue};

#[derive(Clone, Copy, Debug)]
pub struct SectionInstallLimits {
    pub max_pages: usize,
    /// Logical retained-plus-working reservation, not measured process RSS.
    pub max_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct InstalledSection {
    pub position: SectionPos,
    pub at: CommitCursor,
    pub entities: BTreeMap<EntityId, EntityId>,
}

pub struct SectionInstallProgress {
    /// Accepted during this call; covered by durability on successful return.
    pub installed: Vec<InstalledSection>,
    pub durability: Durability,
    pub remaining: usize,
    pub blocked: Option<StageError>,
    pub failure: Option<(SectionPos, Fault)>,
}

pub struct TrustedSectionInstalls {
    binding: Binding,
    expected: CommitCursor,
    pages: SectionPages,
    pending: VecDeque<Snapshot>,
    accepted: Vec<InstalledSection>,
    durability: Durability,
    _memory: Option<MemoryLease>,
}
impl TrustedSectionInstalls {
    pub fn remaining(&self) -> usize {
        self.pending.len()
    }
    /// On terminal storage failure these are accepted, not necessarily durable.
    /// Recover the directory to discover the actual valid prefix; never replay
    /// this handle into another server or assume an unconfirmed suffix rolled back.
    pub fn accepted(&self) -> &[InstalledSection] {
        &self.accepted
    }
    pub fn durability(&self) -> Durability {
        self.durability
    }
    pub fn into_remaining(self) -> Vec<Snapshot> {
        self.pending.into()
    }
}

fn remap(value: &mut SnapshotValue, ids: &BTreeMap<EntityId, EntityId>) -> Result<(), Fault> {
    remap_at(value, ids, 0)
}

fn remap_at(
    value: &mut SnapshotValue,
    ids: &BTreeMap<EntityId, EntityId>,
    depth: u32,
) -> Result<(), Fault> {
    if depth > torse_world::MAX_VALUE_DEPTH {
        return Err(Fault::resource_limit("trusted page reference nesting"));
    }
    match value {
        SnapshotValue::Entity(id) => {
            *id = *ids
                .get(id)
                .ok_or_else(|| Fault::invalid_data("trusted page reference is not page-local"))?;
        }
        SnapshotValue::List(values) => {
            for value in values {
                remap_at(value, ids, depth + 1)?;
            }
        }
        SnapshotValue::Record(fields) => {
            for (_, value) in fields {
                remap_at(value, ids, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

impl<J: CommitJournal<ExecutionModel>, S: HostCheckpointStore<ExecutionModel>>
    Server<ExecutionHost, J, S>
{
    /// Input IDs are nonzero, ordered page-local symbols. All entity references
    /// must name a symbol in the same page; cross-page/live references are rejected.
    /// Inputs must share the current generation cut. The returned handle owns
    /// pending pages and a bounded logical reservation until handle drop.
    pub fn begin_section_installs(
        &self,
        pages: SectionPages,
        input: Vec<Snapshot>,
        limits: SectionInstallLimits,
    ) -> Result<TrustedSectionInstalls, Fault> {
        let host = self.authority()?;
        if !(1..=1024).contains(&limits.max_pages)
            || limits.max_bytes < 4
            || input.len() > limits.max_pages
        {
            return Err(Fault::resource_limit("trusted install batch limits"));
        }
        if !host.residency_ready(false) || self.residency.as_ref().is_some_and(|s| !s.is_idle()) {
            return Err(Fault::invalid_data(
                "trusted install requires idle residency",
            ));
        }
        let memory = host
            .memory_pool()
            .map(|pool| pool.reserve(MemoryClass::Codec, limits.max_bytes, EvictionRank::Pinned))
            .transpose()?;
        let meta = torse_sim::streaming::page_meta(host.world());
        let mut positions = BTreeSet::new();
        let mut charge = 0u64;
        for page in &input {
            // Decode/shape budgets are enforced before cloning/remapping and
            // recursive reference traversal. Scratch fits the same reservation.
            let cost = pages.measure(page, limits.max_bytes / 4)?;
            let components = page
                .entities
                .iter()
                .flat_map(|e| &e.components)
                .fold(0u64, |n, c| n.saturating_add(c.accounted_bytes()));
            charge = charge.saturating_add(
                cost.owned_bytes
                    .saturating_add(cost.encoded_bytes)
                    .saturating_add(components)
                    .saturating_add(4096)
                    .saturating_mul(4),
            );
            if charge > limits.max_bytes {
                return Err(Fault::resource_limit("trusted install byte budget"));
            }
            if page.sections.len() != 1
                || page.meta != meta
                || !positions.insert(page.sections[0].pos)
                || page.entities.iter().any(|e| e.id.0 == 0)
                || page.entities.windows(2).any(|e| e[0].id >= e[1].id)
            {
                return Err(Fault::invalid_data(
                    "trusted install input shape or generation cut",
                ));
            }
        }
        Ok(TrustedSectionInstalls {
            binding: self.binding(),
            expected: host.cursor(),
            pages,
            pending: input.into(),
            accepted: Vec::new(),
            durability: self.durability(),
            _memory: memory,
        })
    }

    /// Commit a bounded prefix and share its durable barrier. Backpressure keeps
    /// original provisional pages for retry; external authority advancement
    /// requires the caller to abandon/rebuild the job explicitly.
    pub fn install_sections(
        &mut self,
        batch: &mut TrustedSectionInstalls,
        sink: &mut impl InvalidationSink,
    ) -> Result<SectionInstallProgress, Fault> {
        let host = self.authority()?;
        if batch.binding != self.binding() || batch.expected != host.cursor() {
            return Err(Fault::new(
                FaultCode::ResetRequired,
                "trusted install batch became stale",
            ));
        }
        if !host.residency_ready(false) || self.residency.as_ref().is_some_and(|s| !s.is_idle()) {
            return Err(Fault::invalid_data(
                "trusted install requires idle residency",
            ));
        }
        let mut report = SectionInstallProgress {
            installed: Vec::new(),
            durability: self.durability(),
            remaining: batch.remaining(),
            blocked: None,
            failure: None,
        };
        while let Some(original) = batch.pending.front() {
            let pos = original.sections[0].pos;
            let prepared = (|| {
                let first = self.authority.world().entities().next_entity_id();
                let end = first
                    .checked_add(original.entities.len() as u128)
                    .ok_or_else(|| Fault::resource_limit("entity allocator exhausted"))?;
                let ids: BTreeMap<_, _> = original
                    .entities
                    .iter()
                    .enumerate()
                    .map(|(i, e)| (e.id, EntityId(first + i as u128)))
                    .collect();
                let mut page = original.clone();
                page.meta = torse_sim::streaming::page_meta(self.authority.world());
                page.next_entity_id = end;
                for entity in &mut page.entities {
                    entity.id = ids[&entity.id];
                    for component in &mut entity.components {
                        for (_, value) in &mut component.fields {
                            remap(value, &ids)?;
                        }
                    }
                }
                let digest = batch.pages.digest(&page)?;
                let change = SectionChange {
                    reference: super::streaming::reference(&page, digest),
                    page,
                    load: true,
                    fresh: true,
                };
                Ok::<_, Fault>((self.authority.prepare_residency(change)?, ids))
            })();
            let (prepared, ids) = match prepared {
                Ok(value) => value,
                Err(error) => {
                    report.failure = Some((pos, error));
                    break;
                }
            };
            self.authority.section_pending = Some(prepared);
            match self.publish_one(sink, None)? {
                PublishedStep::Published(publication) => {
                    let installed = InstalledSection {
                        position: pos,
                        at: publication.at,
                        entities: ids,
                    };
                    batch.accepted.push(installed.clone());
                    report.installed.push(installed);
                    batch.pending.pop_front();
                    batch.expected = self.authority.cursor();
                }
                PublishedStep::Blocked(error, _) => {
                    report.blocked = Some(error);
                    break;
                }
                PublishedStep::Idle => {
                    return Err(self.fail(Fault::invalid_data(
                        "trusted install preparation disappeared",
                    )));
                }
            }
        }
        if !report.installed.is_empty() {
            report.durability = self.flush_to(self.authority.cursor())?;
            batch.durability = report.durability;
        }
        report.remaining = batch.remaining();
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provisional_references_remap_through_nested_fields_and_reject_external_ids() {
        let ids = [(EntityId(1), EntityId(40)), (EntityId(2), EntityId(41))].into();
        let key = ContentKey::parse("test:links").unwrap();
        let mut value = SnapshotValue::Record(vec![(
            key.clone(),
            SnapshotValue::List(vec![
                SnapshotValue::Entity(EntityId(2)),
                SnapshotValue::Entity(EntityId(1)),
            ]),
        )]);
        remap(&mut value, &ids).unwrap();
        assert_eq!(
            value,
            SnapshotValue::Record(vec![(
                key,
                SnapshotValue::List(vec![
                    SnapshotValue::Entity(EntityId(41)),
                    SnapshotValue::Entity(EntityId(40))
                ])
            )])
        );
        assert!(remap(&mut SnapshotValue::Entity(EntityId(3)), &ids).is_err());
        let mut nested = SnapshotValue::Entity(EntityId(1));
        for _ in 0..=torse_world::MAX_VALUE_DEPTH {
            nested = SnapshotValue::List(vec![nested]);
        }
        assert_eq!(
            remap(&mut nested, &ids).unwrap_err().code,
            FaultCode::ResourceLimit
        );
    }
}
