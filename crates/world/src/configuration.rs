//! Trusted startup configuration, canonical rule identity and freeze boundary.
use crate::{
    ComponentError, ComponentFields, ComponentSchemas, ComponentSet, ComponentValue, component_size,
};
use sha2::{Digest as ShaDigest, Sha256};
use std::collections::BTreeSet;
use torse_content::CompiledContent;
use torse_types::{Compatibility, ContentKey, Digest};

/// One owner-defined durability choice. Fixed policies cannot be overridden.
#[derive(Clone, Debug)]
pub struct PersistenceSelection {
    pub owner: ContentKey,
    pub schema: ContentKey,
    pub persist: bool,
}
/// Initial fields for one component explicitly selected by a block declaration.
#[derive(Clone, Debug)]
pub struct BlockDefault {
    pub owner: ContentKey,
    pub block: ContentKey,
    pub schema: ContentKey,
    pub fields: ComponentFields,
}
/// Order-independent startup input. Duplicate bindings are rejected, not overwritten.
#[derive(Clone, Debug, Default)]
pub struct ComponentConfiguration {
    pub persistence: Vec<PersistenceSelection>,
    pub block_defaults: Vec<BlockDefault>,
}

impl ComponentSchemas {
    /// Validate the entire configuration before returning any state, including
    /// required fields of every declared block component. The base compatibility
    /// identifies other host rules; effective choices are folded into its rules digest.
    pub fn assemble(
        content: &CompiledContent,
        configuration: ComponentConfiguration,
        base: Compatibility,
    ) -> Result<(Self, Compatibility), ComponentError> {
        if configuration
            .persistence
            .len()
            .saturating_add(configuration.block_defaults.len())
            > 4096
        {
            return Err(ComponentError::BudgetExceeded);
        }
        let mut schemas = Self::build(content);
        let mut seen = BTreeSet::new();
        for selection in configuration.persistence {
            if !seen.insert(selection.schema.clone()) {
                return Err(ComponentError::InvalidConfiguration {
                    id: selection.schema,
                });
            }
            schemas.resolve_persistence(&selection.owner, &selection.schema, selection.persist)?;
        }
        let mut seen = BTreeSet::new();
        let mut bytes = 0u64;
        for default in configuration.block_defaults {
            bytes += component_size(&default.fields)?;
            if bytes > 8_388_608 {
                return Err(ComponentError::BudgetExceeded);
            }
            if !seen.insert((default.block.clone(), default.schema.clone())) {
                return Err(ComponentError::InvalidConfiguration { id: default.schema });
            }
            schemas.set_block_default(
                &default.owner,
                default.block,
                default.schema,
                default.fields,
            )?;
        }
        let empty = ComponentFields::new();
        for (block, declared) in &schemas.blocks {
            for schema in declared {
                ComponentSet::validate(
                    &schemas,
                    schema,
                    schemas.block_default(block, schema).unwrap_or(&empty),
                )?;
            }
        }
        schemas.bind_compatibility(base)
    }

    /// Freeze startup state and derive the compatibility used by authority,
    /// persistence and network admission. Calling again with that returned
    /// compatibility is idempotent; a different binding is rejected.
    /// Legacy incremental registration remains supported, with placement-time
    /// required-field checks. `assemble` validates all defaults at startup.
    pub fn bind_compatibility(
        mut self,
        base: Compatibility,
    ) -> Result<(Self, Compatibility), ComponentError> {
        if let Some(expected) = self.compatibility {
            if expected != base {
                return Err(ComponentError::ConfigurationMismatch);
            }
            return Ok((self, base));
        }
        let entries = self.persistence.len().saturating_add(
            self.block_defaults
                .values()
                .map(|entries| entries.len())
                .sum::<usize>(),
        );
        if entries > 4096 {
            return Err(ComponentError::BudgetExceeded);
        }
        let mut bytes = 0u64;
        for fields in self
            .block_defaults
            .values()
            .flat_map(|entries| entries.values())
        {
            bytes += component_size(fields)?;
            if bytes > 8_388_608 {
                return Err(ComponentError::BudgetExceeded);
            }
        }
        let persistence: Vec<_> = self
            .persistence
            .iter()
            .filter(|(_, persist)| !**persist)
            .collect();
        let defaults: Vec<_> = self
            .block_defaults
            .iter()
            .flat_map(|(block, components)| {
                components
                    .iter()
                    .filter(|(_, fields)| !fields.is_empty())
                    .map(move |(schema, fields)| (block, schema, fields))
            })
            .collect();
        let mut compatibility = base;
        // Preserve old worlds when every effective choice is the legacy default.
        if !persistence.is_empty() || !defaults.is_empty() {
            let mut hash = Sha256::new();
            hash.update(b"torse/component-configuration/v1\0");
            hash.update(base.rules_digest.0);
            count(&mut hash, persistence.len());
            for (schema, _) in persistence {
                key(&mut hash, schema);
            }
            count(&mut hash, defaults.len());
            for (block, schema, fields) in defaults {
                key(&mut hash, block);
                key(&mut hash, schema);
                record(&mut hash, fields);
            }
            compatibility.rules_digest = Digest(hash.finalize().into());
        }
        self.compatibility = Some(compatibility);
        Ok((self, compatibility))
    }

