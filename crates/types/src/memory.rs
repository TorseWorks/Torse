//! Process memory ledger.
//!
//! Subsystems report bytes in a fixed set of classes. A charge that would pass
//! its class ceiling or the process total is refused with
//! [`FaultCode::ResourceLimit`](crate::FaultCode::ResourceLimit) and does not
//! change the recorded usage. Work that can wait keeps the demand and retries
//! on a later frame. The ledger keeps only as many waiting demands as its
//! retained limit; a full list refuses the new demand and leaves the records
//! already kept unchanged.
//!
//! [`MemoryLedger::eviction_plan`] names rebuildable residents to drop under
//! pressure, farthest meshes first. Charges in the world class stay on the
//! ledger until their owner releases them. Memory pressure must not delete
//! committed world state.
//!
//! The ledger is an account. It does not replace the process allocator, trace
//! individual allocations, share a heap across processes, or sandbox native
//! code. Saving a world and advancing the simulation tick do not consult it.

use std::collections::BTreeMap;

use crate::{Fault, FaultCode};

/// Accounted memory classes.
///
/// Names in this enum are the contract later subsystems report against.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum MemoryClass {
    /// Resident authoritative sections.
    World,
    /// Meshes and other geometry derived from sections.
    Mesh,
    /// Replication and network output buffers.
    Replica,
    /// CPU copies of resources that can be loaded again.
    Resource,
    /// Short-lived encode and decode buffers.
    Codec,
    /// Executor slot, input, and output reservations.
    Executor,
}

impl MemoryClass {
    /// Every class, in a stable order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::World,
        Self::Mesh,
        Self::Replica,
        Self::Resource,
        Self::Codec,
        Self::Executor,
    ];

    /// Number of classes.
    pub const COUNT: usize = 6;

    /// Index into a class-sized table.
    pub const fn index(self) -> usize {
        match self {
            Self::World => 0,
            Self::Mesh => 1,
            Self::Replica => 2,
            Self::Resource => 3,
            Self::Codec => 4,
            Self::Executor => 5,
        }
    }

    /// Stable lower-case name used in fault context.
    pub const fn name(self) -> &'static str {
        match self {
            Self::World => "world",
            Self::Mesh => "mesh",
            Self::Replica => "replica",
            Self::Resource => "resource",
            Self::Codec => "codec",
            Self::Executor => "executor",
        }
    }
}

impl core::fmt::Display for MemoryClass {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

/// How a charged resident may be dropped under memory pressure.
///
/// The world class ignores this rank and is stored as [`Self::Pinned`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvictionRank {
    /// The owner releases this charge. Pressure does not select it.
    Pinned,
    /// Geometry cache. `distance_sq` is the squared section distance from the
    /// camera. `u64::MAX` means the camera is unknown, so the mesh is treated
    /// as far. Zero is the camera section and is released only after the other
    /// rebuildable ranks.
    Mesh { distance_sq: u64 },
    /// Derived data for a section that has no subscriber.
    UnsubscribedDerived,
    /// A resource CPU copy that can be loaded again from its source.
    ReloadableResource,
}

/// One resident to charge or replace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryResident {
    /// Class the bytes count toward.
    pub class: MemoryClass,
    /// Bytes this resident holds. Zero is rejected.
    pub bytes: u64,
    /// Pressure policy for this resident.
    pub rank: EvictionRank,
}

/// Ceilings for one ledger.
///
/// Each class also shares `total_bytes`. A class limit above the process total
/// still cannot push the process past `total_bytes`. `retained_limit` bounds
/// how many refused demands may wait for a later frame. Those records are
/// ledger metadata, separate from the payload bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryBudget {
    class_bytes: [u64; MemoryClass::COUNT],
    total_bytes: u64,
    retained_limit: usize,
}

impl MemoryBudget {
    /// Demands [`Self::new`] keeps for a later frame.
    pub const DEFAULT_RETAINED_LIMIT: usize = 1024;

    /// Every class may use up to `total_bytes`, and so may the process.
    ///
    /// The retained list holds [`Self::DEFAULT_RETAINED_LIMIT`] demands.
    pub const fn new(total_bytes: u64) -> Self {
        Self {
            class_bytes: [total_bytes; MemoryClass::COUNT],
            total_bytes,
            retained_limit: Self::DEFAULT_RETAINED_LIMIT,
        }
    }

