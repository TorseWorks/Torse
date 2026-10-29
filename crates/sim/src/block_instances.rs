//! Derive attached-entity lifecycle from a complete, authorized set of cell writes.

use torse_types::{EntityId, RejectCode};
use torse_world::{
    BlockEdit, BlockInstance, BlockStateRegistry, ComponentEdit, ComponentSchemas, ComponentSet,
    EntityLifecycle, EntityMove, EntityState, EntityStore, LifecycleBatch, MAX_ENTITY_LIFECYCLE,
    SectionStore, TerrainQuery, block_instance_schema,
};

pub(crate) struct Writes<'a> {
    pub edits: &'a [BlockEdit],
    pub components: &'a [ComponentEdit],
    pub movements: &'a [EntityMove],
}

/// Automatic lifecycle is kernel work, after each source member has passed its
/// own ownership checks. It shares the transaction's allocator and commit record.
pub(crate) fn prepare(
    store: &SectionStore,
    entities: &EntityStore,
    registry: &BlockStateRegistry,
    schemas: &ComponentSchemas,
    writes: Writes<'_>,
    existing: Option<LifecycleBatch>,
) -> Result<Option<LifecycleBatch>, RejectCode> {
    let mut change = existing
        .as_ref()
        .map(|b| b.change().clone())
        .unwrap_or(EntityLifecycle {
            previous_next_id: entities.next_entity_id(),
            next_id: entities.next_entity_id(),
            spawned: Vec::new(),
            despawned: Vec::new(),
        });
    let mut changed = false;
    // Low-level unchecked groups historically allow repeated cell writes.
    // Derive one lifecycle from their final state, in canonical cell order.
    let cells: std::collections::BTreeMap<_, _> =
        writes.edits.iter().map(|e| (e.pos, e.state)).collect();
    for (pos, state) in cells {
        let edit = BlockEdit { pos, state };
        let old = match store.query(edit.pos) {
            TerrainQuery::Ready(state) => state,
            TerrainQuery::NotLoaded => return Err(RejectCode::NotLoaded),
            TerrainQuery::OutOfBounds | TerrainQuery::Exterior => {
                return Err(RejectCode::MissingTarget);
            }
        };
        let old_definition = registry.definition(old);
        let new_definition = if edit.state.is_empty() {
            None
        } else {
            Some(
                registry
                    .definition(edit.state)
                    .ok_or(RejectCode::InvalidData)?,
            )
        };
        let attached = entities
            .block_instance_at(edit.pos)
            .map_err(|_| RejectCode::InvalidData)?;
        let required = old_definition.is_some_and(|d| !d.component_defaults.is_empty());
        if required != attached.is_some() {
            return Err(RejectCode::InvalidData);
        }
        if let Some((_, entity)) = attached {
            let binding = BlockInstance::of(entity)
                .map_err(|_| RejectCode::InvalidData)?
                .ok_or(RejectCode::InvalidData)?;
            if old_definition.is_none_or(|d| d.id != binding.block) {
                return Err(RejectCode::InvalidData);
            }
        }
        if old_definition.map(|d| &d.id) == new_definition.map(|d| &d.id) {
            // Geometry/state changes of one logical block retain its instance.
            continue;
        }
        if let Some((_, entity)) = attached {
            if writes.components.iter().any(|e| e.id == entity.id)
                || writes.movements.iter().any(|m| m.id == entity.id)
                || change.conflicts_with(entity.id)
            {
                return Err(RejectCode::Conflict);
            }
            if change.spawned.len() + change.despawned.len() >= MAX_ENTITY_LIFECYCLE {
                return Err(RejectCode::QuotaExceeded);
            }
            change.despawned.push(EntityState::capture(entity));
            changed = true;
        }
        if let Some(definition) = new_definition.filter(|d| !d.component_defaults.is_empty()) {
            if change.spawned.len() + change.despawned.len() >= MAX_ENTITY_LIFECYCLE {
                return Err(RejectCode::QuotaExceeded);
            }
            let binding = BlockInstance {
                pos: edit.pos,
                block: definition.id.clone(),
            };
            let mut fields = std::collections::BTreeMap::new();
            fields.insert(block_instance_schema(), binding.fields());
            for schema in &definition.component_defaults {
                let defaults = schemas
                    .block_default(&definition.id, schema)
                    .cloned()
                    .unwrap_or_default();
                ComponentSet::validate(schemas, schema, &defaults)
                    .map_err(|_| RejectCode::InvalidData)?;
                fields.insert(schema.clone(), defaults);
            }
            let id = change.next_id;
            change.next_id = id.checked_add(1).ok_or(RejectCode::QuotaExceeded)?;
            change.spawned.push(EntityState {
                id: EntityId(id),
                position_bits: binding.position().to_bits(),
                components: fields,
            });
            changed = true;
        }
        // Check after each cell so a large edit cannot accumulate unbounded data.
        EntityLifecycle::validate_payload(
            change.spawned.len() + change.despawned.len(),
            change
                .spawned
                .iter()
                .chain(&change.despawned)
                .map(|e| &e.components),
        )
        .map_err(|_| RejectCode::QuotaExceeded)?;
    }
    if !changed {
        return Ok(existing);
    }
    change
        .validate_world(store)
        .map_err(|_| RejectCode::InvalidData)?;
    entities
        .prepare_lifecycle(&change, schemas)
        .map(Some)
        .map_err(|_| RejectCode::Conflict)
}

/// Recovery rejects orphaned, duplicated or relocated block instances. Ordinary
/// entities have no implicit block relationship and pass through unchanged.
pub(crate) fn validate(
    store: &SectionStore,
    entities: &EntityStore,
    registry: &BlockStateRegistry,
) -> Result<(), torse_world::ExchangeError> {
    let mut seen = std::collections::BTreeSet::new();
    let invalid = || torse_world::ExchangeError::Store {
        detail: "invalid block instance binding".into(),
    };
    for (_, entity) in entities.iter() {
        let Some(binding) = BlockInstance::of(entity).map_err(|_| invalid())? else {
            continue;
        };
        let state = store.query(binding.pos).ready().ok_or_else(invalid)?;
        let definition = registry.definition(state).ok_or_else(invalid)?;
        if definition.id != binding.block
            || definition.component_defaults.is_empty()
            || !seen.insert(binding.pos)
        {
            return Err(invalid());
        }
    }
    let mut required = 0usize;
    for pos in store.positions() {
        let section = store.section(pos).ok_or_else(invalid)?;
        let needs_instance = |state| {
            registry
                .definition(state)
                .is_some_and(|d| !d.component_defaults.is_empty())
        };
        let count = match section.storage() {
            torse_world::SectionStorage::Uniform(state) => {
                if needs_instance(*state) {
                    (store.side() as usize).pow(3)
                } else {
                    0
                }
            }
            torse_world::SectionStorage::Dense(cells) => {
                cells.iter().filter(|s| needs_instance(**s)).count()
            }
            torse_world::SectionStorage::Palette(_) => {
                section.values().filter(|s| needs_instance(*s)).count()
            }
        };
        required = required.checked_add(count).ok_or_else(invalid)?;
    }
    if required != seen.len() {
        return Err(invalid());
    }
    Ok(())
}