    /// Reject recovery under different effective startup choices. Unconfigured
    /// legacy schemas accept old metadata; configured schemas must be frozen first.
    pub fn validate_compatibility(
        &self,
        compatibility: Compatibility,
    ) -> Result<(), ComponentError> {
        match self.compatibility {
            Some(expected) if expected == compatibility => Ok(()),
            None if self.persistence.is_empty() && self.block_defaults.is_empty() => Ok(()),
            _ => Err(ComponentError::ConfigurationMismatch),
        }
    }

    /// Storage faces the host has opened as known empty space.
    pub const fn exterior_policy(&self) -> crate::ExteriorPolicy {
        self.exterior
    }

    /// Fold a non-default exterior policy into the frozen rules digest.
    ///
    /// Schemas must already be bound. The default policy leaves the digest
    /// unchanged. A second non-default policy is rejected so recovery cannot
    /// hash the same choice twice.
    pub fn bind_exterior_policy(
        &mut self,
        policy: crate::ExteriorPolicy,
    ) -> Result<torse_types::Compatibility, ComponentError> {
        if self.exterior != crate::ExteriorPolicy::default() {
            return Err(ComponentError::ConfigurationFrozen);
        }
        let current = self
            .compatibility
            .ok_or(ComponentError::ConfigurationMismatch)?;
        if policy == crate::ExteriorPolicy::default() {
            return Ok(current);
        }
        let next = crate::bind_exterior(current, policy);
        self.compatibility = Some(next);
        self.exterior = policy;
        Ok(next)
    }
}
fn count(hash: &mut Sha256, count: usize) {
    hash.update((count as u64).to_le_bytes());
}
fn key(hash: &mut Sha256, key: &ContentKey) {
    count(hash, key.namespace().len());
    hash.update(key.namespace().as_bytes());
    count(hash, key.path().len());
    hash.update(key.path().as_bytes());
}
fn record(hash: &mut Sha256, fields: &ComponentFields) {
    count(hash, fields.len());
    for (id, data) in fields {
        key(hash, id);
        value(hash, data);
    }
}
fn value(hash: &mut Sha256, value: &ComponentValue) {
    match value {
        ComponentValue::Bool(value) => hash.update([0, u8::from(*value)]),
        ComponentValue::U32(value) => {
            hash.update([1]);
            hash.update(value.to_le_bytes());
        }
        ComponentValue::I64(value) => {
            hash.update([2]);
            hash.update(value.to_le_bytes());
        }
        ComponentValue::Content(value) => {
            hash.update([3]);
            key(hash, value);
        }
        ComponentValue::Entity(value) => {
            hash.update([4]);
            hash.update(value.0.to_le_bytes());
        }
        ComponentValue::Text(value) => {
            hash.update([5]);
            count(hash, value.len());
            hash.update(value.as_bytes());
        }
        ComponentValue::List(values) => {
            hash.update([6]);
            count(hash, values.len());
            for data in values {
                self::value(hash, data);
            }
        }
        ComponentValue::Record(values) => {
            hash.update([7]);
            record(hash, values);
        }
    }
}
