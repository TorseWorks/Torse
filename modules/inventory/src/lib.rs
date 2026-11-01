//! Optional inventory rules and protected native item-port actions.
//!
//! The kernel only sees schema-owned component replacements. This module owns
//! item identities, stack limits, slot compatibility, and conservation. A plan
//! carries exact source components and publishes through the ordinary world
//! coordinator; it never mutates a container while an action is being handled.

#![forbid(unsafe_code)]

mod content;
mod instance;
mod operations;
mod transfer;

use std::collections::BTreeMap;
use torse_types::{Compatibility, ContentKey, Digest, RejectCode};
use torse_world::{ComponentFields, ComponentValue};

pub use content::{descriptors, owner, register_content, schema};
pub use instance::validate_data as validate_instance_data;
pub use instance::{
    DATA_FORMAT_VERSION, ItemPolicy, MAX_INSTANCE_BYTES, MAX_INSTANCE_COLLECTION,
    MAX_INSTANCE_DEPTH, MAX_INSTANCE_KEY_BYTES, MAX_INSTANCE_NODES, MAX_INSTANCE_TEXT_BYTES,
    U32Rule,
};
pub use operations::{
    InventoryOperation, InventoryPlan, MAX_OPERATIONS, MAX_PLAN_BYTES, plan_inventory,
};
pub use transfer::{Transfer, handlers, plan_transfer, plan_transfers};

/// Bounded slot count of the first native container implementation.
pub const MAX_SLOTS: usize = 64;

fn key(value: &str) -> ContentKey {
    ContentKey::parse(value).expect("inventory key")
}

/// Game-supplied item limits, frozen before handlers are bound.
#[derive(Clone, Debug)]
pub struct ItemCatalog {
    limits: BTreeMap<ContentKey, u32>,
    policies: BTreeMap<ContentKey, ItemPolicy>,
}

impl ItemCatalog {
    /// Canonical identity of item membership and stack limits, independent of input order.
    pub fn rules_digest(&self) -> Digest {
        use sha2::{Digest as _, Sha256};
        let mut digest = Sha256::new();
        digest.update(b"torse:inventory/item-rules/v2\0");
        instance::digest_contract(&mut digest);
        digest.update((self.limits.len() as u64).to_le_bytes());
        for (item, limit) in &self.limits {
            let key = item.to_string();
            digest.update((key.len() as u64).to_le_bytes());
            digest.update(key.as_bytes());
            digest.update(limit.to_le_bytes());
            self.policies[item].digest(&mut digest);
        }
        Digest(digest.finalize().into())
    }
    /// Apply once before freezing schemas and building authority or recovery.
    /// Presentation identity is unaffected; unbound legacy rule identities differ.
    pub fn bind_compatibility(&self, mut base: Compatibility) -> Compatibility {
        use sha2::{Digest as _, Sha256};
        let mut digest = Sha256::new();
        digest.update(b"torse:inventory/compatibility/v1\0");
        digest.update(base.rules_digest.0);
        digest.update(self.rules_digest().0);
        base.rules_digest = Digest(digest.finalize().into());
        base
    }
    /// Reject duplicate items and zero stack capacities.
    pub fn new(items: impl IntoIterator<Item = (ContentKey, u32)>) -> Result<Self, RejectCode> {
        let mut limits = BTreeMap::new();
        for (item, limit) in items {
            if limit == 0 || limits.insert(item, limit).is_some() {
                return Err(RejectCode::InvalidData);
            }
        }
        let policies = limits
            .keys()
            .cloned()
            .map(|item| (item, ItemPolicy::default()))
            .collect();
        Ok(Self { limits, policies })
    }
    fn limit(&self, item: &ContentKey) -> Result<u32, RejectCode> {
        self.limits
            .get(item)
            .copied()
            .ok_or(RejectCode::InvalidData)
    }
}

/// Stable item identity and positive count; no runtime item numbering is saved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ItemStack {
    pub item: ContentKey,
    pub count: u32,
    pub data: ComponentFields,
}

impl ItemStack {
    pub fn new(item: ContentKey, count: u32) -> Self {
        Self {
            item,
            count,
            data: ComponentFields::new(),
        }
    }
    pub fn with_count(&self, count: u32) -> Self {
        Self {
            item: self.item.clone(),
            count,
            data: self.data.clone(),
        }
    }
}

