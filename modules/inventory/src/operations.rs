//! Explicit, owner-authored plans for external item ingress, withdrawal and consumption.
use crate::{Inventory, ItemCatalog, ItemStack, key, owner, schema};
use std::collections::{BTreeMap, BTreeSet};
use torse_sim::action::{ActionTarget, ActionView};
use torse_sim::{OrderingKey, Phase, Transaction};
use torse_types::{EntityId, RejectCode, TransactionId};
use torse_world::{ComponentEdit, ComponentFields, component_size};

pub const MAX_OPERATIONS: usize = 64;
/// Logical plan work, including borrowed inputs, owned drafts, output and
/// provisional extractions. Shared host reservations remain independently enforced.
pub const MAX_PLAN_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) fn charge_plan(used: &mut u64, bytes: u64) -> Result<(), RejectCode> {
    *used = used.checked_add(bytes).ok_or(RejectCode::QuotaExceeded)?;
    if *used > MAX_PLAN_BYTES {
        return Err(RejectCode::QuotaExceeded);
    }
    Ok(())
}

fn stack_bytes(stack: &ItemStack) -> Result<u64, RejectCode> {
    Ok(
        component_size(&stack.data).map_err(|_| RejectCode::QuotaExceeded)?
            + stack.item.namespace().len() as u64
            + stack.item.path().len() as u64
            + 256,
    )
}

/// One operation over exactly observed containers. External ingress requires
/// its own trusted permission; transfer alone never grants item creation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InventoryOperation {
    Transfer(crate::Transfer),
    Insert {
        target: EntityId,
        slot: usize,
        stack: ItemStack,
    },
    Extract {
        target: EntityId,
        slot: usize,
        stack: ItemStack,
    },
    Consume {
        target: EntityId,
        slot: usize,
        stack: ItemStack,
    },
    Swap {
        left: EntityId,
        left_slot: usize,
        right: EntityId,
        right_slot: usize,
    },
}

/// No authoritative state changes until the returned transaction is committed.
/// Extracted stacks are proposed results, usable only after that commit succeeds.
pub struct InventoryPlan {
    transaction: Transaction,
    extracted: Vec<ItemStack>,
}
impl InventoryPlan {
    pub fn transaction(&self) -> &Transaction {
        &self.transaction
    }
    pub fn extracted(&self) -> &[ItemStack] {
        &self.extracted
    }
    pub fn into_parts(self) -> (Transaction, Vec<ItemStack>) {
        (self.transaction, self.extracted)
    }
}

