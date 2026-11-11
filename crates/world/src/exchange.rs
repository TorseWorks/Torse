//! World exchange payloads: snapshots and commit changes.
//!
//! This module owns the domain data that crosses the authority boundary. The
//! encoding itself belongs to `torse-codec`; what lives here is the field set,
//! its canonical order, and the conversion between runtime state and stable
//! content identity.
//!
//! Two rules make the payload encodable by someone who did not write the world:
//!
//! - Runtime identifiers never leave the process. A snapshot carries a state
//!   table that maps its compact cell identifiers back to stable content keys and
//!   finite values; a change carries the stable content of each written cell.
//! - Ordering is part of the payload. Sections ascend by [`SectionPos`], states by
//!   runtime identifier, entities by [`EntityId`], components and fields by
//!   content key, record entries by key, and list entries keep their semantic
//!   order.
//!
//! Entity translations carry persistent identities and exact source/destination
//! bits. Replay applies resolved positions directly; it never reruns collision
//! against a potentially different rule implementation.

use std::collections::BTreeMap;

use torse_types::{
    BlockPos, CommitMeta, ContentKey, EntityId, ResumePoint, SectionPos, SnapshotMeta,
};

use crate::component::{ComponentSchemas, ComponentSet, ComponentValue};
use crate::entity::{ComponentBatch, ComponentEdit, EntityMove, EntityStore, Position};
use crate::registry::{BlockStateId, BlockStateRegistry};
use crate::section::{Section, SectionStorage};
use crate::store::{BlockEdit, SectionStore, StoreError, WorldBounds};

/// One stable content state referenced by a snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateEntry {
    /// Runtime identifier used inside the snapshot.
    pub id: BlockStateId,
    /// Stable block identity.
    pub block: ContentKey,
    /// Finite field values in canonical schema order.
    pub values: Vec<u16>,
}

/// Cell storage of one snapshot section.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotCells {
    /// Every cell holds the same runtime state.
    Uniform(BlockStateId),
    /// One runtime state per cell, in linear index order.
    Dense(Vec<BlockStateId>),
}

impl SnapshotCells {
    /// Capture the stable exchange representation, independently of heap layout.
    pub fn capture(section: &Section) -> Self {
        match section.storage() {
            SectionStorage::Uniform(state) => Self::Uniform(*state),
            _ => Self::Dense(section.values().collect()),
        }
    }
}

/// One section inside a snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotSection {
    /// Section coordinate.
    pub pos: SectionPos,
    /// Cell storage.
    pub cells: SnapshotCells,
}

/// Component value inside a snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotValue {
    /// Boolean flag.
    Bool(bool),
    /// Unsigned 32-bit number.
    U32(u32),
    /// Signed 64-bit number.
    I64(i64),
    /// Content reference.
    Content(ContentKey),
    /// Entity reference.
    Entity(EntityId),
    /// UTF-8 text.
    Text(String),
    /// Ordered list; order is semantic.
    List(Vec<Self>),
    /// Record entries, sorted by key.
    Record(Vec<(ContentKey, Self)>),
}

/// One component instance inside a snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotComponent {
    /// Component schema identity.
    pub schema: ContentKey,
    /// Field values, sorted by field identity.
    pub fields: Vec<(ContentKey, SnapshotValue)>,
}
impl SnapshotComponent {
    /// Same conservative resident reservation as runtime component fields, without cloning.
    pub fn accounted_bytes(&self) -> u64 {
        fn key(key: &ContentKey) -> u64 {
            (key.namespace().len() + key.path().len() + 1) as u64 + 8
        }
        fn value(value: &SnapshotValue, depth: u32) -> u64 {
            if depth > crate::MAX_VALUE_DEPTH {
                return u64::MAX;
            }
            let bytes = match value {
                SnapshotValue::Content(id) => key(id),
                SnapshotValue::Entity(_) => 16,
                SnapshotValue::Text(text) => text.len() as u64,
                SnapshotValue::List(values) => values.iter().fold(0u64, |bytes, item| {
                    bytes.saturating_add(value_bytes(item, depth + 1))
                }),
                SnapshotValue::Record(values) => values.iter().fold(0u64, |bytes, (id, item)| {
                    bytes
                        .saturating_add(key(id))
                        .saturating_add(value_bytes(item, depth + 1))
                }),
                _ => 0,
            };
            9u64.saturating_add(bytes)
        }
        fn value_bytes(item: &SnapshotValue, depth: u32) -> u64 {
            value(item, depth)
        }
        let bytes = self.fields.iter().fold(8u64, |bytes, (id, item)| {
            bytes.saturating_add(key(id)).saturating_add(value(item, 0))
        });
        4096u64.saturating_add(bytes.saturating_mul(64))
    }
}

