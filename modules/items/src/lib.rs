//! Optional item definitions and atomic block/inventory interaction policies.
#![forbid(unsafe_code)]

mod actions;
mod drop_actions;
pub mod drops;
pub mod supply;
pub use actions::{block_handlers, descriptors, handlers, register_content};
pub use drop_actions::{pickup_call, throw_call, with_position};
use std::collections::BTreeMap;
use torse_inventory::{Inventory, ItemCatalog, ItemStack};
use torse_types::{Compatibility, ContentKey, Digest, RejectCode};
use torse_world::{BlockStateId, BlockStateRegistry};

pub fn key(value: &str) -> ContentKey {
    ContentKey::parse(value).expect("item module key")
}
pub fn owner() -> ContentKey {
    key("items:module")
}

fn digest_data(hash: &mut sha2::Sha256, fields: &torse_world::ComponentFields) {
    use sha2::Digest as _;
    use torse_world::ComponentValue as Value;
    fn text(hash: &mut sha2::Sha256, value: &str) {
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value.as_bytes());
    }
    fn value(hash: &mut sha2::Sha256, data: &Value) {
        match data {
            Value::Bool(v) => hash.update([0, u8::from(*v)]),
            Value::U32(v) => {
                hash.update([1]);
                hash.update(v.to_le_bytes());
            }
            Value::I64(v) => {
                hash.update([2]);
                hash.update(v.to_le_bytes());
            }
            Value::Content(v) => {
                hash.update([3]);
                text(hash, &v.to_string());
            }
            Value::Entity(v) => {
                hash.update([4]);
                hash.update(v.0.to_le_bytes());
            }
            Value::Text(v) => {
                hash.update([5]);
                text(hash, v);
            }
            Value::List(values) => {
                hash.update([6]);
                hash.update((values.len() as u64).to_le_bytes());
                for entry in values {
                    value(hash, entry);
                }
            }
            Value::Record(fields) => {
                hash.update([7]);
                digest_data(hash, fields);
            }
        }
    }
    hash.update((fields.len() as u64).to_le_bytes());
    for (key, data) in fields {
        text(hash, &key.to_string());
        value(hash, data);
    }
}

/// Stable item identity, stack policy and optional placement association.
#[derive(Clone, Debug)]
pub struct ItemDefinition {
    pub id: ContentKey,
    pub limit: u32,
    pub block: Option<ContentKey>,
}

/// Frozen game rules. Runtime block-state numbers never enter its digest.
#[derive(Clone, Debug)]
pub struct Catalog {
    pub(crate) definitions: BTreeMap<ContentKey, ItemDefinition>,
    drops: BTreeMap<ContentKey, ItemStack>,
    pub(crate) states: BTreeMap<BlockStateId, ItemStack>,
    pub(crate) stacks: ItemCatalog,
    pub(crate) world_drops: bool,
}
impl Catalog {
    /// Bind the same inventory and physical-drop rules to storage and recovery.
    pub fn bind_schemas(
        &self,
        schemas: &mut torse_world::ComponentSchemas,
    ) -> Result<(), torse_world::ComponentError> {
        self.stacks.bind_schemas(schemas)?;
        schemas.register_validator(
            &owner(),
            &drops::schema(),
            std::sync::Arc::new(self.clone()),
        )
    }
    /// Conservative retained catalog charge for a host's module budget.
    pub fn accounted_bytes(&self) -> u64 {
        32768
            + (self.definitions.len() + self.drops.len()) as u64 * 1024
            + self.states.len() as u64 * 768
            + self
                .drops
                .values()
                .chain(self.states.values())
                .map(|stack| {
                    torse_world::component_size(&stack.data)
                        .unwrap_or(torse_inventory::MAX_INSTANCE_BYTES)
                        .saturating_mul(64)
                })
                .sum::<u64>()
    }
    pub fn new(
        definitions: impl IntoIterator<Item = ItemDefinition>,
        drops: impl IntoIterator<Item = (ContentKey, ItemStack)>,
        registry: &BlockStateRegistry,
    ) -> Result<Self, RejectCode> {
        let mut entries = BTreeMap::new();
        for entry in definitions {
            if entries.len() == 4096
                || entry
                    .block
                    .as_ref()
                    .is_some_and(|id| registry.default_state(id).is_none())
                || entries.insert(entry.id.clone(), entry).is_some()
            {
                return Err(RejectCode::InvalidData);
            }
        }
        if entries.is_empty() {
            return Err(RejectCode::InvalidData);
        }
        let stacks = ItemCatalog::new(
            entries
                .values()
                .map(|entry| (entry.id.clone(), entry.limit)),
        )?;
        let mut drop_map = BTreeMap::new();
        for (block, stack) in drops {
            if drop_map.len() == 4096
                || registry.default_state(&block).is_none()
                || stacks.validate_stack(&stack).is_err()
                || drop_map.insert(block, stack).is_some()
            {
                return Err(RejectCode::InvalidData);
            }
        }
        let mut states = BTreeMap::new();
        for index in 0..registry.state_count() {
            let state = BlockStateId(index as u16);
            if let Some(stack) = registry
                .definition(state)
                .and_then(|definition| drop_map.get(&definition.id))
            {
                states.insert(state, stack.clone());
            }
        }
        Ok(Self {
            definitions: entries,
            drops: drop_map,
            states,
            stacks,
            world_drops: false,
        })
    }
    pub fn definitions(&self) -> impl Iterator<Item = &ItemDefinition> {
        self.definitions.values()
    }
    /// Opt into physical harvest drops instead of direct insertion into stock.
    pub fn with_world_drops(mut self) -> Self {
        self.world_drops = true;
        self
    }
    pub fn stack_rules(&self) -> &ItemCatalog {
        &self.stacks
    }
    pub fn block(&self, item: &ContentKey) -> Option<&ContentKey> {
        self.definitions.get(item)?.block.as_ref()
    }
    pub fn bind_compatibility(&self, base: Compatibility) -> Compatibility {
        use sha2::{Digest as _, Sha256};
        let mut result = self.stacks.bind_compatibility(base);
        let mut hash = Sha256::new();
        hash.update(b"torse:items/rules/v2\0");
        if self.world_drops {
            hash.update(b"physical-drops/v1\0");
        }
        hash.update(result.rules_digest.0);
        hash.update((self.definitions.len() as u64).to_le_bytes());
        for entry in self.definitions.values() {
            let id = entry.id.to_string();
            hash.update((id.len() as u64).to_le_bytes());
            hash.update(id.as_bytes());
            let block = entry
                .block
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            hash.update((block.len() as u64).to_le_bytes());
            hash.update(block.as_bytes());
        }
        hash.update((self.drops.len() as u64).to_le_bytes());
        for (block, stack) in &self.drops {
            for id in [block, &stack.item] {
                let text = id.to_string();
                hash.update((text.len() as u64).to_le_bytes());
                hash.update(text.as_bytes());
            }
            hash.update(stack.count.to_le_bytes());
            digest_data(&mut hash, &stack.data);
        }
        result.rules_digest = Digest(hash.finalize().into());
        result
    }
    /// Explicit initial stock for a newly created demo player, never a reconnect refill.
    pub fn starter(&self, slots: usize) -> Result<Inventory, RejectCode> {
        let mut inventory = Inventory::new(slots)?;
        for (slot, entry) in self.definitions.values().take(slots).enumerate() {
            inventory.set(
                slot,
                Some(ItemStack {
                    data: Default::default(),
                    item: entry.id.clone(),
                    count: entry.limit.min(64),
                }),
                &self.stacks,
            )?;
        }
        Ok(inventory)
    }
}

