//! Shared process reservations around pure preparation and durable publication.
use super::*;

pub(super) struct Resident {
    pool: MemoryPool,
    world: MemoryLease,
    runtime: MemoryLease,
    _control: MemoryLease,
}
pub(super) struct Prepared {
    growth: MemoryLease,
    work: MemoryLease,
    runtime_bytes: u64,
}
fn runtime_bytes(state: &RuntimeState, limits: ExecutionLimits) -> Result<u64, Fault> {
    let cost = wire::measure_inner(state, limits.runtime, limits.codec)?;
    cost.owned_bytes
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(u64::from(cost.items).checked_mul(128)?))
        .ok_or_else(|| Fault::resource_limit("runtime memory reservation overflow"))
}
impl ExecutionHost {
    /// Attach after recovery and before accepting sessions or replicas.
    pub fn attach_memory(&mut self, pool: MemoryPool) -> Result<(), Fault> {
        if self.memory.is_some()
            || !self.sessions.is_empty()
            || !self.pending.is_empty()
            || self.section_pending.is_some()
        {
            return Err(Fault::invalid_data(
                "execution memory account requires an idle host",
            ));
        }
        let _codec = pool.reserve(
            MemoryClass::Codec,
            self.limits.runtime.max_bytes.saturating_mul(4),
            EvictionRank::Pinned,
        )?;
        let control = self
            .limits
            .pending_bytes
            .checked_mul(2)
            .and_then(|bytes| {
                bytes.checked_add((self.limits.sessions as u64).checked_mul(1024 * 128)?)
            })
            .and_then(|bytes| bytes.checked_add(self.limits.runtime.max_bytes.checked_mul(2)?))
            .and_then(|bytes| {
                bytes.checked_add(u64::from(self.limits.runtime.max_items).checked_mul(128)?)
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    (crate::visual::TRANSITION_LIMIT
                        * size_of::<(SectionPos, crate::visual::Stamp, crate::visual::Stamp)>())
                        as u64,
                )
            })
            .ok_or_else(|| Fault::resource_limit("control memory reservation overflow"))?;
        let resident = Resident {
            world: pool.reserve(
                MemoryClass::World,
                self.world().accounted_bytes(),
                EvictionRank::Pinned,
            )?,
            runtime: pool.reserve(
                MemoryClass::Executor,
                runtime_bytes(&self.runtime, self.limits)?,
                EvictionRank::Pinned,
            )?,
            _control: pool.reserve(MemoryClass::Executor, control, EvictionRank::Pinned)?,
            pool: pool.clone(),
        };
        self.source.attach_memory(pool)?;
        self.memory = Some(resident);
        Ok(())
    }
    pub fn memory_pool(&self) -> Option<&MemoryPool> {
        self.memory.as_ref().map(|memory| &memory.pool)
    }
    pub(super) fn prepare_with_memory(&mut self) -> Result<Option<ExecutionPrepared>, Fault> {
        let Some(pool) = self.memory_pool().cloned() else {
            return self.prepare_unbudgeted();
        };
        // A queued residency transition already owns its growth reservation.
        if self.section_pending.is_some() {
            return self.prepare_unbudgeted();
        }
        let bytes = self
            .limits
            .runtime
            .max_bytes
            .checked_mul(4)
            .and_then(|bytes| {
                bytes.checked_add(u64::from(self.limits.runtime.max_items).checked_mul(128)?)
            })
            .ok_or_else(|| Fault::resource_limit("planning memory reservation overflow"))?;
        let work = match pool.reserve(MemoryClass::Executor, bytes, EvictionRank::Pinned) {
            Ok(lease) => lease,
            Err(error) if error.code == FaultCode::ResourceLimit => return Ok(None),
            Err(error) => return Err(error),
        };
        let _codec = match pool.reserve(
            MemoryClass::Codec,
            self.limits.runtime.max_bytes.saturating_mul(4),
            EvictionRank::Pinned,
        ) {
            Ok(lease) => lease,
            Err(error) if error.code == FaultCode::ResourceLimit => return Ok(None),
            Err(error) => return Err(error),
        };
        let Some(mut prepared) = self.prepare_unbudgeted()? else {
            return Ok(None);
        };
        match self.reserve_prepared(&mut prepared, work) {
            Ok(()) => Ok(Some(prepared)),
            Err(error) if error.code == FaultCode::ResourceLimit => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn reserve_prepared(
        &self,
        prepared: &mut ExecutionPrepared,
        work: MemoryLease,
    ) -> Result<(), Fault> {
        let pool = self.memory_pool().expect("attached memory");
        let growth = if let Some(normal) = &prepared.world.normal {
            self.world().growth_reservation(normal)
        } else if !prepared.record.changes.sections.is_empty() {
            prepared
                .record
                .changes
                .sections
                .iter()
                .map(|change| self.world().section_growth_reservation(&change.page))
                .fold(0u64, u64::saturating_add)
        } else if let Some(change) = &prepared.record.changes.section {
            if change.load {
                self.world().section_growth_reservation(&change.page)
            } else {
                0
            }
        } else {
            0
        };
        let growth = pool.reserve(MemoryClass::World, growth, EvictionRank::Pinned)?;
        let runtime_bytes = runtime_bytes(&prepared.runtime, self.limits)?;
        if runtime_bytes > work.bytes() {
            return Err(Fault::new(
                FaultCode::InvalidState,
                "runtime reservation underestimated",
            ));
        }
        prepared.memory = Some(Prepared {
            growth,
            work,
            runtime_bytes,
        });
        Ok(())
    }
    pub(super) fn account_residency(&self, prepared: &mut ExecutionPrepared) -> Result<(), Fault> {
        if let Some(pool) = self.memory_pool() {
            let _codec = pool.reserve(
                MemoryClass::Codec,
                self.limits.runtime.max_bytes.saturating_mul(4),
                EvictionRank::Pinned,
            )?;
            let work = pool.reserve(
                MemoryClass::Executor,
                runtime_bytes(&prepared.runtime, self.limits)?,
                EvictionRank::Pinned,
            )?;
            self.reserve_prepared(prepared, work)?;
        }
        Ok(())
    }
    pub(super) fn publish_memory(&mut self, prepared: Option<Prepared>) -> Result<(), Fault> {
        if self.memory.is_none() {
            return Ok(());
        }
        let world_bytes = self.world().accounted_bytes();
        if let (Some(resident), Some(mut prepared)) = (&mut self.memory, prepared) {
            if world_bytes
                > resident
                    .world
                    .bytes()
                    .saturating_add(prepared.growth.bytes())
            {
                return Err(Fault::new(
                    FaultCode::InvalidState,
                    "world reservation underestimated",
                ));
            }
            resident.world.absorb(&mut prepared.growth, world_bytes)?;
            resident
                .runtime
                .absorb(&mut prepared.work, prepared.runtime_bytes)?;
        }
        Ok(())
    }
}
