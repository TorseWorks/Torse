//! One bounded computation and publication per input version, shared by consumers.
use super::{LightInputVersion, LightJob, LightLimits, LightSection, LightSource};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use torse_types::{Binding, Fault, SectionPos};
use torse_world::BlockStateRegistry;

#[derive(Clone, Copy, Debug)]
pub struct LightFieldLimits {
    pub sections: usize,
    pub cache_bytes: usize,
    pub consumers: usize,
    /// Total consumer-to-section links, including overlapping interests.
    pub interests: usize,
    pub job: LightLimits,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LightFieldDiagnostics {
    pub started: u64,
    pub incremental_started: u64,
    pub update_frontier_visits: u64,
    pub published: u64,
    pub coalesced: u64,
    pub stale: u64,
    pub budget_denials: u64,
    pub pending: usize,
    pub pending_peak: usize,
    pub cache_bytes: usize,
    pub cache_peak: usize,
    /// Two bounded jobs can coexist. Does not include external source storage.
    pub work_reservation: u64,
}
pub struct LightPublication {
    pub binding: Binding,
    pub generation: u64,
    pub input: LightInputVersion,
    pub rules_version: u32,
    pub emitted: LightSection,
    pub sky: LightSection,
}
impl LightPublication {
    pub fn validate(
        &self,
        source: &(impl LightSource + ?Sized),
        registry: &BlockStateRegistry,
    ) -> bool {
        self.emitted.validate(self.binding, source, registry)
            && self.sky.validate(self.binding, source, registry)
    }
    pub fn allocated_bytes(&self) -> usize {
        size_of::<Self>() + 64 + self.emitted.allocated_bytes() + self.sky.allocated_bytes()
    }
}
struct Work {
    target: SectionPos,
    emitted: Option<LightJob>,
    sky: Option<LightJob>,
    emitted_result: Option<LightSection>,
    sky_result: Option<LightSection>,
}
struct Retained {
    emitted: LightJob,
    sky: LightJob,
}
impl Retained {
    fn bytes(&self) -> usize {
        self.emitted.allocated_bytes() + self.sky.allocated_bytes()
    }
}
/// The host owns this field once per world. Consumers only retain interests and
/// publication generations; they never own a separate calculation queue.
pub struct SharedLightField {
    binding: Binding,
    limits: LightFieldLimits,
    consumers: BTreeMap<u128, BTreeSet<SectionPos>>,
    entries: BTreeMap<SectionPos, Arc<LightPublication>>,
    retained: BTreeMap<SectionPos, Retained>,
    pending: BTreeSet<SectionPos>,
    blocked: BTreeSet<SectionPos>,
    work: Option<Work>,
    generation: u64,
    diagnostics: LightFieldDiagnostics,
}
impl SharedLightField {
    pub fn new(binding: Binding, limits: LightFieldLimits) -> Result<Self, Fault> {
        if limits.sections == 0
            || limits.sections > 32768
            || limits.cache_bytes == 0
            || limits.consumers == 0
            || limits.consumers > 4096
            || limits.interests == 0
            || limits.interests > 1_000_000
            || limits.job.cells == 0
            || limits.job.cells > 1_000_000
            || limits.job.dependencies == 0
            || limits.job.dependencies > 32768
            || limits.job.frontier == 0
            || limits.job.frontier > 2_000_000
            || limits.job.bytes == 0
        {
            return Err(Fault::resource_limit("shared light field limits"));
        }
        let work_reservation = limits
            .job
            .bytes
            .checked_mul(2)
            .ok_or_else(|| Fault::resource_limit("light work reservation"))?;
        Ok(Self {
            binding,
            limits,
            consumers: BTreeMap::new(),
            entries: BTreeMap::new(),
            retained: BTreeMap::new(),
            pending: BTreeSet::new(),
            blocked: BTreeSet::new(),
            work: None,
            generation: 0,
            diagnostics: LightFieldDiagnostics {
                work_reservation,
                ..Default::default()
            },
        })
    }
    /// Replace one consumer's interest atomically. Delivery/backpressure is a
    /// separate concern and cannot block this host-owned computation queue.
    pub fn subscribe(&mut self, consumer: u128, sections: &[SectionPos]) -> Result<(), Fault> {
        if sections.len() > self.limits.sections || sections.windows(2).any(|w| w[0] >= w[1]) {
            return Err(Fault::resource_limit(
                "light interest must be sorted and bounded",
            ));
        }
        if !self.consumers.contains_key(&consumer) && self.consumers.len() == self.limits.consumers
        {
            return Err(Fault::resource_limit("light consumer capacity"));
        }
        let links: usize = self
            .consumers
            .iter()
            .filter(|(id, _)| **id != consumer)
            .map(|(_, s)| s.len())
            .sum();
        if links + sections.len() > self.limits.interests {
            return Err(Fault::resource_limit("light interest capacity"));
        }
        let mut wanted = BTreeSet::new();
        for (&id, scope) in &self.consumers {
            if id != consumer {
                wanted.extend(scope);
            }
        }
        wanted.extend(sections);
        if wanted.len() > self.limits.sections {
            return Err(Fault::resource_limit("light target capacity"));
        }
        for pos in sections {
            if self.entries.contains_key(pos)
                || self.pending.contains(pos)
                || self.work.as_ref().is_some_and(|w| w.target == *pos)
            {
                self.diagnostics.coalesced = self.diagnostics.coalesced.saturating_add(1);
            }
        }
        self.consumers
            .insert(consumer, sections.iter().copied().collect());
        self.retain(wanted);
        Ok(())
    }
    pub fn unsubscribe(&mut self, consumer: u128) {
        self.consumers.remove(&consumer);
        let wanted = self
            .consumers
            .values()
            .flat_map(|s| s.iter().copied())
            .collect();
        self.retain(wanted);
    }
    fn retain(&mut self, wanted: BTreeSet<SectionPos>) {
        self.blocked.clear();
        self.entries.retain(|p, _| wanted.contains(p));
        self.retained.retain(|p, _| wanted.contains(p));
        self.pending.retain(|p| wanted.contains(p));
        if self
            .work
            .as_ref()
            .is_some_and(|w| !wanted.contains(&w.target))
        {
            self.work = None;
        }
        for pos in wanted {
            if !self.entries.contains_key(&pos)
                && self.work.as_ref().is_none_or(|w| w.target != pos)
            {
                self.pending.insert(pos);
            }
        }
        self.measure();
    }
    /// Call after an input transaction. Negative dependencies are included, so
    /// the arrival of a previously unknown neighbor invalidates incomplete light.
    /// Revalidation covers cache entries and in-flight jobs even without a hint.
    pub fn inputs_changed(
        &mut self,
        source: &(impl LightSource + ?Sized),
        registry: &BlockStateRegistry,
    ) {
        for (&pos, entry) in &self.entries {
            if entry.validate(source, registry) {
                self.pending.remove(&pos);
            } else {
                self.pending.insert(pos);
            }
        }
        self.measure();
    }
    /// Borrow a current publication. Two consumers get the very same Arc.
    /// Revoked consumers cannot retrieve a retained result.
    pub fn publication(
        &self,
        consumer: u128,
        target: SectionPos,
        source: &(impl LightSource + ?Sized),
        registry: &BlockStateRegistry,
    ) -> Option<&Arc<LightPublication>> {
        if !self.consumers.get(&consumer)?.contains(&target) {
            return None;
        }
        self.entries
            .get(&target)
            .filter(|p| p.validate(source, registry))
    }
    pub fn diagnostics(&self) -> LightFieldDiagnostics {
        self.diagnostics
    }
    /// Retry budget-denied targets after freeing capacity or explicit refresh.
    pub fn retry(&mut self) {
        self.blocked.clear();
    }

