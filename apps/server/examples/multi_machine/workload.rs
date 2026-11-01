//! Real Wasm machine fixtures using the normal durable adapter and limits.
use std::collections::BTreeSet;
use std::path::PathBuf;
use torse_codec::{StorageCodec, wire};
use torse_inventory::{Inventory, ItemCatalog, ItemStack};
use torse_server::{HostAuthority, bootstrap as base, execution::*, mods};
use torse_sim::WorldAuthority;
use torse_types::*;
use torse_world::{ComponentValue, Position};

#[derive(Clone, Copy)]
pub struct Load {
    pub machines: usize,
    pub shared: bool,
}

pub fn entity_field(world: &WorldAuthority, id: EntityId, field: &str) -> EntityId {
    let entity = world
        .entities()
        .get(world.entities().handle_of(id).unwrap())
        .unwrap();
    match entity
        .components
        .component(&base::key("machine:schema/state"))
        .unwrap()
        .get(&base::key(field))
        .unwrap()
    {
        ComponentValue::Entity(id) => *id,
        _ => panic!("machine port field"),
    }
}

impl Load {
    pub fn assemble(self) -> Result<mods::ModWorld, Fault> {
        let mut game = mods::mod_world_with_layout(
            &[PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/mods/machine")],
            false,
            &[],
            mods::ModLayout {
                machines_per_package: self.machines,
                shared_ports: self.shared,
            },
        )?;
        let inputs: BTreeSet<_> = game
            .machines
            .iter()
            .map(|(_, id)| entity_field(&game.world, *id, "machine:field/input"))
            .collect();
        let schemas = game.world.schemas().clone();
        let catalog = catalog();
        for id in inputs {
            let mut inventory = Inventory::new(2).unwrap();
            inventory
                .set(
                    0,
                    Some(ItemStack {
                        data: Default::default(),
                        item: base::key("base:stone"),
                        count: 64,
                    }),
                    &catalog,
                )
                .unwrap();
            let handle = game.world.entities().handle_of(id).unwrap();
            game.world
                .entities_mut()
                .components_mut(handle)
                .unwrap()
                .insert(&schemas, &torse_inventory::schema(), inventory.fields())
                .map_err(|e| Fault::invalid_data(e.to_string()))?;
        }
        // Keep the fixture inside the two standard loaded sections.
        assert!(game.world.entities().iter().all(|(_, e)| {
            let Position { x, y, z } = e.position;
            (0.0..32.0).contains(&x) && (0.0..16.0).contains(&z) && y == 2.0
        }));
        Ok(game)
    }

    pub fn verify(self, host: &ExecutionHost, ticks: u64) -> (u32, u32) {
        let world = host.world();
        let ids: Vec<_> = world
            .entities()
            .iter()
            .filter_map(|(_, e)| {
                e.components
                    .component(&base::key("machine:schema/state"))
                    .map(|_| e.id)
            })
            .collect();
        assert_eq!(ids.len(), self.machines);
        let mut inputs = BTreeSet::new();
        let mut outputs = BTreeSet::new();
        let active = ticks.saturating_sub(1);
        for (index, id) in ids.iter().enumerate() {
            inputs.insert(entity_field(world, *id, "machine:field/input"));
            outputs.insert(entity_field(world, *id, "machine:field/output"));
            let entity = world
                .entities()
                .get(world.entities().handle_of(*id).unwrap())
                .unwrap();
            let fields = entity
                .components
                .component(&base::key("machine:schema/state"))
                .unwrap();
            let progress = if self.shared && index != 0 && active >= 4 {
                3
            } else {
                (active % 4) as u32
            };
            assert_eq!(
                fields[&base::key("machine:field/progress")],
                ComponentValue::U32(progress)
            );
        }
        let input = inputs.iter().map(|id| count(world, *id)).sum::<u32>();
        let output = outputs.iter().map(|id| count(world, *id)).sum::<u32>();
        assert_eq!(output, (active / 4) as u32 * outputs.len() as u32);
        assert_eq!(input + output, inputs.len() as u32 * 64);
        let expected_events = if active >= 4 && self.shared {
            1
        } else {
            self.machines
        };
        assert_eq!(host.runtime().events.len(), expected_events);
        (input, output)
    }
}

fn catalog() -> ItemCatalog {
    ItemCatalog::new([(base::key("base:stone"), 64)]).unwrap()
}
fn count(world: &WorldAuthority, id: EntityId) -> u32 {
    let entity = world
        .entities()
        .get(world.entities().handle_of(id).unwrap())
        .unwrap();
    Inventory::from_fields(
        entity
            .components
            .component(&torse_inventory::schema())
            .unwrap(),
        &catalog(),
    )
    .unwrap()
    .slots()
    .iter()
    .flatten()
    .map(|s| s.count)
    .sum()
}
pub fn state(host: &ExecutionHost) -> Result<Vec<u8>, Fault> {
    let meta = SnapshotMeta {
        cursor: host.cursor(),
        compatibility: host.binding().compatibility,
        resume_at: host.resume_point(),
    };
    let snapshot = WorldSnapshot {
        meta,
        state: torse_sim::durable::ExecutionSnapshot {
            parked: Default::default(),
            world: host
                .world()
                .capture_snapshot(meta)
                .map_err(|e| Fault::invalid_data(format!("{e:?}")))?,
            runtime: host.runtime().clone(),
        },
    };
    let mut bytes = vec![];
    base::Codec::new(base::codec_limits())?.encode_snapshot(
        &snapshot,
        &mut bytes,
        base::budget(),
    )?;
    Ok(bytes)
}
pub fn encoded<T: wire::Wire>(value: &T) -> Result<Vec<u8>, Fault> {
    let mut bytes = vec![];
    wire::encode(value, &mut bytes, base::budget(), base::codec_limits())?;
    Ok(bytes)
}
