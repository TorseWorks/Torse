//! Item-port planning; both inventory endpoints publish in one transaction.

use crate::{Inventory, ItemCatalog, key, owner, schema};
use std::sync::Arc;
use torse_sim::action::{
    ActionHandler, ActionOutput, ActionRequest, ActionTarget, ActionView, NativeHandler,
};
use torse_sim::{OrderingKey, Transaction};
use torse_types::{EntityId, RejectCode, TransactionId};
use torse_world::{ComponentFields, ComponentValue, component_size};

/// Bind query and transfer handlers to one immutable, game-supplied item catalog.
pub fn handlers(catalog: ItemCatalog) -> Vec<NativeHandler> {
    let catalog = Arc::new(catalog);
    [false, true]
        .into_iter()
        .map(|transfer| NativeHandler {
            handler: key(if transfer {
                "inventory:handler/transfer"
            } else {
                "inventory:handler/inspect"
            }),
            owner: owner(),
            implementation: Box::new(PortHandler {
                catalog: catalog.clone(),
                transfer,
            }),
        })
        .collect()
}

struct PortHandler {
    catalog: Arc<ItemCatalog>,
    transfer: bool,
}

fn read<'a>(view: &'a ActionView<'_>, id: EntityId) -> Result<&'a ComponentFields, RejectCode> {
    view.component(id, &schema())
}

fn number(fields: &ComponentFields, name: &str) -> Result<u32, RejectCode> {
    match fields.get(&key(&format!("inventory:field/{name}"))) {
        Some(ComponentValue::U32(value)) => Ok(*value),
        _ => Err(RejectCode::InvalidData),
    }
}

/// Typed item-port operation shared by interactive actions and scheduled machines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Transfer {
    pub source: EntityId,
    pub source_slot: usize,
    pub destination: EntityId,
    pub destination_slot: usize,
    pub count: u32,
}

/// Sequential private drafts for a bounded batch, including shared endpoints.
/// Final components and conservation publish together through one owner transaction.
pub fn plan_transfers(
    view: &ActionView<'_>,
    catalog: &ItemCatalog,
    transfers: &[Transfer],
    transaction: TransactionId,
    ordering: OrderingKey,
) -> Result<Transaction, RejectCode> {
    let mut expected = std::collections::BTreeMap::new();
    if transfers.is_empty() || transfers.len() > crate::MAX_OPERATIONS {
        return Err(RejectCode::QuotaExceeded);
    }
    if !view.actor().permits(&key("inventory:permission/transfer")) {
        return Err(RejectCode::PermissionDenied);
    }
    let mut budget = 4096;
    for transfer in transfers {
        for entity in [transfer.source, transfer.destination] {
            if !view.actor().allows(ActionTarget::Entity(entity)) {
                return Err(RejectCode::PermissionDenied);
            }
            if let std::collections::btree_map::Entry::Vacant(entry) = expected.entry(entity) {
                let fields = view.component(entity, &schema())?;
                let bytes = component_size(fields).map_err(|_| RejectCode::QuotaExceeded)?;
                crate::operations::charge_plan(&mut budget, bytes * 4 + 256)?;
                entry.insert(fields.clone());
            }
        }
    }
    crate::plan_inventory(
        view,
        catalog,
        &transfers
            .iter()
            .copied()
            .map(crate::InventoryOperation::Transfer)
            .collect::<Vec<_>>(),
        &expected,
        transaction,
        ordering,
    )
    .map(|plan| plan.into_parts().0)
}

impl ActionHandler for PortHandler {
    fn plan(
        &self,
        view: &ActionView<'_>,
        request: &ActionRequest,
    ) -> Result<ActionOutput, RejectCode> {
        let ActionTarget::Entity(source) = request.target else {
            return Err(RejectCode::InvalidData);
        };
        if !self.transfer {
            return Ok(ActionOutput::Query(
                Inventory::from_fields(read(view, source)?, &self.catalog)?.fields(),
            ));
        }
        let Some(ComponentValue::Entity(destination)) =
            request.parameters.get(&key("inventory:field/destination"))
        else {
            return Err(RejectCode::InvalidData);
        };
        plan_transfer(
            view,
            &self.catalog,
            Transfer {
                source,
                destination: *destination,
                source_slot: number(&request.parameters, "source_slot")? as usize,
                destination_slot: number(&request.parameters, "destination_slot")? as usize,
                count: number(&request.parameters, "count")?,
            },
            request.transaction,
            request.ordering,
        )
        .map(|tx| ActionOutput::Mutation(Box::new(tx)))
    }
}

/// Plan a bounded transfer at the interaction or simulation phase.
///
/// A native machine uses a host-issued view and typed arguments directly; it
/// does not format a command or bypass permission, capacity, or commit checks.
pub fn plan_transfer(
    view: &ActionView<'_>,
    catalog: &ItemCatalog,
    transfer: Transfer,
    transaction: TransactionId,
    ordering: OrderingKey,
) -> Result<Transaction, RejectCode> {
    plan_transfers(view, catalog, &[transfer], transaction, ordering)
}