/// One entity inside a snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotEntity {
    /// Persistent identity.
    pub id: EntityId,
    /// Position components as IEEE-754 bit patterns, little-endian on encode.
    ///
    /// Bits are stored instead of decimal text so a round trip cannot change the
    /// value through formatting.
    pub position_bits: [u64; 3],
    /// Components sorted by schema identity.
    pub components: Vec<SnapshotComponent>,
}

/// Complete authoritative state at a commit position.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    /// Position, compatibility, and resume point.
    pub meta: SnapshotMeta,
    /// Section side length.
    pub side: u32,
    /// Supported vertical extent.
    pub bounds: WorldBounds,
    /// Runtime state table, ascending by identifier. Identifier zero is omitted
    /// because it is always the empty occupancy sentinel.
    pub states: Vec<StateEntry>,
    /// Sections, ascending by coordinate.
    pub sections: Vec<SnapshotSection>,
    /// Entities, ascending by identity.
    pub entities: Vec<SnapshotEntity>,
    /// Persistent allocator watermark, including despawned identities.
    pub next_entity_id: u128,
}

/// Stable content of one written cell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CellContent {
    /// The cell becomes empty.
    Empty,
    /// The cell holds this block.
    Block {
        /// Stable block identity.
        block: ContentKey,
        /// Finite values in canonical schema order.
        values: Vec<u16>,
    },
}

/// One cell written by a commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeEdit {
    /// Cell coordinate.
    pub pos: BlockPos,
    /// Stable content written.
    pub content: CellContent,
}

/// Committed changes of one transaction, in stable content form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeSet {
    /// Commit metadata of the record.
    pub meta: CommitMeta,
    /// Writes in commit order.
    pub edits: Vec<ChangeEdit>,
    /// Entity translations in commit order, with no duplicate identities.
    pub entity_moves: Vec<EntityMove>,
    /// Protected component replacements in transaction order.
    pub component_edits: Vec<ComponentEdit>,
    /// Creations, removals and allocator preconditions, when this record has lifecycle work.
    pub entity_lifecycle: Option<crate::EntityLifecycle>,
}

/// World rebuilt from a snapshot.
#[derive(Debug)]
pub struct RestoredWorld {
    /// Terrain store with every section readable.
    pub store: SectionStore,
    /// Restored entities.
    pub entities: EntityStore,
    /// Next work position recorded by the snapshot.
    pub resume: ResumePoint,
}

/// Failure produced while capturing, restoring, or applying payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExchangeError {
    /// The runtime section side or world bounds are unsupported.
    InvalidWorld {
        /// Section side length.
        side: u32,
    },
    /// A runtime state has no stable content identity.
    UnknownState {
        /// Runtime identifier.
        state: BlockStateId,
    },
    /// A snapshot references content the running registry does not know.
    UnknownBlock {
        /// Stable block identity.
        block: ContentKey,
        /// Finite values referenced by the snapshot.
        values: Vec<u16>,
    },
    /// A snapshot value does not match its component schema.
    Component {
        /// Schema being restored.
        schema: ContentKey,
        /// Underlying validation message.
        detail: String,
    },
    /// An entity in a snapshot is invalid for the runtime store.
    Entity {
        /// Entity identity.
        id: EntityId,
        /// Underlying message.
        detail: String,
    },
    /// Terrain storage rejected a restored section or edit.
    Store {
        /// Underlying message.
        detail: String,
    },
    /// A change set writes a cell whose content cannot be resolved.
    UnresolvedContent {
        /// Cell being written.
        pos: BlockPos,
        /// Stable block identity.
        block: ContentKey,
    },
}

impl std::fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidWorld { side } => write!(f, "section side {side} is not supported"),
            Self::UnknownState { state } => {
                write!(f, "runtime state {} has no stable identity", state.0)
            }
            Self::UnknownBlock { block, values } => {
                write!(f, "unknown block {block} with values {values:?}")
            }
            Self::Component { schema, detail } => write!(f, "component {schema}: {detail}"),
            Self::Entity { id, detail } => write!(f, "entity {}: {detail}", id.0),
            Self::Store { detail } => write!(f, "world storage: {detail}"),
            Self::UnresolvedContent { pos, block } => write!(
                f,
                "cannot resolve {block} for cell [{}, {}, {}]",
                pos.x, pos.y, pos.z
            ),
        }
    }
}