    /// Replace one class ceiling. The process total is unchanged.
    pub const fn with_class_limit(mut self, class: MemoryClass, bytes: u64) -> Self {
        self.class_bytes[class.index()] = bytes;
        self
    }

    /// Replace how many refused demands may wait.
    pub const fn with_retained_limit(mut self, limit: usize) -> Self {
        self.retained_limit = limit;
        self
    }

    /// Process-wide ceiling.
    pub const fn total_bytes(self) -> u64 {
        self.total_bytes
    }

    /// Ceiling for one class.
    pub const fn class_limit(self, class: MemoryClass) -> u64 {
        self.class_bytes[class.index()]
    }

    /// How many refused demands may wait for a later frame.
    pub const fn retained_limit(self) -> usize {
        self.retained_limit
    }
}

/// Handle for bytes currently charged to the ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[must_use = "release the charge or the ledger keeps the bytes"]
pub struct MemoryChargeId(u64);

/// Handle for a demand that could not be charged and should be retried.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[must_use = "retry or forget the retained demand"]
pub struct RetainedDemandId(u64);

/// A demand kept for a later frame, plus the fault that refused it.
#[derive(Debug)]
pub struct RetainedFailure {
    /// Identity passed to [`MemoryLedger::forget_retained`] or observed after retry.
    pub id: RetainedDemandId,
    /// [`FaultCode::ResourceLimit`] describing the ceiling that refused the charge.
    pub fault: Fault,
}

/// Why [`MemoryLedger::charge_retained`] did not return a charge.
#[derive(Debug)]
pub enum RetainFailure {
    /// The demand can never fit, contains no bytes, or the retained list is
    /// already full. Nothing was retained.
    Rejected(Fault),
    /// Usage is unchanged. Retry after something else releases bytes.
    Retained(RetainedFailure),
}

/// Residents selected for the caller to drop.
///
/// `bytes` may be smaller than the request when every remaining resident is
/// pinned. The ledger does not release these charges; the caller drops the
/// data and then calls [`MemoryLedger::release`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvictionPlan {
    /// Charges to drop, in the order they should be released.
    pub charges: Vec<MemoryChargeId>,
    /// Sum of the selected charges.
    pub bytes: u64,
}

impl EvictionPlan {
    /// Return whether the plan frees at least `needed` bytes.
    pub const fn covers(&self, needed: u64) -> bool {
        self.bytes >= needed
    }
}

/// A waiting demand that fit on this retry, with the charge created for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmittedDemand {
    /// Demand that was waiting.
    pub demand: RetainedDemandId,
    /// Charge created for `demand`.
    pub charge: MemoryChargeId,
}

/// Result of retrying demands retained on an earlier frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryReport {
    /// Demands that fit on this attempt, in retained order.
    ///
    /// Each entry names the original demand and the charge that replaced it.
    /// A demand that still does not fit is absent here and remains waiting.
    pub admitted: Vec<AdmittedDemand>,
    /// Demands still waiting.
    pub waiting: usize,
}

#[derive(Clone, Copy, Debug)]
struct Charge {
    class: MemoryClass,
    bytes: u64,
    rank: EvictionRank,
}

#[derive(Clone, Copy, Debug)]
struct Retained {
    id: u64,
    resident: MemoryResident,
}

/// Byte account shared by engine subsystems.
#[derive(Debug)]
pub struct MemoryLedger {
    budget: MemoryBudget,
    used: [u64; MemoryClass::COUNT],
    charges: BTreeMap<u64, Charge>,
    retained: Vec<Retained>,
    next_charge: u64,
    next_retained: u64,
}

impl MemoryLedger {
    /// Empty ledger bound by `budget`.
    pub const fn new(budget: MemoryBudget) -> Self {
        Self {
            budget,
            used: [0; MemoryClass::COUNT],
            charges: BTreeMap::new(),
            retained: Vec::new(),
            next_charge: 1,
            next_retained: 1,
        }
    }

    /// Ceilings this ledger enforces.
    pub const fn budget(&self) -> MemoryBudget {
        self.budget
    }

