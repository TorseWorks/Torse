//! Versioned, lossless world archives and explicit offline field migrations.

use std::collections::BTreeMap;

use torse_content::CompiledContent;
use torse_types::{Compatibility, ContentKey, Fault, OutputBudget};

use crate::{ComponentFields, ComponentSchemas, ComponentSet, Snapshot, SnapshotValue};

/// Source versions are recorded independently of the currently installed packages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockStamp {
    pub definition_version: u32,
    pub schema: ContentKey,
    pub schema_version: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentStamp {
    pub owner: ContentKey,
    pub version: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArchiveManifest {
    pub blocks: BTreeMap<crate::BlockStateId, BlockStamp>,
    pub components: BTreeMap<ContentKey, ComponentStamp>,
}

/// Unknown payloads remain here, never replaced by air or silently discarded.
/// An archive is not an active world; activation still requires full validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotArchive {
    pub origin: Compatibility,
    pub manifest: ArchiveManifest,
    pub snapshot: Snapshot,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ArchiveIssue {
    Block { block: ContentKey, values: Vec<u16> },
    Component { schema: ContentKey },
    BlockVersion { block: ContentKey },
    ComponentVersion { schema: ContentKey },
    MissingStamp { key: ContentKey },
}

/// Trusted native callback over fields only. Source data is borrowed and immutable.
pub type MigrateFields = fn(&ComponentFields) -> Result<ComponentFields, Fault>;

struct FieldRule {
    owner: ContentKey,
    binding: ContentKey,
    to_version: u32,
    apply: MigrateFields,
}

type FiniteState = (ContentKey, Vec<u16>);

/// An explicitly authorized source/target compatibility transition.
pub struct MigrationPlan {
    pub source: Compatibility,
    pub target: Compatibility,
    fields: BTreeMap<(ContentKey, u32), FieldRule>,
    blocks: BTreeMap<FiniteState, FiniteState>,
}

impl MigrationPlan {
    pub fn new(source: Compatibility, target: Compatibility) -> Self {
        Self {
            source,
            target,
            fields: BTreeMap::new(),
            blocks: BTreeMap::new(),
        }
    }

    /// Bind a declared target migration to one exact source schema version.
    pub fn bind_component(
        &mut self,
        schemas: &ComponentSchemas,
        owner: &ContentKey,
        schema: ContentKey,
        from_version: u32,
        binding: ContentKey,
        apply: MigrateFields,
    ) -> Result<(), Fault> {
        let target = schemas
            .get(&schema)
            .ok_or_else(|| Fault::invalid_data("unknown migration target schema"))?;
        let key = (schema, from_version);
        if target.owner_module != *owner
            || target.migration_binding.as_ref() != Some(&binding)
            || from_version == 0
            || from_version >= target.schema_version
            || self.fields.contains_key(&key)
        {
            return Err(Fault::invalid_data("invalid component migration binding"));
        }
        self.fields.insert(
            key,
            FieldRule {
                owner: owner.clone(),
                binding,
                to_version: target.schema_version,
                apply,
            },
        );
        Ok(())
    }

    /// Explicit finite-state remapping. The destination must resolve at migration.
    /// This never guesses a placeholder or deletes unavailable cells.
    pub fn remap_block(
        &mut self,
        source: ContentKey,
        source_values: Vec<u16>,
        target: ContentKey,
        target_values: Vec<u16>,
    ) -> Result<(), Fault> {
        let key = (source, source_values);
        if self.blocks.contains_key(&key) {
            return Err(Fault::invalid_data("duplicate block migration"));
        }
        self.blocks.insert(key, (target, target_values));
        Ok(())
    }
}

fn block_stamp(content: &CompiledContent, id: &ContentKey) -> Option<BlockStamp> {
    let block = content.blocks.get(id)?;
    let schema = content.state_schemas.get(&block.state_schema)?;
    Some(BlockStamp {
        definition_version: block.definition_version,
        schema: schema.id.clone(),
        schema_version: schema.schema_version,
    })
}

impl SnapshotArchive {
    /// Stamp existing snapshots only when the source content digest is exact.
    pub fn capture(
        snapshot: &Snapshot,
        content: &CompiledContent,
        budget: OutputBudget,
    ) -> Result<Self, Fault> {
        if snapshot.meta.compatibility.content_digest != content.authority_digest {
            return Err(Fault::incompatible(
                "archive source content digest mismatch",
            ));
        }
        check_budget(snapshot, &ArchiveManifest::default(), budget)?;
        let schemas = ComponentSchemas::build(content);
        let mut manifest = ArchiveManifest::default();
        for entry in &snapshot.states {
            manifest.blocks.insert(
                entry.id,
                block_stamp(content, &entry.block)
                    .ok_or_else(|| Fault::invalid_data("archive source block is unavailable"))?,
            );
        }
        for component in snapshot.entities.iter().flat_map(|e| &e.components) {
            let schema = schemas
                .get(&component.schema)
                .ok_or_else(|| Fault::invalid_data("archive source schema is unavailable"))?;
            manifest.components.insert(
                schema.id.clone(),
                ComponentStamp {
                    owner: schema.owner_module.clone(),
                    version: schema.schema_version,
                },
            );
        }
        check_budget(snapshot, &manifest, budget)?;
        Ok(Self {
            origin: snapshot.meta.compatibility,
            manifest,
            snapshot: snapshot.clone(),
        })
    }

    /// Preserve a legacy payload without inventing source versions. Callers may
    /// supply a manifest obtained from the original, trusted package lock/config.
    pub fn import_legacy(
        snapshot: &Snapshot,
        manifest: ArchiveManifest,
        budget: OutputBudget,
    ) -> Result<Self, Fault> {
        Self::validate_legacy(snapshot, &manifest, budget)?;
        Ok(Self {
            origin: snapshot.meta.compatibility,
            manifest,
            snapshot: snapshot.clone(),
        })
    }

    /// Validate the same canonical structure and owned budget as import, without
    /// creating an owned archive. The caller retains the immutable snapshot.
    pub fn validate_legacy(
        snapshot: &Snapshot,
        manifest: &ArchiveManifest,
        budget: OutputBudget,
    ) -> Result<(), Fault> {
        check_budget(snapshot, manifest, budget)
    }

    pub fn inspect(
        &self,
        target: &CompiledContent,
        budget: OutputBudget,
    ) -> Result<Vec<ArchiveIssue>, Fault> {
        check_budget(&self.snapshot, &self.manifest, budget)?;
        let registry = crate::BlockStateRegistry::build(target)
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        let schemas = ComponentSchemas::build(target);
        let mut issues = std::collections::BTreeSet::new();
        for entry in &self.snapshot.states {
            if registry.state_of(&entry.block, &entry.values).is_none() {
                issues.insert(ArchiveIssue::Block {
                    block: entry.block.clone(),
                    values: entry.values.clone(),
                });
            }
            match self.manifest.blocks.get(&entry.id) {
                None => {
                    issues.insert(ArchiveIssue::MissingStamp {
                        key: entry.block.clone(),
                    });
                }
                Some(stamp) if block_stamp(target, &entry.block).as_ref() != Some(stamp) => {
                    issues.insert(ArchiveIssue::BlockVersion {
                        block: entry.block.clone(),
                    });
                }
                _ => {}
            }
            if issues.len() > budget.max_items as usize {
                return Err(Fault::resource_limit("archive report item budget"));
            }
        }
        for component in self.snapshot.entities.iter().flat_map(|e| &e.components) {
            let target_schema = schemas.get(&component.schema);
            if target_schema.is_none() {
                issues.insert(ArchiveIssue::Component {
                    schema: component.schema.clone(),
                });
            }
            match self.manifest.components.get(&component.schema) {
                None => {
                    issues.insert(ArchiveIssue::MissingStamp {
                        key: component.schema.clone(),
                    });
                }
                Some(stamp)
                    if target_schema.is_none_or(|s| {
                        s.owner_module != stamp.owner || s.schema_version != stamp.version
                    }) =>
                {
                    issues.insert(ArchiveIssue::ComponentVersion {
                        schema: component.schema.clone(),
                    });
                }
                _ => {}
            }
            if issues.len() > budget.max_items as usize {
                return Err(Fault::resource_limit("archive report item budget"));
            }
        }
        Ok(issues.into_iter().collect())
    }

    /// Produce a candidate without modifying the original archive. Unavailable
    /// objects keep their exact fields and stamps; activation remains forbidden.
    pub fn migrate(
        &self,
        target: &CompiledContent,
        plan: &MigrationPlan,
        budget: OutputBudget,
    ) -> Result<Self, Fault> {
        check_budget(&self.snapshot, &self.manifest, budget)?;
        if self.snapshot.meta.compatibility != plan.source
            || plan.target.content_digest != target.authority_digest
        {
            return Err(Fault::incompatible(
                "archive migration compatibility mismatch",
            ));
        }
        let registry = crate::BlockStateRegistry::build(target)
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        let schemas = ComponentSchemas::build(target);
        let mut candidate = self.clone();
        let mut block_stamps = self.manifest.blocks.clone();
        for entry in &mut candidate.snapshot.states {
            if let Some((block, values)) = plan
                .blocks
                .get(&(entry.block.clone(), entry.values.clone()))
            {
                if registry.state_of(block, values).is_none() {
                    return Err(Fault::invalid_data(
                        "unresolved block migration destination",
                    ));
                }
                entry.block = block.clone();
                entry.values = values.clone();
                block_stamps.insert(
                    entry.id,
                    block_stamp(target, block).expect("resolved block"),
                );
            }
        }
        candidate.manifest.blocks = block_stamps;
        let mut new_stamps = self.manifest.components.clone();
        for component in candidate
            .snapshot
            .entities
            .iter_mut()
            .flat_map(|e| &mut e.components)
        {
            let Some(target_schema) = schemas.get(&component.schema) else {
                continue;
            };
            let source = self
                .manifest
                .components
                .get(&component.schema)
                .ok_or_else(|| Fault::incompatible("component source version is unknown"))?;
            if source.owner != target_schema.owner_module {
                return Err(Fault::incompatible(
                    "component owner migration is not authorized",
                ));
            }
            let mut fields: ComponentFields = component
                .fields
                .iter()
                .map(|(key, value)| (key.clone(), crate::exchange::from_snapshot_value(value)))
                .collect();
            if source.version != target_schema.schema_version {
                let rule = plan
                    .fields
                    .get(&(component.schema.clone(), source.version))
                    .ok_or_else(|| Fault::incompatible("component migration handler is missing"))?;
                if rule.owner != source.owner
                    || rule.to_version != target_schema.schema_version
                    || target_schema.migration_binding.as_ref() != Some(&rule.binding)
                {
                    return Err(Fault::incompatible("component migration version mismatch"));
                }
                fields = (rule.apply)(&fields)?;
            }
            ComponentSet::validate(&schemas, &component.schema, &fields)
                .map_err(|e| Fault::invalid_data(e.to_string()))?;
            component.fields = fields
                .iter()
                .map(|(key, value)| (key.clone(), crate::exchange::to_snapshot_value(value)))
                .collect();
            new_stamps.insert(
                component.schema.clone(),
                ComponentStamp {
                    owner: source.owner.clone(),
                    version: target_schema.schema_version,
                },
            );
        }
        candidate.manifest.components = new_stamps;
        candidate.snapshot.meta.compatibility = plan.target;
        check_budget(&candidate.snapshot, &candidate.manifest, budget)?;
        Ok(candidate)
    }

    /// Versions and availability are checked before normal domain restoration.
    pub fn validate_for_restore(
        &self,
        target: &CompiledContent,
        budget: OutputBudget,
    ) -> Result<(), Fault> {
        if self.snapshot.meta.compatibility.content_digest != target.authority_digest
            || !self.inspect(target, budget)?.is_empty()
        {
            return Err(Fault::incompatible(
                "archive has unavailable or unmigrated content",
            ));
        }
        let registry = crate::BlockStateRegistry::build(target)
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        let schemas = ComponentSchemas::build(target);
        self.snapshot
            .restore(&registry, &schemas)
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        Ok(())
    }
}

struct Budget {
    items: u32,
    bytes: u64,
}
impl Budget {
    fn charge(&mut self, items: usize, bytes: usize) -> Result<(), Fault> {
        self.items = self
            .items
            .checked_sub(
                u32::try_from(items)
                    .map_err(|_| Fault::resource_limit("archive item count overflow"))?,
            )
            .ok_or_else(|| Fault::resource_limit("archive item budget"))?;
        self.bytes = self
            .bytes
            .checked_sub(bytes as u64)
            .ok_or_else(|| Fault::resource_limit("archive byte budget"))?;
        Ok(())
    }
    fn key(&mut self, key: &ContentKey) -> Result<(), Fault> {
        self.charge(0, key.namespace().len() + key.path().len())
    }
    fn value(&mut self, value: &SnapshotValue, depth: u32) -> Result<(), Fault> {
        if depth > crate::MAX_VALUE_DEPTH + 1 {
            return Err(Fault::resource_limit("archive value nesting"));
        }
        self.charge(1, size_of::<SnapshotValue>())?;
        match value {
            SnapshotValue::Content(key) => self.key(key)?,
            SnapshotValue::Text(text) => self.charge(0, text.len())?,
            SnapshotValue::List(values) => {
                for value in values {
                    self.value(value, depth + 1)?;
                }
            }
            SnapshotValue::Record(fields) => {
                if fields.windows(2).any(|w| w[0].0 >= w[1].0) {
                    return Err(Fault::invalid_data("archive record keys are not canonical"));
                }
                for (key, value) in fields {
                    self.charge(1, size_of::<ContentKey>())?;
                    self.key(key)?;
                    self.value(value, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn check_budget(
    snapshot: &Snapshot,
    manifest: &ArchiveManifest,
    budget: OutputBudget,
) -> Result<(), Fault> {
    snapshot
        .validate_structure()
        .map_err(|e| Fault::invalid_data(e.to_string()))?;
    let mut remaining = Budget {
        items: budget.max_items,
        bytes: budget.max_bytes,
    };
    remaining.charge(1, size_of::<SnapshotArchive>())?;
    for stamp in manifest.blocks.values() {
        remaining.charge(
            1,
            size_of::<BlockStamp>() + size_of::<crate::BlockStateId>(),
        )?;
        remaining.key(&stamp.schema)?;
    }
    for (key, stamp) in &manifest.components {
        remaining.charge(1, size_of::<ComponentStamp>() + size_of::<ContentKey>())?;
        remaining.key(key)?;
        remaining.key(&stamp.owner)?;
    }
    for state in &snapshot.states {
        remaining.charge(1, size_of::<crate::StateEntry>())?;
        remaining.key(&state.block)?;
        remaining.charge(
            0,
            state
                .values
                .len()
                .checked_mul(size_of::<u16>())
                .ok_or_else(|| Fault::resource_limit("archive state budget overflow"))?,
        )?;
    }
    for section in &snapshot.sections {
        remaining.charge(1, size_of::<crate::SnapshotSection>())?;
        if let crate::SnapshotCells::Dense(cells) = &section.cells {
            if cells.len() != (snapshot.side as usize).pow(3) {
                return Err(Fault::invalid_data("archive section dimensions"));
            }
            remaining.charge(
                0,
                cells
                    .len()
                    .checked_mul(size_of::<crate::BlockStateId>())
                    .ok_or_else(|| Fault::resource_limit("archive cell budget overflow"))?,
            )?;
        }
    }
    for entity in &snapshot.entities {
        remaining.charge(1, size_of::<crate::SnapshotEntity>())?;
        if entity
            .components
            .windows(2)
            .any(|w| w[0].schema >= w[1].schema)
        {
            return Err(Fault::invalid_data("noncanonical archive components"));
        }
        for component in &entity.components {
            remaining.charge(1, size_of::<crate::SnapshotComponent>())?;
            remaining.key(&component.schema)?;
            if component.fields.windows(2).any(|w| w[0].0 >= w[1].0) {
                return Err(Fault::invalid_data(
                    "archive component keys are not canonical",
                ));
            }
            for (key, value) in &component.fields {
                remaining.charge(1, size_of::<ContentKey>())?;
                remaining.key(key)?;
                remaining.value(value, 0)?;
            }
        }
    }
    Ok(())
}
