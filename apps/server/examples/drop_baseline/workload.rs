//! Explicit item-body fixtures shared by the benchmark and regression tests.
use torse_inventory::ItemStack;
use torse_items::{Catalog, drops, fixture_catalog};
use torse_server::HostAuthority;
use torse_server::{bootstrap as base, execution::*};
use torse_sim::{WorldAuthority, action::ActionService};
use torse_types::*;
use torse_world::*;

pub fn world(bodies: usize, clustered: bool) -> (WorldAuthority, ActionService, Catalog) {
    assert!(bodies <= 64);
    let (mut world, actions) = base::world().unwrap();
    base::fill_floor(&mut world).unwrap();
    let catalog = fixture_catalog(world.registry())
        .unwrap()
        .with_world_drops();
    let schemas = world.schemas().clone();
    for index in 0..bodies {
        let cell = if clustered && index < 16 {
            index / 2
        } else {
            index
        };
        let extra = if clustered && index < 16 && index % 2 == 1 {
            0.2
        } else {
            0.0
        };
        let handle = world
            .entities_mut()
            .spawn(Position::new(
                1.5 + (cell % 10) as f64 * 3.0 + extra,
                8.0,
                1.5 + (cell / 10) as f64 * 2.0,
            ))
            .unwrap();
        let data = if clustered && index < 8 {
            [(
                base::key("test:instance/variant"),
                ComponentValue::U32(index as u32),
            )]
            .into()
        } else {
            ComponentFields::new()
        };
        let components = world.entities_mut().components_mut(handle).unwrap();
        components
            .insert(
                &schemas,
                &drops::schema(),
                drops::DropState {
                    stack: ItemStack {
                        item: base::key("base:stone"),
                        count: 2,
                        data,
                    },
                    velocity: [0, -8_000_000, 0],
                }
                .fields(),
            )
            .unwrap();
        components
            .insert(
                &schemas,
                &collider_schema(),
                EntityCollider::Box { half: [125_000; 3] }.fields(),
            )
            .unwrap();
    }
    (world, actions, catalog)
}

pub fn rules(catalog: Catalog) -> Vec<Box<dyn DurableRule>> {
    vec![Box::new(torse_server::item_drops::DropPhysics::new(
        catalog,
    ))]
}

pub fn snapshot(host: &ExecutionHost) -> Vec<u8> {
    use torse_codec::StorageCodec;
    use torse_sim::durable::ExecutionSnapshot;
    let meta = SnapshotMeta {
        cursor: host.cursor(),
        compatibility: host.binding().compatibility,
        resume_at: host.resume_point(),
    };
    let snapshot = WorldSnapshot {
        meta,
        state: ExecutionSnapshot {
            parked: Default::default(),
            world: host.world().capture_snapshot(meta).unwrap(),
            runtime: host.runtime().clone(),
        },
    };
    let mut out = Vec::new();
    base::Codec::new(base::codec_limits())
        .unwrap()
        .encode_snapshot(&snapshot, &mut out, base::budget())
        .unwrap();
    out
}