/// Typed module view of a protected inventory component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Inventory {
    slots: Vec<Option<ItemStack>>,
}

impl Inventory {
    /// Create a fixed-capacity container with empty slots.
    pub fn new(slots: usize) -> Result<Self, RejectCode> {
        if slots == 0 || slots > MAX_SLOTS {
            return Err(RejectCode::InvalidData);
        }
        Ok(Self {
            slots: vec![None; slots],
        })
    }
    /// Read slots without granting mutation authority.
    pub fn slots(&self) -> &[Option<ItemStack>] {
        &self.slots
    }
    /// Set up a validated slot; authoritative transfers use the action handler.
    pub fn set(
        &mut self,
        slot: usize,
        stack: Option<ItemStack>,
        catalog: &ItemCatalog,
    ) -> Result<(), RejectCode> {
        if let Some(stack) = &stack {
            catalog.validate_stack(stack)?;
        }
        self.slots.get(slot).ok_or(RejectCode::InvalidData)?;
        instance::validate_container(self.slots.iter().enumerate().map(|(index, old)| {
            if index == slot {
                stack.as_ref()
            } else {
                old.as_ref()
            }
        }))?;
        self.slots[slot] = stack;
        Ok(())
    }
    /// Convert to stable, schema-compatible component fields.
    pub fn fields(&self) -> ComponentFields {
        [(
            key("inventory:field/slots"),
            ComponentValue::List(
                self.slots
                    .iter()
                    .map(|stack| {
                        let fields = match stack {
                            None => BTreeMap::new(),
                            Some(stack) => [
                                (
                                    key("inventory:field/item"),
                                    ComponentValue::Content(stack.item.clone()),
                                ),
                                (
                                    key("inventory:field/count"),
                                    ComponentValue::U32(stack.count),
                                ),
                                (
                                    key("inventory:field/data_format"),
                                    ComponentValue::U32(DATA_FORMAT_VERSION),
                                ),
                                (
                                    key("inventory:field/data"),
                                    ComponentValue::Record(stack.data.clone()),
                                ),
                            ]
                            .into_iter()
                            .collect(),
                        };
                        ComponentValue::Record(fields)
                    })
                    .collect(),
            ),
        )]
        .into_iter()
        .collect()
    }
    /// Validate nested module rules beyond the generic list/record schema.
    pub fn from_fields(
        fields: &ComponentFields,
        catalog: &ItemCatalog,
    ) -> Result<Self, RejectCode> {
        torse_world::component_size(fields).map_err(|_| RejectCode::QuotaExceeded)?;
        if fields.len() != 1 {
            return Err(RejectCode::InvalidData);
        }
        let Some(ComponentValue::List(slots)) = fields.get(&key("inventory:field/slots")) else {
            return Err(RejectCode::InvalidData);
        };
        let mut inventory = Self::new(slots.len())?;
        for (index, slot) in slots.iter().enumerate() {
            let ComponentValue::Record(fields) = slot else {
                return Err(RejectCode::InvalidData);
            };
            if fields.is_empty() {
                continue;
            }
            if fields.len() != 2 && fields.len() != 4 {
                return Err(RejectCode::InvalidData);
            }
            let Some(ComponentValue::Content(item)) = fields.get(&key("inventory:field/item"))
            else {
                return Err(RejectCode::InvalidData);
            };
            let Some(ComponentValue::U32(count)) = fields.get(&key("inventory:field/count")) else {
                return Err(RejectCode::InvalidData);
            };
            let empty = ComponentFields::new();
            let data = if fields.len() == 2 {
                &empty
            } else {
                if fields.get(&key("inventory:field/data_format"))
                    != Some(&ComponentValue::U32(DATA_FORMAT_VERSION))
                {
                    return Err(RejectCode::InvalidData);
                }
                let Some(ComponentValue::Record(data)) = fields.get(&key("inventory:field/data"))
                else {
                    return Err(RejectCode::InvalidData);
                };
                data
            };
            catalog.validate_parts(item, *count, data)?;
            inventory.slots[index] = Some(ItemStack {
                item: item.clone(),
                count: *count,
                data: data.clone(),
            });
        }
        instance::validate_container(inventory.slots.iter().map(Option::as_ref))?;
        Ok(inventory)
    }
}
