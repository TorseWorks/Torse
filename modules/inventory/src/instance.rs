//! Versioned structural bounds and trusted, declarative per-item validation.
use crate::{ItemCatalog, ItemStack};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use torse_types::{ContentKey, RejectCode};
use torse_world::{ComponentFields, ComponentValue};

pub const DATA_FORMAT_VERSION: u32 = 1;
pub const MAX_INSTANCE_BYTES: u64 = 4096;
pub const MAX_INSTANCE_DEPTH: u32 = 4;
pub const MAX_INSTANCE_NODES: usize = 256;
pub const MAX_INSTANCE_COLLECTION: usize = 64;
pub const MAX_INSTANCE_TEXT_BYTES: usize = 1024;
pub const MAX_INSTANCE_KEY_BYTES: usize = 128;

/// A recognized unsigned attribute. Unknown attributes remain opaque and exact.
#[derive(Clone, Debug)]
pub struct U32Rule {
    pub minimum: u32,
    pub maximum: u32,
    pub required: bool,
}

/// Authority-supplied policy, frozen before binding handlers. No implicit defaults,
/// coercion or attribute removal is performed. Revision changes authority identity.
#[derive(Clone, Debug)]
pub struct ItemPolicy {
    pub revision: u32,
    pub mergeable: bool,
    pub unsigned: BTreeMap<ContentKey, U32Rule>,
}
impl Default for ItemPolicy {
    fn default() -> Self {
        Self {
            revision: 1,
            mergeable: true,
            unsigned: BTreeMap::new(),
        }
    }
}
impl ItemPolicy {
    pub(crate) fn digest(&self, digest: &mut Sha256) {
        digest.update(self.revision.to_le_bytes());
        digest.update([u8::from(self.mergeable)]);
        digest.update((self.unsigned.len() as u64).to_le_bytes());
        for (key, rule) in &self.unsigned {
            digest.update((key.to_string().len() as u64).to_le_bytes());
            digest.update(key.to_string().as_bytes());
            digest.update(rule.minimum.to_le_bytes());
            digest.update(rule.maximum.to_le_bytes());
            digest.update([u8::from(rule.required)]);
        }
    }
}
impl ItemCatalog {
    /// Bind owner rules before world assembly so every publication, restore and
    /// authorized replica decode enforces the same nested inventory contract.
    pub fn bind_schemas(
        &self,
        schemas: &mut torse_world::ComponentSchemas,
    ) -> Result<(), torse_world::ComponentError> {
        schemas.register_validator(
            &crate::owner(),
            &crate::schema(),
            std::sync::Arc::new(self.clone()),
        )
    }
    /// Replace one item's trusted policy before binding compatibility or handlers.
    pub fn with_policy(
        mut self,
        item: &ContentKey,
        policy: ItemPolicy,
    ) -> Result<Self, RejectCode> {
        self.limit(item)?;
        if policy.revision == 0
            || policy.unsigned.len() > MAX_INSTANCE_COLLECTION
            || policy.unsigned.iter().any(|(key, rule)| {
                key.to_string().len() > MAX_INSTANCE_KEY_BYTES || rule.minimum > rule.maximum
            })
        {
            return Err(RejectCode::InvalidData);
        }
        self.policies.insert(item.clone(), policy);
        Ok(self)
    }
    pub fn validate_stack(&self, stack: &ItemStack) -> Result<(), RejectCode> {
        self.validate_parts(&stack.item, stack.count, &stack.data)
    }
    pub(crate) fn validate_parts(
        &self,
        item: &ContentKey,
        count: u32,
        data: &ComponentFields,
    ) -> Result<(), RejectCode> {
        if count == 0 || count > self.limit(item)? {
            return Err(RejectCode::InvalidData);
        }
        validate_data(data)?;
        for (key, rule) in &self.policies[item].unsigned {
            match data.get(key) {
                None if !rule.required => {}
                Some(ComponentValue::U32(value))
                    if (rule.minimum..=rule.maximum).contains(value) => {}
                _ => return Err(RejectCode::InvalidData),
            }
        }
        Ok(())
    }
    /// Equality is typed and conservative: record keys are sorted, lists ordered,
    /// and strings exact. Counts do not participate in instance identity.
    pub fn can_merge(&self, left: &ItemStack, right: &ItemStack) -> Result<bool, RejectCode> {
        self.validate_stack(left)?;
        self.validate_stack(right)?;
        Ok(left.item == right.item
            && self.policies[&left.item].mergeable
            && left.data == right.data)
    }
}

impl torse_world::ComponentValidator for ItemCatalog {
    fn validate(&self, fields: &ComponentFields) -> Result<(), torse_world::ComponentError> {
        crate::Inventory::from_fields(fields, self)
            .map(|_| ())
            .map_err(|error| match error {
                RejectCode::QuotaExceeded => torse_world::ComponentError::BudgetExceeded,
                _ => torse_world::ComponentError::InvalidConfiguration {
                    id: crate::schema(),
                },
            })
    }
    fn rules_digest(&self) -> torse_types::Digest {
        self.rules_digest()
    }
}