    /// Atomically replace the ceilings without revoking charges or waiting demands.
    ///
    /// A ceiling below live usage, the retained entry count, or an individual
    /// retained demand is refused. Release residents or forget unwanted demands
    /// before retrying. Success preserves every handle and does not retry demands.
    pub fn set_budget(&mut self, budget: MemoryBudget) -> Result<(), Fault> {
        if self.total_usage() > budget.total_bytes() {
            return Err(Fault::resource_limit(
                "new memory total is below live usage",
            ));
        }
        for class in MemoryClass::ALL {
            if self.usage(class) > budget.class_limit(class) {
                return Err(class_limit_fault(class));
            }
        }
        if self.retained.len() > budget.retained_limit()
            || self.retained.iter().any(|item| {
                item.resident.bytes > budget.total_bytes()
                    || item.resident.bytes > budget.class_limit(item.resident.class)
            })
        {
            return Err(Fault::resource_limit(
                "new memory budget cannot retain waiting demands",
            ));
        }
        self.budget = budget;
        Ok(())
    }

    /// Bytes currently charged to `class`.
    pub const fn usage(&self, class: MemoryClass) -> u64 {
        self.used[class.index()]
    }

    /// Bytes currently charged across every class.
    pub fn total_usage(&self) -> u64 {
        self.used.iter().sum()
    }

    /// Demands waiting for a later frame.
    pub const fn retained_len(&self) -> usize {
        self.retained.len()
    }

    /// Look up a live charge.
    pub fn get(&self, id: MemoryChargeId) -> Option<MemoryResident> {
        self.charges.get(&id.0).map(|charge| MemoryResident {
            class: charge.class,
            bytes: charge.bytes,
            rank: charge.rank,
        })
    }

    /// Charge `resident` immediately.
    ///
    /// On failure the ledger is unchanged. World-class residents are stored as
    /// [`EvictionRank::Pinned`] whatever rank the caller supplied.
    pub fn charge(&mut self, resident: MemoryResident) -> Result<MemoryChargeId, Fault> {
        let ids = self.exchange(&[], &[resident])?;
        match ids.as_slice() {
            [id] => Ok(*id),
            _ => Err(Fault::invalid_data(
                "memory exchange did not return the new charge",
            )),
        }
    }

    /// Charge `resident`, or keep it when the ceiling is only temporarily full.
    ///
    /// A demand larger than its class limit or the process total is rejected
    /// without being retained, because a later retry cannot admit it.
    pub fn charge_retained(
        &mut self,
        resident: MemoryResident,
    ) -> Result<MemoryChargeId, RetainFailure> {
        if resident.bytes == 0 {
            return Err(RetainFailure::Rejected(Fault::invalid_data(
                "memory charge must contain bytes",
            )));
        }
        if !self.admissible_eventually(resident.class, resident.bytes) {
            return Err(RetainFailure::Rejected(
                self.impossible_fault(resident.class, resident.bytes),
            ));
        }
        match self.charge(resident) {
            Ok(id) => Ok(id),
            Err(fault) if fault.code == FaultCode::ResourceLimit => {
                if self.retained.len() >= self.budget.retained_limit() {
                    return Err(RetainFailure::Rejected(Fault::resource_limit(
                        "retained memory demands exceed their entry limit",
                    )));
                }
                let id = self.push_retained(resident);
                Err(RetainFailure::Retained(RetainedFailure { id, fault }))
            }
            Err(fault) => Err(RetainFailure::Rejected(fault)),
        }
    }

    /// Drop a demand that the caller no longer wants to retry.
    pub fn forget_retained(&mut self, id: RetainedDemandId) -> Result<(), Fault> {
        let Some(index) = self.retained.iter().position(|item| item.id == id.0) else {
            return Err(Fault::invalid_data("unknown retained memory demand"));
        };
        self.retained.remove(index);
        Ok(())
    }

    /// Admit retained demands that fit now.
    ///
    /// Each demand is tried in the order it was retained. One that still does
    /// not fit stays retained and does not block a later, smaller demand.
    /// An admitted entry pairs that demand with the new charge.
    pub fn retry_retained(&mut self) -> RetryReport {
        let pending = std::mem::take(&mut self.retained);
        let mut admitted = Vec::new();
        let mut waiting = Vec::new();
        for item in pending {
            match self.charge(item.resident) {
                Ok(id) => admitted.push(AdmittedDemand {
                    demand: RetainedDemandId(item.id),
                    charge: id,
                }),
                Err(_) => waiting.push(item),
            }
        }
        self.retained = waiting;
        RetryReport {
            admitted,
            waiting: self.retained.len(),
        }
    }