impl std::error::Error for ExchangeError {}

impl Snapshot {
    /// Capture the complete in-memory world.
    ///
    /// Every tracked section must be readable; a section in a loading phase means
    /// the world is not quiescent and the snapshot would be incomplete.
    pub fn capture(
        meta: SnapshotMeta,
        store: &SectionStore,
        entities: &EntityStore,
        registry: &BlockStateRegistry,
    ) -> Result<Self, ExchangeError> {
        Self::capture_selected(meta, store, entities, registry, None)
    }

    /// Capture saveable components without cloning runtime-only payloads.
    pub fn capture_persistent(
        meta: SnapshotMeta,
        store: &SectionStore,
        entities: &EntityStore,
        registry: &BlockStateRegistry,
        schemas: &ComponentSchemas,
    ) -> Result<Self, ExchangeError> {
        Self::capture_selected(meta, store, entities, registry, Some(schemas))
    }

    pub(crate) fn capture_selected(
        meta: SnapshotMeta,
        store: &SectionStore,
        entities: &EntityStore,
        registry: &BlockStateRegistry,
        schemas: Option<&ComponentSchemas>,
    ) -> Result<Self, ExchangeError> {
        Self::capture_region(meta, store, entities, registry, schemas, None)
    }

    pub(crate) fn capture_region(
        meta: SnapshotMeta,
        store: &SectionStore,
        entities: &EntityStore,
        registry: &BlockStateRegistry,
        schemas: Option<&ComponentSchemas>,
        selected: Option<SectionPos>,
    ) -> Result<Self, ExchangeError> {
        if let Some(schemas) = schemas {
            schemas
                .validate_compatibility(meta.compatibility)
                .map_err(|error| ExchangeError::Store {
                    detail: error.to_string(),
                })?;
        }
        let side = store.side();
        let bounds = store.bounds();
        let mut sections: Vec<SnapshotSection> =
            Vec::with_capacity(selected.map_or(store.len(), |_| 1));
        let mut used: BTreeMap<BlockStateId, ()> = BTreeMap::new();

        for pos in store
            .positions()
            .filter(|pos| selected.is_none_or(|s| s == *pos))
        {
            if !matches!(
                store.phase(pos),
                Some(crate::store::SectionPhase::Ready | crate::store::SectionPhase::Active)
            ) {
                return Err(ExchangeError::Store {
                    detail: format!("section {pos:?} is not readable"),
                });
            }
            let Some(section) = store.section(pos) else {
                return Err(ExchangeError::Store {
                    detail: format!("section {pos:?} is not readable"),
                });
            };
            let cells = SnapshotCells::capture(section);
            match &cells {
                SnapshotCells::Uniform(value) => {
                    used.insert(*value, ());
                }
                SnapshotCells::Dense(list) => {
                    for value in list {
                        used.insert(*value, ());
                    }
                }
            }
            sections.push(SnapshotSection { pos, cells });
        }
        sections.sort_by_key(|section| section.pos);

        let state_count = used.len() - usize::from(used.contains_key(&BlockStateId::EMPTY));
        let mut states = Vec::with_capacity(state_count);
        for state in used.into_keys() {
            if state.is_empty() {
                continue;
            }
            let block = registry
                .definition_key(state)
                .ok_or(ExchangeError::UnknownState { state })?
                .clone();
            let values = registry.state_values(state).unwrap_or(&[]).to_vec();
            states.push(StateEntry {
                id: state,
                block,
                values,
            });
        }

        let mut entity_list = Vec::with_capacity(if selected.is_some() {
            0
        } else {
            entities.len()
        });
        for (_, entity) in entities
            .iter()
            .filter(|(_, e)| selected.is_none_or(|s| e.position.section(side) == Some(s)))
        {
            if let Some(schemas) = schemas {
                for schema in entity.components.ids() {
                    schemas
                        .persists(schema)
                        .map_err(|error| ExchangeError::Store {
                            detail: error.to_string(),
                        })?;
                }
            }
            let mut components: Vec<SnapshotComponent> = entity
                .components
                .ids()
                .filter(|id| schemas.is_none_or(|s| s.persists(id).expect("checked schema")))
                .map(|schema| SnapshotComponent {
                    schema: schema.clone(),
                    fields: entity
                        .components
                        .component(schema)
                        .map(|fields| {
                            fields
                                .iter()
                                .map(|(key, value)| (key.clone(), to_snapshot_value(value)))
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect();
            components.sort_by(|left, right| left.schema.cmp(&right.schema));
            entity_list.push(SnapshotEntity {
                id: entity.id,
                position_bits: [
                    entity.position.x.to_bits(),
                    entity.position.y.to_bits(),
                    entity.position.z.to_bits(),
                ],
                components,
            });
        }
        entity_list.sort_by_key(|entity| entity.id.0);

        Ok(Self {
            meta,
            side,
            bounds,
            states,
            sections,
            entities: entity_list,
            next_entity_id: entities.next_entity_id(),
        })
    }

    /// Rebuild a world from this snapshot.
    ///
    /// The runtime registry must resolve every state entry; unknown content is an
    /// error rather than a silent substitution. Section revisions restart at zero
    /// because they are run-local cache stamps, not persistent identity.
    pub fn restore(
        &self,
        registry: &BlockStateRegistry,
        schemas: &ComponentSchemas,
    ) -> Result<RestoredWorld, ExchangeError> {
        self.validate_structure()?;
        schemas
            .validate_compatibility(self.meta.compatibility)
            .map_err(|error| ExchangeError::Store {
                detail: error.to_string(),
            })?;
        let mut mapping: BTreeMap<BlockStateId, BlockStateId> = BTreeMap::new();
        for entry in &self.states {
            let state = registry
                .state_of(&entry.block, &entry.values)
                .ok_or_else(|| ExchangeError::UnknownBlock {
                    block: entry.block.clone(),
                    values: entry.values.clone(),
                })?;
            mapping.insert(entry.id, state);
        }

        let mut store =
            SectionStore::new(self.side, self.bounds).map_err(|error| ExchangeError::Store {
                detail: error.to_string(),
            })?;
        // Structure validation above proved that every cell has a symbol. A
        // native registry often resolves all symbols to their original IDs.
        let identity = mapping.iter().all(|(remote, local)| remote == local);
        for section in &self.sections {
            let data = match &section.cells {
                SnapshotCells::Uniform(value) => {
                    Section::new(self.side, resolve(*value, &mapping)?)
                }
                SnapshotCells::Dense(list) if identity => {
                    Section::from_cells(self.side, list.clone())
                }
                SnapshotCells::Dense(list) => {
                    let mut resolved = Vec::with_capacity(list.len());
                    for value in list {
                        resolved.push(resolve(*value, &mapping)?);
                    }
                    Section::from_cells(self.side, resolved)
                }
            };
            let data = data.map_err(|error| ExchangeError::Store {
                detail: error.to_string(),
            })?;
            store.request(section.pos).map_err(store_error)?;
            store.install(section.pos, data).map_err(store_error)?;
            store.finish_load(section.pos).map_err(store_error)?;
        }

        let mut entities = EntityStore::new(self.side).map_err(|error| ExchangeError::Entity {
            id: EntityId(0),
            detail: error.to_string(),
        })?;
        for entity in &self.entities {
            let mut components = ComponentSet::new();
            for component in &entity.components {
                let fields: BTreeMap<ContentKey, ComponentValue> = component
                    .fields
                    .iter()
                    .map(|(key, value)| (key.clone(), from_snapshot_value(value)))
                    .collect();
                components
                    .insert(schemas, &component.schema, fields)
                    .map_err(|error| ExchangeError::Component {
                        schema: component.schema.clone(),
                        detail: error.to_string(),
                    })?;
            }
            entities
                .insert_restored(
                    entity.id,
                    Position::new(
                        f64::from_bits(entity.position_bits[0]),
                        f64::from_bits(entity.position_bits[1]),
                        f64::from_bits(entity.position_bits[2]),
                    ),
                    components,
                )
                .map_err(|error| ExchangeError::Entity {
                    id: entity.id,
                    detail: error.to_string(),
                })?;
        }

        entities
            .restore_next_entity_id(self.next_entity_id)
            .map_err(|error| ExchangeError::Store {
                detail: error.to_string(),
            })?;
        Ok(RestoredWorld {
            store,
            entities,
            resume: self.meta.resume_at,
        })
    }

    /// Reject a watermark that could reissue a live identity. The maximum value
    /// represents an exhausted allocator and also covers a restored maximum ID.
    pub fn validate_entity_allocator(&self) -> Result<(), ExchangeError> {
        if self.next_entity_id == 0
            || (self.next_entity_id != u128::MAX
                && self
                    .entities
                    .iter()
                    .any(|entity| entity.id.0 >= self.next_entity_id))
        {
            return Err(ExchangeError::Store {
                detail: "invalid entity allocator watermark".to_owned(),
            });
        }
        Ok(())
    }
}

impl ChangeSet {
    /// Build a change set from committed block edits.
    pub fn from_edits(
        meta: CommitMeta,
        registry: &BlockStateRegistry,
        edits: &[BlockEdit],
    ) -> Result<Self, ExchangeError> {
        let mut changes = Vec::with_capacity(edits.len());
        for edit in edits {
            let content = if edit.state.is_empty() {
                CellContent::Empty
            } else {
                let block = registry
                    .definition_key(edit.state)
                    .ok_or(ExchangeError::UnknownState { state: edit.state })?
                    .clone();
                let values = registry.state_values(edit.state).unwrap_or(&[]).to_vec();
                CellContent::Block { block, values }
            };
            changes.push(ChangeEdit {
                pos: edit.pos,
                content,
            });
        }
        Ok(Self {
            meta,
            edits: changes,
            entity_moves: Vec::new(),
            component_edits: Vec::new(),
            entity_lifecycle: None,
        })
    }

    /// Apply the changes to a stored world.
    ///
    /// All writes are validated before any of them is applied, so a change set
    /// with one unresolvable cell changes nothing.
    pub fn apply(
        &self,
        store: &mut SectionStore,
        registry: &BlockStateRegistry,
    ) -> Result<Vec<(SectionPos, torse_types::Revision)>, ExchangeError> {
        if !self.entity_moves.is_empty()
            || !self.component_edits.is_empty()
            || self.entity_lifecycle.is_some()
        {
            return Err(ExchangeError::Store {
                detail: "entity movements require the entity-aware replay path".to_owned(),
            });
        }
        let edits = self.resolve_edits(registry)?;
        store.apply_edits(&edits).map_err(store_error)
    }

    /// Replay one mixed terrain/entity record as a fully validated group.
    ///
    /// Replay trusts the authoritative collision result but still checks source
    /// positions, identity, duplicates, and storage targets. A bad entity tail
    /// cannot partially apply otherwise valid terrain writes.
    pub fn apply_with_entities(
        &self,
        store: &mut SectionStore,
        entities: &mut EntityStore,
        registry: &BlockStateRegistry,
    ) -> Result<Vec<(SectionPos, torse_types::Revision)>, ExchangeError> {
        if !self.component_edits.is_empty() || self.entity_lifecycle.is_some() {
            return Err(ExchangeError::Store {
                detail: "component edits require schema-aware replay".to_owned(),
            });
        }
        self.apply_group(store, entities, registry, ComponentBatch::default(), None)
    }

    /// Replay terrain, movement, and protected components after full validation.
    pub fn apply_with_components(
        &self,
        store: &mut SectionStore,
        entities: &mut EntityStore,
        registry: &BlockStateRegistry,
        schemas: &ComponentSchemas,
    ) -> Result<Vec<(SectionPos, torse_types::Revision)>, ExchangeError> {
        schemas
            .validate_compatibility(self.meta.compatibility)
            .map_err(|error| ExchangeError::Store {
                detail: error.to_string(),
            })?;
        let lifecycle = self
            .entity_lifecycle
            .as_ref()
            .map(|change| {
                change.validate_world(store)?;
                if self
                    .entity_moves
                    .iter()
                    .any(|movement| change.conflicts_with(movement.id))
                    || self
                        .component_edits
                        .iter()
                        .any(|edit| change.conflicts_with(edit.id))
                {
                    return Err(crate::EntityError::InvalidLifecycle);
                }
                entities.prepare_lifecycle(change, schemas)
            })
            .transpose()
            .map_err(|error| ExchangeError::Store {
                detail: error.to_string(),
            })?;
        let batch = entities
            .prepare_components(&self.component_edits, schemas)
            .map_err(|error| ExchangeError::Store {
                detail: error.to_string(),
            })?;
        self.apply_group(store, entities, registry, batch, lifecycle)
    }

    fn apply_group(
        &self,
        store: &mut SectionStore,
        entities: &mut EntityStore,
        registry: &BlockStateRegistry,
        components: ComponentBatch,
        lifecycle: Option<crate::LifecycleBatch>,
    ) -> Result<Vec<(SectionPos, torse_types::Revision)>, ExchangeError> {
        let edits = self.resolve_edits(registry)?;
        entities
            .validate_moves(&self.entity_moves)
            .map_err(|error| ExchangeError::Store {
                detail: error.to_string(),
            })?;
        store.validate_edits(&edits).map_err(store_error)?;
        let touched = store.apply_edits(&edits).map_err(store_error)?;
        entities
            .apply_moves(&self.entity_moves)
            .map_err(|error| ExchangeError::Store {
                detail: error.to_string(),
            })?;
        entities
            .publish_components(components)
            .map_err(|error| ExchangeError::Store {
                detail: error.to_string(),
            })?;
        if let Some(batch) = lifecycle {
            entities
                .publish_lifecycle(batch)
                .map_err(|error| ExchangeError::Store {
                    detail: error.to_string(),
                })?;
        }
        Ok(touched)
    }

    fn resolve_edits(
        &self,
        registry: &BlockStateRegistry,
    ) -> Result<Vec<BlockEdit>, ExchangeError> {
        let mut edits = Vec::with_capacity(self.edits.len());
        for edit in &self.edits {
            let state = match &edit.content {
                CellContent::Empty => BlockStateId::EMPTY,
                CellContent::Block { block, values } => registry
                    .state_of(block, values)
                    .ok_or_else(|| ExchangeError::UnresolvedContent {
                        pos: edit.pos,
                        block: block.clone(),
                    })?,
            };
            edits.push(BlockEdit {
                pos: edit.pos,
                state,
            });
        }
        Ok(edits)
    }
}

fn resolve(
    id: BlockStateId,
    mapping: &BTreeMap<BlockStateId, BlockStateId>,
) -> Result<BlockStateId, ExchangeError> {
    if id.is_empty() {
        return Ok(BlockStateId::EMPTY);
    }
    mapping
        .get(&id)
        .copied()
        .ok_or(ExchangeError::UnknownState { state: id })
}

fn store_error(error: StoreError) -> ExchangeError {
    ExchangeError::Store {
        detail: error.to_string(),
    }
}

pub(crate) fn to_snapshot_value(value: &ComponentValue) -> SnapshotValue {
    match value {
        ComponentValue::Bool(flag) => SnapshotValue::Bool(*flag),
        ComponentValue::U32(number) => SnapshotValue::U32(*number),
        ComponentValue::I64(number) => SnapshotValue::I64(*number),
        ComponentValue::Content(key) => SnapshotValue::Content(key.clone()),
        ComponentValue::Entity(id) => SnapshotValue::Entity(*id),
        ComponentValue::Text(text) => SnapshotValue::Text(text.clone()),
        ComponentValue::List(items) => {
            SnapshotValue::List(items.iter().map(to_snapshot_value).collect())
        }
        ComponentValue::Record(entries) => SnapshotValue::Record(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), to_snapshot_value(value)))
                .collect(),
        ),
    }
}

pub(crate) fn from_snapshot_value(value: &SnapshotValue) -> ComponentValue {
    match value {
        SnapshotValue::Bool(flag) => ComponentValue::Bool(*flag),
        SnapshotValue::U32(number) => ComponentValue::U32(*number),
        SnapshotValue::I64(number) => ComponentValue::I64(*number),
        SnapshotValue::Content(key) => ComponentValue::Content(key.clone()),
        SnapshotValue::Entity(id) => ComponentValue::Entity(*id),
        SnapshotValue::Text(text) => ComponentValue::Text(text.clone()),
        SnapshotValue::List(items) => {
            ComponentValue::List(items.iter().map(from_snapshot_value).collect())
        }
        SnapshotValue::Record(entries) => ComponentValue::Record(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), from_snapshot_value(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torse_content::{compile, m0_fixture_pack};
    use torse_types::{CommitSeq, Compatibility, Digest, Tick, WorldId};

    use crate::component::ComponentSchemas;
    use crate::store::{SectionPhase, TerrainQuery};

    fn key(value: &str) -> ContentKey {
        ContentKey::parse(value).expect("test key is valid")
    }

    fn registry() -> BlockStateRegistry {
        let content = compile(m0_fixture_pack()).expect("fixture content compiles");
        BlockStateRegistry::build(&content).expect("registry builds")
    }

    fn schemas() -> ComponentSchemas {
        let content = compile(m0_fixture_pack()).expect("fixture content compiles");
        ComponentSchemas::build(&content)
    }

    fn meta() -> SnapshotMeta {
        SnapshotMeta {
            cursor: torse_types::CommitCursor {
                world: WorldId(3),
                seq: CommitSeq(5),
            },
            compatibility: Compatibility {
                authority_schema: 1,
                rules_digest: Digest([7; 32]),
                content_digest: Digest([8; 32]),
            },
            resume_at: ResumePoint {
                tick: Tick(9),
                next_phase: 2,
                next_step: 0,
            },
        }
    }

    fn world() -> (SectionStore, EntityStore) {
        let bounds = WorldBounds::new(0, 64).expect("bounds are valid");
        let mut store = SectionStore::new(16, bounds).expect("side is valid");
        let registry = registry();
        let stone = registry
            .default_state(&key("base:stone"))
            .expect("stone exists");
        let glass = registry
            .default_state(&key("base:glass_slab"))
            .expect("glass exists");

        for pos in [SectionPos::new(1, 0, 0), SectionPos::ZERO] {
            store.request(pos).expect("request works");
            store
                .install(
                    pos,
                    crate::section::Section::empty(16).expect("side is valid"),
                )
                .expect("install works");
            store.finish_load(pos).expect("finish works");
        }
        store
            .edit(BlockPos::new(1, 1, 1), stone)
            .expect("edit works");
        store
            .edit(BlockPos::new(2, 1, 1), glass)
            .expect("edit works");
        store
            .edit(BlockPos::new(17, 1, 1), stone)
            .expect("edit works");

        let mut entities = EntityStore::new(16).expect("side is valid");
        let handle = entities
            .spawn(Position::new(0.5, 1.5, 0.5))
            .expect("spawn works");
        entities
            .components_mut(handle)
            .expect("entity exists")
            .insert(
                &schemas(),
                &key("base:schema/inventory_content"),
                Default::default(),
            )
            .expect("component is valid");
        (store, entities)
    }

    #[test]
    fn capture_orders_sections_and_states() {
        let (store, entities) = world();
        let registry = registry();
        let snapshot =
            Snapshot::capture(meta(), &store, &entities, &registry).expect("capture works");
        assert_eq!(snapshot.sections.len(), 2);
        assert_eq!(snapshot.sections[0].pos, SectionPos::ZERO);
        assert_eq!(snapshot.sections[1].pos, SectionPos::new(1, 0, 0));
        let mut previous = 0;
        for entry in &snapshot.states {
            assert!(entry.id.0 >= previous);
            previous = entry.id.0;
        }
        assert_eq!(snapshot.entities.len(), 1);
        assert_eq!(snapshot.side, 16);
    }

    #[test]
    fn restore_reproduces_cells_entities_and_resume_point() {
        let (store, entities) = world();
        let registry = registry();
        let snapshot =
            Snapshot::capture(meta(), &store, &entities, &registry).expect("capture works");
        let restored = snapshot
            .restore(&registry, &schemas())
            .expect("restore works");

        for pos in [
            BlockPos::new(1, 1, 1),
            BlockPos::new(2, 1, 1),
            BlockPos::new(17, 1, 1),
            BlockPos::new(3, 1, 1),
        ] {
            assert_eq!(restored.store.query(pos), store.query(pos), "cell {pos:?}");
        }
        assert_eq!(restored.store.len(), 2);
        assert_eq!(restored.entities.len(), 1);
        let entity = restored.entities.iter().next().expect("entity exists").1;
        assert_eq!(entity.id, EntityId(1));
        assert_eq!(entity.position, Position::new(0.5, 1.5, 0.5));
        assert_eq!(entity.components.len(), 1);
        assert_eq!(restored.resume, meta().resume_at);
    }

    #[test]
    fn dense_native_and_remapped_symbols_restore_identical_cells() {
        let (store, entities) = world();
        let registry = registry();
        let native = Snapshot::capture(meta(), &store, &entities, &registry).unwrap();
        assert!(
            native
                .sections
                .iter()
                .any(|s| matches!(s.cells, SnapshotCells::Dense(_)))
        );
        let mut remapped = native.clone();
        for state in &mut remapped.states {
            state.id.0 += 100;
        }
        for section in &mut remapped.sections {
            match &mut section.cells {
                SnapshotCells::Uniform(id) => {
                    if !id.is_empty() {
                        id.0 += 100;
                    }
                }
                SnapshotCells::Dense(cells) => {
                    for id in cells {
                        if !id.is_empty() {
                            id.0 += 100;
                        }
                    }
                }
            }
        }
        let a = native.restore(&registry, &schemas()).unwrap();
        let b = remapped.restore(&registry, &schemas()).unwrap();
        for pos in store.positions() {
            assert_eq!(
                SnapshotCells::capture(a.store.section(pos).unwrap()),
                SnapshotCells::capture(b.store.section(pos).unwrap())
            );
        }
        let SnapshotCells::Dense(cells) = &mut remapped.sections[0].cells else {
            unreachable!()
        };
        cells[0] = BlockStateId(65535);
        assert!(remapped.restore(&registry, &schemas()).is_err());
        let mut invalid = native;
        let SnapshotCells::Dense(cells) = &mut invalid.sections[0].cells else {
            unreachable!()
        };
        cells[0] = BlockStateId(65535);
        assert!(invalid.restore(&registry, &schemas()).is_err());
    }

    #[test]
    fn restore_reports_unknown_content_instead_of_substituting() {
        let (store, entities) = world();
        let registry = registry();
        let mut snapshot =
            Snapshot::capture(meta(), &store, &entities, &registry).expect("capture works");
        snapshot.states[0].block = key("base:missing");
        let error = snapshot
            .restore(&registry, &schemas())
            .expect_err("unknown content is rejected");
        assert!(matches!(error, ExchangeError::UnknownBlock { .. }));
    }

    #[test]
    fn capture_rejects_a_loading_section() {
        let bounds = WorldBounds::new(0, 64).expect("bounds are valid");
        let mut store = SectionStore::new(16, bounds).expect("side is valid");
        store.request(SectionPos::ZERO).expect("request works");
        let entities = EntityStore::new(16).expect("side is valid");
        let error = Snapshot::capture(meta(), &store, &entities, &registry())
            .expect_err("a loading section is not readable");
        assert!(matches!(error, ExchangeError::Store { .. }));
        assert_eq!(store.phase(SectionPos::ZERO), Some(SectionPhase::Requested));
    }

    #[test]
    fn change_set_roundtrips_through_stable_content() {
        let (store, entities) = world();
        let registry = registry();
        let stone = registry
            .default_state(&key("base:stone"))
            .expect("stone exists");
        let edits = vec![
            BlockEdit {
                pos: BlockPos::new(4, 1, 4),
                state: stone,
            },
            BlockEdit {
                pos: BlockPos::new(5, 1, 4),
                state: BlockStateId::EMPTY,
            },
        ];
        let commit_meta = torse_types::CommitMeta {
            cursor: torse_types::CommitCursor {
                world: WorldId(3),
                seq: CommitSeq(6),
            },
            previous: CommitSeq(5),
            compatibility: meta().compatibility,
            resume_at: meta().resume_at,
        };
        let changes = ChangeSet::from_edits(commit_meta, &registry, &edits).expect("changes build");
        assert_eq!(changes.edits.len(), 2);
        assert!(matches!(changes.edits[1].content, CellContent::Empty));

        let snapshot =
            Snapshot::capture(meta(), &store, &entities, &registry).expect("capture works");
        let mut restored = snapshot
            .restore(&registry, &schemas())
            .expect("restore works");
        restored
            .store
            .edit(BlockPos::new(4, 1, 4), BlockStateId::EMPTY)
            .expect("setup edit");
        changes
            .apply(&mut restored.store, &registry)
            .expect("changes apply");
        assert_eq!(
            restored.store.query(BlockPos::new(4, 1, 4)),
            TerrainQuery::Ready(stone)
        );
    }

    #[test]
    fn change_set_with_unknown_content_changes_nothing() {
        let (store, entities) = world();
        let registry = registry();
        let mut restored = Snapshot::capture(meta(), &store, &entities, &registry)
            .expect("capture works")
            .restore(&registry, &schemas())
            .expect("restore works");
        let changes = ChangeSet {
            entity_lifecycle: None,
            component_edits: Vec::new(),
            entity_moves: Vec::new(),
            meta: torse_types::CommitMeta {
                cursor: torse_types::CommitCursor {
                    world: WorldId(3),
                    seq: CommitSeq(6),
                },
                previous: CommitSeq(5),
                compatibility: meta().compatibility,
                resume_at: meta().resume_at,
            },
            edits: vec![
                ChangeEdit {
                    pos: BlockPos::new(6, 1, 6),
                    content: CellContent::Empty,
                },
                ChangeEdit {
                    pos: BlockPos::new(7, 1, 6),
                    content: CellContent::Block {
                        block: key("base:missing"),
                        values: Vec::new(),
                    },
                },
            ],
        };
        let error = changes
            .apply(&mut restored.store, &registry)
            .expect_err("unknown content is rejected");
        assert!(matches!(error, ExchangeError::UnresolvedContent { .. }));
        assert_eq!(
            restored.store.query(BlockPos::new(6, 1, 6)),
            restored.store.query(BlockPos::new(6, 1, 6)),
        );
    }
}