    /// Advance at most `work` seed cells/frontier pops. Preparation, final copy,
    /// and dependency checks are additionally bounded by the configured job.
    /// Returns the newly published target, if any. Output queues are never used.
    pub fn poll(
        &mut self,
        source: &(impl LightSource + ?Sized),
        registry: &BlockStateRegistry,
        work: usize,
    ) -> Result<Option<SectionPos>, Fault> {
        if work == 0 {
            return Ok(None);
        }
        if source.binding().is_some_and(|b| b != self.binding) {
            return Err(Fault::invalid_data("light field binding"));
        }
        if self.work.is_none() {
            // Unknown targets wait without spinning calculations. A later input
            // arrival needs no new subscription to resume them.
            let target = self
                .pending
                .iter()
                .find(|&&p| {
                    !self.blocked.contains(&p)
                        && matches!(
                            source.version(p),
                            LightInputVersion::Content(_)
                                | LightInputVersion::Resident { readable: true, .. }
                        )
                })
                .copied();
            let Some(target) = target else {
                return Ok(None);
            };
            let warm = self.retained.remove(&target).and_then(|mut retained| {
                if retained
                    .emitted
                    .update(self.binding, source, registry)
                    .is_ok()
                    && retained.sky.update(self.binding, source, registry).is_ok()
                {
                    Some(retained)
                } else {
                    None
                }
            });
            let prepared = (|| -> Result<Retained, Fault> {
                Ok(if let Some(warm) = warm {
                    self.diagnostics.incremental_started =
                        self.diagnostics.incremental_started.saturating_add(1);
                    warm
                } else {
                    Retained {
                        emitted: LightJob::prepare(
                            self.binding,
                            source,
                            registry,
                            target,
                            self.limits.job,
                        )?
                        .reusable(self.limits.job.bytes)?,
                        sky: LightJob::prepare_sky(
                            self.binding,
                            source,
                            registry,
                            target,
                            self.limits.job,
                        )?
                        .reusable(self.limits.job.bytes)?,
                    }
                })
            })();
            let Retained { emitted, sky } = match prepared {
                Ok(work) => work,
                Err(e) => {
                    if e.code == torse_types::FaultCode::ResourceLimit {
                        self.blocked.insert(target);
                        self.diagnostics.budget_denials =
                            self.diagnostics.budget_denials.saturating_add(1);
                    }
                    self.measure();
                    return Err(e);
                }
            };
            self.work = Some(Work {
                target,
                emitted: Some(emitted),
                sky: Some(sky),
                emitted_result: None,
                sky_result: None,
            });
            self.pending.remove(&target);
            self.diagnostics.started = self.diagnostics.started.saturating_add(1);
            self.measure();
        }
        let mut active = self.work.take().expect("prepared lighting work");
        let result = advance(&mut active, self.binding, source, registry, work);
        match result {
            Err(e) => {
                self.pending.insert(active.target);
                if e.code == torse_types::FaultCode::ResourceLimit {
                    self.blocked.insert(active.target);
                    self.diagnostics.budget_denials =
                        self.diagnostics.budget_denials.saturating_add(1);
                } else {
                    self.diagnostics.stale = self.diagnostics.stale.saturating_add(1);
                }
                self.measure();
                return Err(e);
            }
            Ok(false) => {
                self.work = Some(active);
                self.measure();
                return Ok(None);
            }
            Ok(true) => {}
        }
        let emitted = active
            .emitted_result
            .take()
            .expect("finished emitted light");
        let sky = active.sky_result.take().expect("finished skylight");
        if !emitted.validate(self.binding, source, registry)
            || !sky.validate(self.binding, source, registry)
        {
            self.pending.insert(active.target);
            self.diagnostics.stale = self.diagnostics.stale.saturating_add(1);
            self.measure();
            return Ok(None);
        }
        let Some(generation) = self.generation.checked_add(1) else {
            self.pending.insert(active.target);
            self.blocked.insert(active.target);
            self.measure();
            return Err(Fault::resource_limit("light publication generation"));
        };
        let output = LightPublication {
            binding: self.binding,
            generation,
            input: emitted.input_version().expect("target dependency"),
            rules_version: emitted.rules_version(),
            emitted,
            sky,
        };
        let retained = Retained {
            emitted: active.emitted.take().expect("retained emission"),
            sky: active.sky.take().expect("retained sky"),
        };
        self.diagnostics.update_frontier_visits = self
            .diagnostics
            .update_frontier_visits
            .saturating_add(retained.emitted.update_frontier_visits())
            .saturating_add(retained.sky.update_frontier_visits());
        if self
            .diagnostics
            .cache_bytes
            .checked_sub(
                self.entries
                    .get(&active.target)
                    .map_or(0, |p| p.allocated_bytes()),
            )
            .and_then(|bytes| bytes.checked_add(output.allocated_bytes()))
            .and_then(|bytes| bytes.checked_add(retained.bytes()))
            .is_none_or(|n| n > self.limits.cache_bytes)
        {
            self.pending.insert(active.target);
            self.blocked.insert(active.target);
            self.diagnostics.budget_denials = self.diagnostics.budget_denials.saturating_add(1);
            self.measure();
            return Err(Fault::resource_limit("light publication cache"));
        }
        self.entries.insert(active.target, Arc::new(output));
        self.retained.insert(active.target, retained);
        self.generation = generation;
        self.diagnostics.published = self.diagnostics.published.saturating_add(1);
        self.measure();
        Ok(Some(active.target))
    }
    fn measure(&mut self) {
        self.diagnostics.pending = self.pending.len() + usize::from(self.work.is_some());
        self.diagnostics.pending_peak = self.diagnostics.pending_peak.max(self.diagnostics.pending);
        self.diagnostics.cache_bytes = self
            .entries
            .values()
            .map(|p| p.allocated_bytes())
            .sum::<usize>()
            + self.retained.values().map(Retained::bytes).sum::<usize>();
        self.diagnostics.cache_peak = self
            .diagnostics
            .cache_peak
            .max(self.diagnostics.cache_bytes);
    }
}
fn advance(
    active: &mut Work,
    binding: Binding,
    source: &(impl LightSource + ?Sized),
    registry: &BlockStateRegistry,
    work: usize,
) -> Result<bool, Fault> {
    let emission_work = if active.sky_result.is_none() {
        work.div_ceil(2)
    } else {
        work
    };
    let sky_work = if active.emitted_result.is_none() {
        work / 2
    } else {
        work
    };
    if active.emitted_result.is_none() {
        let job = active.emitted.as_mut().expect("retained emission job");
        if job.poll(binding, source, registry, emission_work)? {
            active.emitted_result = Some(job.capture(binding, source, registry)?);
        }
    }
    if active.sky_result.is_none() {
        let job = active.sky.as_mut().expect("retained sky job");
        if job.poll(binding, source, registry, sky_work)? {
            active.sky_result = Some(job.capture(binding, source, registry)?);
        }
    }
    Ok(active.emitted_result.is_some() && active.sky_result.is_some())
}