    /// Release one charge. Releasing an unknown id leaves the ledger unchanged.
    pub fn release(&mut self, id: MemoryChargeId) -> Result<(), Fault> {
        self.exchange(&[id], &[])?;
        Ok(())
    }

    /// Replace `release` with `incoming` as one account update.
    ///
    /// The previous charges stay if the incoming residents do not fit, or if
    /// any id in `release` is unknown or repeated.
    pub fn exchange(
        &mut self,
        release: &[MemoryChargeId],
        incoming: &[MemoryResident],
    ) -> Result<Vec<MemoryChargeId>, Fault> {
        if !release_ids_unique(release) {
            return Err(Fault::invalid_data("duplicate memory charge in exchange"));
        }
        let mut released = Vec::with_capacity(release.len());
        for id in release {
            let Some(charge) = self.charges.get(&id.0).copied() else {
                return Err(Fault::invalid_data("unknown memory charge"));
            };
            released.push((*id, charge));
        }
        for resident in incoming {
            if resident.bytes == 0 {
                return Err(Fault::invalid_data("memory charge must contain bytes"));
            }
        }

        let mut used = self.used;
        for (_, charge) in &released {
            used[charge.class.index()] -= charge.bytes;
        }
        let mut normalized = Vec::with_capacity(incoming.len());
        for resident in incoming {
            let resident = normalize(*resident);
            let class = resident.class;
            let next_class = used[class.index()]
                .checked_add(resident.bytes)
                .ok_or_else(|| {
                    Fault::resource_limit(format!("memory charge overflows the {class} ledger"))
                })?;
            if next_class > self.budget.class_limit(class) {
                return Err(class_limit_fault(class));
            }
            let next_total: u64 = used.iter().sum();
            let next_total = next_total.checked_add(resident.bytes).ok_or_else(|| {
                Fault::resource_limit(format!("memory charge overflows the {class} ledger"))
            })?;
            if next_total > self.budget.total_bytes() {
                return Err(process_limit_fault(class));
            }
            used[class.index()] = next_class;
            normalized.push(resident);
        }

        let start = self.reserve_ids(normalized.len())?;
        for (id, _) in &released {
            self.charges.remove(&id.0);
        }
        self.used = used;
        let mut ids = Vec::with_capacity(normalized.len());
        for (offset, resident) in normalized.into_iter().enumerate() {
            let id = start + offset as u64;
            self.charges.insert(
                id,
                Charge {
                    class: resident.class,
                    bytes: resident.bytes,
                    rank: resident.rank,
                },
            );
            ids.push(MemoryChargeId(id));
        }
        Ok(ids)
    }

    /// Choose rebuildable charges to free at least `needed` bytes.
    ///
    /// Order: meshes with a non-zero camera distance, farthest first; derived
    /// data for unsubscribed sections; reloadable resource copies; meshes on
    /// the camera section. Equal distances release the earlier charge first.
    /// Pinned charges and every world-class charge are omitted.
    /// `needed == 0` selects nothing.
    pub fn eviction_plan(&self, needed: u64) -> EvictionPlan {
        if needed == 0 {
            return EvictionPlan {
                charges: Vec::new(),
                bytes: 0,
            };
        }
        let mut candidates = Vec::new();
        for (id, charge) in &self.charges {
            if charge.class == MemoryClass::World {
                continue;
            }
            let Some((tier, distance_sq)) = eviction_key(charge.rank) else {
                continue;
            };
            candidates.push((*id, tier, distance_sq, charge.bytes));
        }
        candidates.sort_by(|left, right| {
            left.1
                .cmp(&right.1)
                .then_with(|| right.2.cmp(&left.2))
                .then_with(|| left.0.cmp(&right.0))
        });

        let mut charges = Vec::new();
        let mut bytes = 0u64;
        for (id, _, _, charge_bytes) in candidates {
            if bytes >= needed {
                break;
            }
            charges.push(MemoryChargeId(id));
            bytes = bytes.saturating_add(charge_bytes);
        }
        EvictionPlan { charges, bytes }
    }