impl torse_world::ComponentValidator for Catalog {
    fn validate(
        &self,
        fields: &torse_world::ComponentFields,
    ) -> Result<(), torse_world::ComponentError> {
        drops::DropState::from_fields(fields, self)
            .map(|_| ())
            .map_err(|_| torse_world::ComponentError::InvalidConfiguration {
                id: drops::schema(),
            })
    }
    fn rules_digest(&self) -> Digest {
        use sha2::Digest as _;
        let mut hash = sha2::Sha256::new();
        hash.update(b"torse:items/drop-validator/v2\0");
        hash.update(self.stacks.rules_digest().0);
        Digest(hash.finalize().into())
    }
}

/// The small demonstration's item policy; applications may supply another catalog.
pub fn fixture_catalog(registry: &BlockStateRegistry) -> Result<Catalog, RejectCode> {
    Catalog::new(
        ["base:stone", "base:glass_slab", "base:plant"].map(|id| ItemDefinition {
            id: key(id),
            limit: 64,
            block: Some(key(id)),
        }),
        [
            ("base:stone", "base:stone", 1),
            ("base:glass_slab", "base:glass_slab", 1),
            ("base:glass", "base:glass_slab", 2),
            ("base:plant", "base:plant", 1),
        ]
        .map(|(block, item, count)| {
            (
                key(block),
                ItemStack {
                    data: Default::default(),
                    item: key(item),
                    count,
                },
            )
        }),
        registry,
    )
}

/// One item and one recovered unit per registered block for a simple game pack.
pub fn block_catalog(registry: &BlockStateRegistry) -> Result<Catalog, RejectCode> {
    let blocks: std::collections::BTreeSet<_> = (0..registry.state_count())
        .filter_map(|index| registry.definition(BlockStateId(index as u16)))
        .map(|definition| definition.id.clone())
        .collect();
    Catalog::new(
        blocks.iter().map(|id| ItemDefinition {
            id: id.clone(),
            limit: 64,
            block: Some(id.clone()),
        }),
        blocks.iter().map(|id| {
            (
                id.clone(),
                ItemStack {
                    data: Default::default(),
                    item: id.clone(),
                    count: 1,
                },
            )
        }),
        registry,
    )
}

/// Untrusted request data; the host still derives all entity and cell grants.
pub fn edit_call(
    inventory: torse_types::EntityId,
    place: bool,
    slot: u32,
    pos: torse_types::BlockPos,
    half: u32,
) -> torse_sim::durable::Call {
    use torse_world::ComponentValue as Value;
    torse_sim::durable::Call {
        action: key(if place {
            "items:action/place"
        } else {
            "items:action/harvest"
        }),
        version: 1,
        target: torse_sim::action::ActionTarget::Block(pos),
        parameters: [
            (key("items:field/inventory"), Value::Entity(inventory)),
            (key("items:field/place"), Value::Bool(place)),
            (key("items:field/slot"), Value::U32(slot)),
            (key("items:field/half"), Value::U32(half)),
        ]
        .into(),
    }
}
pub fn swap_call(
    inventory: torse_types::EntityId,
    slot: u32,
    other: u32,
) -> torse_sim::durable::Call {
    torse_sim::durable::Call {
        action: key("items:action/swap"),
        version: 1,
        target: torse_sim::action::ActionTarget::Entity(inventory),
        parameters: [
            (
                key("items:field/slot"),
                torse_world::ComponentValue::U32(slot),
            ),
            (
                key("items:field/other"),
                torse_world::ComponentValue::U32(other),
            ),
        ]
        .into(),
    }
}