pub(crate) fn digest_contract(digest: &mut Sha256) {
    digest.update(b"typed-exact-record-order-list-order/no-defaults/schema2/action2\0");
    for limit in [
        DATA_FORMAT_VERSION as u64,
        MAX_INSTANCE_BYTES,
        MAX_INSTANCE_DEPTH as u64,
        MAX_INSTANCE_NODES as u64,
        MAX_INSTANCE_COLLECTION as u64,
        MAX_INSTANCE_TEXT_BYTES as u64,
        MAX_INSTANCE_KEY_BYTES as u64,
        crate::MAX_PLAN_BYTES,
    ] {
        digest.update(limit.to_le_bytes());
    }
}

/// Check bounds before cloning, equality or recursive policy work. Accounting
/// follows component_size for the payload record, excluding its stack envelope.
pub fn validate_data(fields: &ComponentFields) -> Result<u64, RejectCode> {
    data_stats(fields).map(|(bytes, _)| bytes)
}

fn data_stats(fields: &ComponentFields) -> Result<(u64, usize), RejectCode> {
    fn record(
        fields: &ComponentFields,
        depth: u32,
        nodes: &mut usize,
        bytes: &mut u64,
    ) -> Result<(), RejectCode> {
        if fields.len() > MAX_INSTANCE_COLLECTION {
            return Err(RejectCode::QuotaExceeded);
        }
        for (key, value) in fields {
            let length = key.namespace().len() + key.path().len() + 1;
            if length > MAX_INSTANCE_KEY_BYTES {
                return Err(RejectCode::QuotaExceeded);
            }
            *bytes += length as u64 + 8;
            visit(value, depth, nodes, bytes)?;
        }
        Ok(())
    }
    fn visit(
        value: &ComponentValue,
        depth: u32,
        nodes: &mut usize,
        bytes: &mut u64,
    ) -> Result<(), RejectCode> {
        if depth > MAX_INSTANCE_DEPTH || *nodes >= MAX_INSTANCE_NODES {
            return Err(RejectCode::QuotaExceeded);
        }
        *nodes += 1;
        *bytes += 9;
        match value {
            ComponentValue::Text(text) => {
                if text.len() > MAX_INSTANCE_TEXT_BYTES {
                    return Err(RejectCode::QuotaExceeded);
                }
                *bytes += text.len() as u64;
            }
            ComponentValue::Content(key) => {
                let length = key.namespace().len() + key.path().len() + 1;
                if length > MAX_INSTANCE_KEY_BYTES {
                    return Err(RejectCode::QuotaExceeded);
                }
                *bytes += length as u64 + 8;
            }
            ComponentValue::Entity(_) => *bytes += 16,
            ComponentValue::Record(fields) => record(fields, depth + 1, nodes, bytes)?,
            ComponentValue::List(values) => {
                if values.len() > MAX_INSTANCE_COLLECTION {
                    return Err(RejectCode::QuotaExceeded);
                }
                for value in values {
                    visit(value, depth + 1, nodes, bytes)?;
                }
            }
            _ => {}
        }
        if *bytes > MAX_INSTANCE_BYTES {
            return Err(RejectCode::QuotaExceeded);
        }
        Ok(())
    }
    let mut bytes = 8;
    let mut nodes = 0;
    record(fields, 0, &mut nodes, &mut bytes)?;
    Ok((bytes, nodes))
}

/// Size the projected container by reference before allocating encoded drafts.
pub(crate) fn validate_container<'a>(
    slots: impl Iterator<Item = Option<&'a ItemStack>>,
) -> Result<(), RejectCode> {
    fn key_bytes(value: &str) -> u64 {
        value.len() as u64 + 8
    }
    let mut bytes = 8 + key_bytes("inventory:field/slots") + 9;
    let mut nodes = 1;
    for stack in slots {
        bytes += 9;
        nodes += 1;
        if let Some(stack) = stack {
            let (data_bytes, data_nodes) = data_stats(&stack.data)?;
            bytes += key_bytes("inventory:field/item")
                + 9
                + stack.item.to_string().len() as u64
                + 8
                + key_bytes("inventory:field/count")
                + 9
                + key_bytes("inventory:field/data_format")
                + 9
                + key_bytes("inventory:field/data")
                + 9
                + data_bytes
                - 8;
            nodes += 4 + data_nodes;
        }
        if bytes > torse_world::MAX_COMPONENT_BYTES || nodes > torse_world::MAX_COMPONENT_NODES {
            return Err(RejectCode::QuotaExceeded);
        }
    }
    Ok(())
}