    fn admissible_eventually(&self, class: MemoryClass, bytes: u64) -> bool {
        bytes <= self.budget.class_limit(class) && bytes <= self.budget.total_bytes()
    }

    fn impossible_fault(&self, class: MemoryClass, bytes: u64) -> Fault {
        if bytes > self.budget.class_limit(class) {
            Fault::resource_limit(format!(
                "memory charge of {bytes} bytes exceeds the {class} limit"
            ))
        } else {
            Fault::resource_limit(format!(
                "memory charge of {bytes} bytes exceeds the process limit"
            ))
        }
    }

    fn push_retained(&mut self, resident: MemoryResident) -> RetainedDemandId {
        let id = self.next_retained;
        self.next_retained = self.next_retained.saturating_add(1);
        self.retained.push(Retained { id, resident });
        RetainedDemandId(id)
    }

    fn reserve_ids(&mut self, count: usize) -> Result<u64, Fault> {
        let count = u64::try_from(count)
            .map_err(|_| Fault::resource_limit("memory charge ids exhausted"))?;
        let start = self.next_charge;
        self.next_charge = start
            .checked_add(count)
            .ok_or_else(|| Fault::resource_limit("memory charge ids exhausted"))?;
        Ok(start)
    }
}

fn normalize(resident: MemoryResident) -> MemoryResident {
    if resident.class == MemoryClass::World {
        MemoryResident {
            rank: EvictionRank::Pinned,
            ..resident
        }
    } else {
        resident
    }
}

fn class_limit_fault(class: MemoryClass) -> Fault {
    Fault::resource_limit(format!("memory class {class} exceeds its byte limit"))
}

fn process_limit_fault(class: MemoryClass) -> Fault {
    Fault::resource_limit(format!(
        "process memory budget exceeded while charging {class}"
    ))
}

fn release_ids_unique(release: &[MemoryChargeId]) -> bool {
    let mut seen = BTreeMap::new();
    for id in release {
        if seen.insert(id.0, ()).is_some() {
            return false;
        }
    }
    true
}

/// Tier and sort distance. `None` means the rank is not evictable.
fn eviction_key(rank: EvictionRank) -> Option<(u8, u64)> {
    match rank {
        EvictionRank::Pinned => None,
        EvictionRank::Mesh { distance_sq } if distance_sq > 0 => Some((0, distance_sq)),
        EvictionRank::UnsubscribedDerived => Some((1, 0)),
        EvictionRank::ReloadableResource => Some((2, 0)),
        EvictionRank::Mesh { .. } => Some((3, 0)),
    }
}

#[cfg(test)]
mod tests {
    use crate::FaultCode;

    use super::*;

    fn resident(class: MemoryClass, bytes: u64, rank: EvictionRank) -> MemoryResident {
        MemoryResident { class, bytes, rank }
    }

    #[test]
    fn class_limit_refuses_without_changing_usage() {
        let mut ledger =
            MemoryLedger::new(MemoryBudget::new(1_000).with_class_limit(MemoryClass::Mesh, 100));
        let _kept = ledger
            .charge(resident(MemoryClass::Mesh, 80, EvictionRank::Pinned))
            .unwrap();
        let error = ledger
            .charge(resident(
                MemoryClass::Mesh,
                30,
                EvictionRank::Mesh { distance_sq: 4 },
            ))
            .unwrap_err();
        assert_eq!(error.code, FaultCode::ResourceLimit);
        assert_eq!(error.context, "memory class mesh exceeds its byte limit");
        assert_eq!(ledger.usage(MemoryClass::Mesh), 80);
        assert_eq!(ledger.total_usage(), 80);
    }

    #[test]
    fn process_limit_applies_across_classes() {
        let budget = MemoryBudget::new(100)
            .with_class_limit(MemoryClass::World, 1_000)
            .with_class_limit(MemoryClass::Mesh, 1_000);
        let mut ledger = MemoryLedger::new(budget);
        let _kept = ledger
            .charge(resident(
                MemoryClass::World,
                70,
                EvictionRank::Mesh { distance_sq: 9 },
            ))
            .unwrap();
        let error = ledger
            .charge(resident(MemoryClass::Mesh, 40, EvictionRank::Pinned))
            .unwrap_err();
        assert_eq!(error.code, FaultCode::ResourceLimit);
        assert_eq!(
            error.context,
            "process memory budget exceeded while charging mesh"
        );
        assert_eq!(ledger.usage(MemoryClass::World), 70);
        assert_eq!(ledger.usage(MemoryClass::Mesh), 0);
        assert!(matches!(
            ledger.get(MemoryChargeId(1)).unwrap().rank,
            EvictionRank::Pinned
        ));
    }