/// Validate all operations against exact expected components, then produce one
/// replacement per touched container. Repeated operations share private draft
/// state; partial failure discards those drafts without touching the world.
pub fn plan_inventory(
    view: &ActionView<'_>,
    catalog: &ItemCatalog,
    operations: &[InventoryOperation],
    expected: &BTreeMap<EntityId, ComponentFields>,
    transaction: TransactionId,
    ordering: OrderingKey,
) -> Result<InventoryPlan, RejectCode> {
    if operations.is_empty() || operations.len() > MAX_OPERATIONS {
        return Err(RejectCode::QuotaExceeded);
    }
    if !matches!(ordering.phase, Phase::Interaction | Phase::Simulation) {
        return Err(RejectCode::InvalidData);
    }
    let mut targets = BTreeSet::new();
    let mut budget = 4096;
    for operation in operations {
        if let InventoryOperation::Insert { stack, .. }
        | InventoryOperation::Extract { stack, .. }
        | InventoryOperation::Consume { stack, .. } = operation
        {
            catalog.validate_stack(stack)?;
            charge_plan(&mut budget, stack_bytes(stack)? * 4)?;
        }
        let permission = match operation {
            InventoryOperation::Transfer(transfer) => {
                targets.insert(transfer.source);
                targets.insert(transfer.destination);
                "transfer"
            }
            InventoryOperation::Insert { target, .. } => {
                targets.insert(*target);
                "insert"
            }
            InventoryOperation::Extract { target, .. } => {
                targets.insert(*target);
                "extract"
            }
            InventoryOperation::Consume { target, .. } => {
                targets.insert(*target);
                "consume"
            }
            InventoryOperation::Swap { left, right, .. } => {
                targets.insert(*left);
                targets.insert(*right);
                "swap"
            }
        };
        if !view
            .actor()
            .permits(&key(&format!("inventory:permission/{permission}")))
        {
            return Err(RejectCode::PermissionDenied);
        }
    }
    if targets.len() > MAX_SLOTS_TOUCHED || expected.len() != targets.len() {
        return Err(RejectCode::InvalidData);
    }
    // Preflight every observation before decoding or cloning any container.
    for target in &targets {
        let target = *target;
        if !view.actor().allows(ActionTarget::Entity(target)) {
            return Err(RejectCode::PermissionDenied);
        }
        let observed = view.component(target, &schema())?;
        let expected = expected.get(&target).ok_or(RejectCode::InvalidData)?;
        let bytes = component_size(expected).map_err(|_| RejectCode::QuotaExceeded)?;
        component_size(observed).map_err(|_| RejectCode::QuotaExceeded)?;
        // Expected input, decoded draft, previous observation and final output.
        charge_plan(&mut budget, bytes * 4 + 256)?;
        if observed != expected {
            return Err(RejectCode::Conflict);
        }
    }
    let mut drafts = BTreeMap::new();
    for target in targets {
        let observed = view.component(target, &schema())?;
        drafts.insert(
            target,
            (observed, Inventory::from_fields(observed, catalog)?),
        );
    }
    let mut extracted = Vec::new();
    for operation in operations {
        match operation {
            InventoryOperation::Transfer(transfer) => {
                let crate::Transfer {
                    source,
                    source_slot,
                    destination,
                    destination_slot,
                    count,
                } = *transfer;
                if count == 0 || (source == destination && source_slot == destination_slot) {
                    return Err(RejectCode::InvalidData);
                }
                let stack = drafts[&source]
                    .1
                    .slots()
                    .get(source_slot)
                    .ok_or(RejectCode::InvalidData)?
                    .as_ref()
                    .ok_or(RejectCode::Conflict)?;
                catalog.validate_stack(stack)?;
                charge_plan(&mut budget, stack_bytes(stack)? * 3)?;
                let stack = stack.clone();
                if stack.count < count {
                    return Err(RejectCode::Conflict);
                }
                let existing = drafts[&destination]
                    .1
                    .slots()
                    .get(destination_slot)
                    .ok_or(RejectCode::InvalidData)?;
                let old = match existing {
                    None => 0,
                    Some(other) if catalog.can_merge(other, &stack)? => other.count,
                    Some(_) => return Err(RejectCode::Conflict),
                };
                let limit = catalog.limit(&stack.item)?;
                let new = old
                    .checked_add(count)
                    .filter(|total| *total <= limit)
                    .ok_or(RejectCode::Conflict)?;
                drafts.get_mut(&source).expect("observed source").1.set(
                    source_slot,
                    (stack.count > count).then(|| stack.with_count(stack.count - count)),
                    catalog,
                )?;
                drafts
                    .get_mut(&destination)
                    .expect("observed destination")
                    .1
                    .set(destination_slot, Some(stack.with_count(new)), catalog)?;
            }
            InventoryOperation::Insert {
                target,
                slot,
                stack,
            } => {
                if stack.count == 0 {
                    return Err(RejectCode::InvalidData);
                }
                let limit = catalog.limit(&stack.item)?;
                let inventory = &mut drafts.get_mut(target).expect("observed target").1;
                let count = match inventory
                    .slots()
                    .get(*slot)
                    .ok_or(RejectCode::InvalidData)?
                {
                    None => 0,
                    Some(old) if catalog.can_merge(old, stack)? => old.count,
                    _ => return Err(RejectCode::Conflict),
                };
                let count = count
                    .checked_add(stack.count)
                    .filter(|count| *count <= limit)
                    .ok_or(RejectCode::Conflict)?;
                inventory.set(*slot, Some(stack.with_count(count)), catalog)?;
            }
            InventoryOperation::Extract {
                target,
                slot,
                stack,
            }
            | InventoryOperation::Consume {
                target,
                slot,
                stack,
            } => {
                if stack.count == 0 {
                    return Err(RejectCode::InvalidData);
                }
                catalog.limit(&stack.item)?;
                let inventory = &mut drafts.get_mut(target).expect("observed target").1;
                let old = inventory
                    .slots()
                    .get(*slot)
                    .ok_or(RejectCode::InvalidData)?
                    .as_ref()
                    .ok_or(RejectCode::Conflict)?;
                if old.item != stack.item || old.data != stack.data || old.count < stack.count {
                    return Err(RejectCode::Conflict);
                }
                let remaining = old.count - stack.count;
                inventory.set(
                    *slot,
                    (remaining > 0).then(|| stack.with_count(remaining)),
                    catalog,
                )?;
                if matches!(operation, InventoryOperation::Extract { .. }) {
                    extracted.push(stack.clone());
                }
            }
            InventoryOperation::Swap {
                left,
                left_slot,
                right,
                right_slot,
            } => {
                if left == right && left_slot == right_slot {
                    return Err(RejectCode::InvalidData);
                }
                let a = drafts[left]
                    .1
                    .slots()
                    .get(*left_slot)
                    .ok_or(RejectCode::InvalidData)?;
                let b = drafts[right]
                    .1
                    .slots()
                    .get(*right_slot)
                    .ok_or(RejectCode::InvalidData)?;
                for stack in [a, b].into_iter().flatten() {
                    catalog.validate_stack(stack)?;
                    charge_plan(&mut budget, stack_bytes(stack)?)?;
                }
                let (a, b) = (a.clone(), b.clone());
                // Vacate both positions first. A same-container swap must not
                // fail because its first write temporarily duplicates a larger
                // instance and exceeds the final container's aggregate bound.
                drafts
                    .get_mut(left)
                    .expect("observed left")
                    .1
                    .set(*left_slot, None, catalog)?;
                drafts
                    .get_mut(right)
                    .expect("observed right")
                    .1
                    .set(*right_slot, None, catalog)?;
                drafts
                    .get_mut(left)
                    .expect("observed left")
                    .1
                    .set(*left_slot, b, catalog)?;
                drafts
                    .get_mut(right)
                    .expect("observed right")
                    .1
                    .set(*right_slot, a, catalog)?;
            }
        }
    }
    let mut transaction = Transaction::new(transaction, owner(), ordering);
    for (id, (previous, inventory)) in drafts {
        let fields = inventory.fields();
        component_size(&fields).map_err(|_| RejectCode::QuotaExceeded)?;
        if &fields != previous {
            transaction.components.push(ComponentEdit {
                id,
                schema: schema(),
                previous: Some(previous.clone()),
                fields: Some(fields),
            });
        }
    }
    Ok(InventoryPlan {
        transaction,
        extracted,
    })
}
const MAX_SLOTS_TOUCHED: usize = 64;