    #[test]
    fn retained_demand_retries_after_a_release_and_skips_a_permanent_charge() {
        let mut ledger = MemoryLedger::new(MemoryBudget::new(10));
        let held = ledger
            .charge(resident(MemoryClass::Executor, 6, EvictionRank::Pinned))
            .unwrap();
        let failure = ledger
            .charge_retained(resident(
                MemoryClass::Mesh,
                8,
                EvictionRank::Mesh { distance_sq: 1 },
            ))
            .unwrap_err();
        let RetainFailure::Retained(retained) = failure else {
            panic!("temporary overflow must be retained");
        };
        assert_eq!(retained.fault.code, FaultCode::ResourceLimit);
        assert_eq!(ledger.retained_len(), 1);
        assert_eq!(ledger.total_usage(), 6);

        let permanent = ledger
            .charge_retained(resident(MemoryClass::Codec, 11, EvictionRank::Pinned))
            .unwrap_err();
        assert!(matches!(permanent, RetainFailure::Rejected(_)));
        assert_eq!(ledger.retained_len(), 1);

        let waiting = ledger.retry_retained();
        assert!(waiting.admitted.is_empty());
        assert_eq!(waiting.waiting, 1);

        ledger.release(held).unwrap();
        let retried = ledger.retry_retained();
        assert_eq!(retried.admitted.len(), 1);
        assert_eq!(retried.waiting, 0);
        assert_eq!(ledger.usage(MemoryClass::Mesh), 8);
        assert_eq!(ledger.retained_len(), 0);
    }

    #[test]
    fn a_large_retained_demand_does_not_block_a_smaller_one() {
        let mut ledger = MemoryLedger::new(MemoryBudget::new(10));
        let large_holder = ledger
            .charge(resident(MemoryClass::Replica, 4, EvictionRank::Pinned))
            .unwrap();
        let _small_holder = ledger
            .charge(resident(MemoryClass::Replica, 3, EvictionRank::Pinned))
            .unwrap();
        ledger
            .charge_retained(resident(MemoryClass::Codec, 8, EvictionRank::Pinned))
            .unwrap_err();
        ledger
            .charge_retained(resident(MemoryClass::Codec, 4, EvictionRank::Pinned))
            .unwrap_err();
        ledger.release(large_holder).unwrap();
        let report = ledger.retry_retained();
        assert_eq!(report.admitted.len(), 1);
        assert_eq!(report.waiting, 1);
        assert_eq!(ledger.usage(MemoryClass::Codec), 4);
        assert_eq!(ledger.usage(MemoryClass::Replica), 3);
    }

    #[test]
    fn pressure_drops_far_meshes_before_other_rebuildable_copies_and_never_world() {
        let mut ledger = MemoryLedger::new(MemoryBudget::new(10_000));
        let world = ledger
            .charge(resident(
                MemoryClass::World,
                500,
                EvictionRank::Mesh { distance_sq: 100 },
            ))
            .unwrap();
        let near = ledger
            .charge(resident(
                MemoryClass::Mesh,
                10,
                EvictionRank::Mesh { distance_sq: 0 },
            ))
            .unwrap();
        let derived = ledger
            .charge(resident(
                MemoryClass::Mesh,
                20,
                EvictionRank::UnsubscribedDerived,
            ))
            .unwrap();
        let resource = ledger
            .charge(resident(
                MemoryClass::Resource,
                30,
                EvictionRank::ReloadableResource,
            ))
            .unwrap();
        let mid = ledger
            .charge(resident(
                MemoryClass::Mesh,
                40,
                EvictionRank::Mesh { distance_sq: 9 },
            ))
            .unwrap();
        let far = ledger
            .charge(resident(
                MemoryClass::Mesh,
                50,
                EvictionRank::Mesh { distance_sq: 25 },
            ))
            .unwrap();
        let distant = ledger
            .charge(resident(
                MemoryClass::Mesh,
                60,
                EvictionRank::Mesh { distance_sq: 100 },
            ))
            .unwrap();
        let executor = ledger
            .charge(resident(MemoryClass::Executor, 70, EvictionRank::Pinned))
            .unwrap();

        let plan = ledger.eviction_plan(90);
        assert_eq!(plan.charges, vec![distant, far]);
        assert_eq!(plan.bytes, 110);
        assert!(plan.covers(90));
        assert!(!plan.charges.contains(&world));
        assert!(!plan.charges.contains(&near));
        assert!(!plan.charges.contains(&mid));
        assert!(!plan.charges.contains(&resource));
        assert!(!plan.charges.contains(&executor));
        assert!(ledger.get(world).is_some());

        let rest = ledger.eviction_plan(10_000);
        assert_eq!(
            rest.charges,
            vec![distant, far, mid, derived, resource, near]
        );
        assert!(!rest.charges.contains(&world));
        assert!(!rest.charges.contains(&executor));
    }

    #[test]
    fn exchange_keeps_the_previous_charge_when_the_replacement_does_not_fit() {
        let mut ledger = MemoryLedger::new(MemoryBudget::new(10));
        let id = ledger
            .charge(resident(
                MemoryClass::Mesh,
                5,
                EvictionRank::Mesh { distance_sq: 1 },
            ))
            .unwrap();
        let error = ledger
            .exchange(
                &[id],
                &[resident(
                    MemoryClass::Mesh,
                    11,
                    EvictionRank::Mesh { distance_sq: 4 },
                )],
            )
            .unwrap_err();
        assert_eq!(error.code, FaultCode::ResourceLimit);
        assert_eq!(ledger.usage(MemoryClass::Mesh), 5);
        assert!(ledger.get(id).is_some());
        ledger.release(id).unwrap();
        assert_eq!(ledger.total_usage(), 0);
        assert_eq!(
            ledger.release(id).unwrap_err().context,
            "unknown memory charge"
        );
    }

    #[test]
    fn zero_bytes_and_duplicate_release_are_rejected() {
        let mut ledger = MemoryLedger::new(MemoryBudget::new(10));
        assert_eq!(
            ledger
                .charge(resident(MemoryClass::Codec, 0, EvictionRank::Pinned))
                .unwrap_err()
                .code,
            FaultCode::InvalidData
        );
        let id = ledger
            .charge(resident(MemoryClass::Codec, 1, EvictionRank::Pinned))
            .unwrap();
        assert_eq!(
            ledger.exchange(&[id, id], &[]).unwrap_err().context,
            "duplicate memory charge in exchange"
        );
        assert_eq!(ledger.usage(MemoryClass::Codec), 1);
    }

    #[test]
    fn empty_plan_when_nothing_is_requested() {
        let mut ledger = MemoryLedger::new(MemoryBudget::new(100));
        let _kept = ledger
            .charge(resident(
                MemoryClass::Mesh,
                10,
                EvictionRank::Mesh {
                    distance_sq: u64::MAX,
                },
            ))
            .unwrap();
        let plan = ledger.eviction_plan(0);
        assert!(plan.charges.is_empty());
        assert!(plan.covers(0));
    }

    fn retained(result: Result<MemoryChargeId, RetainFailure>) -> RetainedDemandId {
        match result {
            Err(RetainFailure::Retained(failure)) => failure.id,
            Ok(_) => panic!("demand must wait"),
            Err(RetainFailure::Rejected(fault)) => panic!("demand was rejected: {fault:?}"),
        }
    }

    #[test]
    fn retry_names_the_later_demand_when_an_earlier_one_still_does_not_fit() {
        let mut ledger = MemoryLedger::new(MemoryBudget::new(10));
        let _stays = ledger
            .charge(resident(MemoryClass::Replica, 4, EvictionRank::Pinned))
            .unwrap();
        let released = ledger
            .charge(resident(MemoryClass::Executor, 3, EvictionRank::Pinned))
            .unwrap();
        let mesh = retained(ledger.charge_retained(resident(
            MemoryClass::Mesh,
            8,
            EvictionRank::Mesh { distance_sq: 4 },
        )));
        let codec =
            retained(ledger.charge_retained(resident(MemoryClass::Codec, 4, EvictionRank::Pinned)));
        ledger.release(released).unwrap();

        let report = ledger.retry_retained();
        assert_eq!(report.admitted.len(), 1);
        assert_eq!(report.admitted[0].demand, codec);
        assert_ne!(report.admitted[0].demand, mesh);
        assert_eq!(report.waiting, 1);
        assert_eq!(ledger.retained_len(), 1);
        let charge = ledger.get(report.admitted[0].charge).unwrap();
        assert_eq!(charge.class, MemoryClass::Codec);
        assert_eq!(charge.bytes, 4);
        assert_eq!(ledger.usage(MemoryClass::Mesh), 0);
        assert_eq!(ledger.usage(MemoryClass::Codec), 4);
        assert_eq!(ledger.usage(MemoryClass::Replica), 4);
        assert_eq!(ledger.usage(MemoryClass::Executor), 0);
    }

    #[test]
    fn retry_pairs_each_same_size_demand_with_its_own_charge() {
        let mut ledger = MemoryLedger::new(MemoryBudget::new(10));
        let held = ledger
            .charge(resident(MemoryClass::Executor, 8, EvictionRank::Pinned))
            .unwrap();
        let first = retained(ledger.charge_retained(resident(
            MemoryClass::Resource,
            4,
            EvictionRank::ReloadableResource,
        )));
        let second = retained(ledger.charge_retained(resident(
            MemoryClass::Resource,
            4,
            EvictionRank::ReloadableResource,
        )));
        ledger.release(held).unwrap();

        let report = ledger.retry_retained();
        assert_eq!(report.waiting, 0);
        assert_eq!(report.admitted.len(), 2);
        assert_eq!(report.admitted[0].demand, first);
        assert_eq!(report.admitted[1].demand, second);
        assert_ne!(report.admitted[0].charge, report.admitted[1].charge);
        assert_eq!(ledger.get(report.admitted[0].charge).unwrap().bytes, 4);
        assert_eq!(ledger.get(report.admitted[1].charge).unwrap().bytes, 4);
        assert_eq!(ledger.usage(MemoryClass::Resource), 8);
    }

    #[test]
    fn retained_list_stops_at_its_entry_limit_and_a_freed_slot_can_be_reused() {
        let mut ledger = MemoryLedger::new(MemoryBudget::new(1).with_retained_limit(2));
        assert_eq!(
            MemoryBudget::new(1).retained_limit(),
            MemoryBudget::DEFAULT_RETAINED_LIMIT
        );
        let held = ledger
            .charge(resident(MemoryClass::Executor, 1, EvictionRank::Pinned))
            .unwrap();
        let first = retained(ledger.charge_retained(resident(
            MemoryClass::Mesh,
            1,
            EvictionRank::Mesh { distance_sq: 1 },
        )));
        let _second =
            retained(ledger.charge_retained(resident(MemoryClass::Codec, 1, EvictionRank::Pinned)));

        for _ in 0..32 {
            let rejected = ledger
                .charge_retained(resident(
                    MemoryClass::Resource,
                    1,
                    EvictionRank::ReloadableResource,
                ))
                .unwrap_err();
            assert!(
                matches!(rejected, RetainFailure::Rejected(ref fault) if fault.code == FaultCode::ResourceLimit)
            );
        }
        assert_eq!(ledger.retained_len(), 2);
        assert_eq!(ledger.total_usage(), 1);

        ledger.forget_retained(first).unwrap();
        assert_eq!(ledger.retained_len(), 1);
        let _third = retained(ledger.charge_retained(resident(
            MemoryClass::Mesh,
            1,
            EvictionRank::Mesh { distance_sq: 9 },
        )));
        assert_eq!(ledger.retained_len(), 2);
        assert_eq!(ledger.total_usage(), 1);

        ledger.release(held).unwrap();
        let report = ledger.retry_retained();
        assert_eq!(report.admitted.len(), 1);
        assert_eq!(report.waiting, 1);
        assert_eq!(ledger.retained_len(), 1);
        let _reused = retained(ledger.charge_retained(resident(
            MemoryClass::Replica,
            1,
            EvictionRank::Pinned,
        )));
        assert_eq!(ledger.retained_len(), 2);
        assert_eq!(ledger.total_usage(), 1);
    }
}
