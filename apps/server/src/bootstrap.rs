//! Finite demonstration game and explicit policies used by the executable.
use crate::execution::{ExecutionHost, ExecutionLimits, ExecutionModel};
use crate::network::NetworkLimits;
use crate::{FileServer, FileServerConfig, ServerConfig};
use torse_codec::{CodecLimits, wire::ExecutionCodec};
use torse_content::{compile, m0_fixture_pack};
use torse_sim::action::ActionService;
use torse_sim::replica::{ReplicaLimits, ReplicaScope};
use torse_sim::{WorldAuthority, block_actions};
use torse_types::*;
use torse_world::*;
pub const WORLD: WorldId = WorldId(7);
pub const TICK_PERIOD: std::time::Duration = std::time::Duration::from_millis(50);
pub type Codec = ExecutionCodec<ExecutionModel>;
pub type Files = FileServer<ExecutionHost, Codec>;
pub fn key(value: &str) -> ContentKey {
    ContentKey::parse(value).expect("static key")
}
pub fn codec_limits() -> CodecLimits {
    CodecLimits {
        max_frame_bytes: 8_388_608,
        max_field_bytes: 262_144,
        max_depth: 12,
    }
}
pub fn budget() -> OutputBudget {
    OutputBudget {
        max_items: 200_000,
        max_bytes: 8_388_608,
    }
}
pub fn execution_limits() -> ExecutionLimits {
    ExecutionLimits {
        pending_requests: 64,
        pending_bytes: 262_144,
        input: OutputBudget {
            max_items: 1024,
            max_bytes: 16_384,
        },
        sessions: 32,
        receipt_window: 32,
        phase_tasks: 128,
        events: 128,
        runtime: OutputBudget {
            max_items: 32_000,
            max_bytes: 2_097_152,
        },
        replicas: ReplicaLimits {
            max_subscriptions: 32,
            max_sections: 4,
            max_queued_batches: 16,
            snapshot: budget(),
            queue: budget(),
        },
        codec: codec_limits(),
    }
}
pub fn file_policy() -> FileServerConfig {
    let limits = codec_limits();
    let b = budget();
    FileServerConfig {
        server: ServerConfig {
            snapshot_budget: b,
            max_invalidations: 32,
            shutdown_max_steps: 4096,
            checkpoint_predecessors: 1,
        },
        journal: torse_storage::JournalConfig {
            max_segment_bytes: 16_777_216,
            max_segment_files: 1024,
            max_pending_records: 4,
            max_pending_bytes: 16_777_216,
            codec_budget: b,
            codec_limits: limits,
        },
        checkpoint: torse_storage::CheckpointConfig {
            codec_limits: limits,
            codec_budget: b,
            max_manifest_bytes: 1_048_576,
            max_snapshot_files: 4,
        },
    }
}
pub fn network_limits() -> NetworkLimits {
    NetworkLimits {
        peers: 16,
        movement_inputs: 32,
        movement_bytes: 16_384,
        requests_per_tick: 32,
        frames_per_poll: 16,
        outstanding: 32,
        subscriptions: 2,
        idle_ticks: 1200,
        budget: budget(),
        codec: codec_limits(),
    }
}
/// Replace the demo sections with a stone floor at the lowest layer, so a
/// connected client has terrain to mirror. Only valid before the host is
/// created; this is initial world state, not a runtime edit.
pub fn fill_floor(world: &mut WorldAuthority) -> Result<(), Fault> {
    let stone = world
        .registry()
        .default_state(&key("base:stone"))
        .ok_or_else(|| Fault::invalid_data("missing base:stone"))?;
    for pos in scope().sections {
        let mut section = torse_world::Section::new(16, torse_world::BlockStateId::EMPTY)
            .map_err(|e| Fault::invalid_data(format!("section: {e}")))?;
        for x in 0..16u32 {
            for z in 0..16u32 {
                section
                    .set(torse_types::LocalPos { x, y: 0, z }, stone)
                    .map_err(|e| Fault::invalid_data(format!("cell: {e}")))?;
            }
        }
        let store = world.store_mut();
        let _ = store.unload(pos);
        store
            .request(pos)
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        store
            .install(pos, section)
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        store
            .finish_load(pos)
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
    }
    Ok(())
}

pub fn scope() -> ReplicaScope {
    ReplicaScope {
        sections: [SectionPos::ZERO, SectionPos::new(1, 0, 0)].into(),
    }
}
/// Initial-state assembly only. Live spawning must go through a durable transaction.
pub fn spawn_players(
    world: &mut WorldAuthority,
    count: usize,
) -> Result<Vec<torse_types::EntityId>, Fault> {
    if count > 16 || world.seq().0 != 0 {
        return Err(Fault::invalid_data("initial player count must be 0..16"));
    }
    let schemas = world.schemas().clone();
    let mut ids = Vec::new();
    for index in 0..count {
        let handle = world
            .entities_mut()
            .spawn(Position::new(
                4.5 + (index % 8) as f64 * 2.0,
                3.0,
                4.5 + (index / 8) as f64 * 2.0,
            ))
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        let id = world.entities().get(handle).expect("spawned player").id;
        let components = world
            .entities_mut()
            .components_mut(handle)
            .expect("spawned player");
        components
            .insert(
                &schemas,
                &torse_world::collider_schema(),
                torse_world::EntityCollider::Box {
                    half: [300_000, 900_000, 300_000],
                }
                .fields(),
            )
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        components
            .insert(
                &schemas,
                &torse_world::player_motion_schema(),
                torse_world::PlayerMotion::default().fields(),
            )
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        if schemas.get(&key("items:schema/place")).is_some() {
            let catalog = torse_items::fixture_catalog(world.registry())
                .map_err(|e| Fault::invalid_data(format!("items: {e:?}")))?;
            let stock = catalog
                .starter(36)
                .map_err(|e| Fault::invalid_data(format!("stock: {e:?}")))?;
            world
                .entities_mut()
                .components_mut(handle)
                .expect("player")
                .insert(&schemas, &torse_inventory::schema(), stock.fields())
                .map_err(|e| Fault::invalid_data(e.to_string()))?;
        }
        ids.push(id);
    }
    Ok(ids)
}
pub fn world() -> Result<(WorldAuthority, ActionService), Fault> {
    assemble(
        torse_inventory::ItemCatalog::new(
            ["base:stone", "base:glass_slab", "base:plant"].map(|id| (key(id), 64)),
        )
        .map_err(|e| Fault::invalid_data(format!("catalog: {e:?}")))?,
        true,
    )
}
/// Freeze the exact item rules used by native handlers into recovery and admission.
pub fn world_with_catalog(
    catalog: torse_inventory::ItemCatalog,
) -> Result<(WorldAuthority, ActionService), Fault> {
    assemble(catalog, false)
}
fn assemble(
    catalog: torse_inventory::ItemCatalog,
    items: bool,
) -> Result<(WorldAuthority, ActionService), Fault> {
    let mut pack = m0_fixture_pack();
    block_actions::register_content(&mut pack);
    torse_inventory::register_content(&mut pack);
    if items {
        torse_items::register_content(&mut pack);
    }
    let content = compile(pack).map_err(|e| Fault::invalid_data(format!("demo content: {e:?}")))?;
    let registry =
        BlockStateRegistry::build(&content).map_err(|e| Fault::invalid_data(e.to_string()))?;
    let item_catalog = if items {
        Some(
            torse_items::fixture_catalog(&registry)
                .map_err(|e| Fault::invalid_data(format!("items: {e:?}")))?
                .with_world_drops(),
        )
    } else {
        None
    };
    let base = Compatibility {
        authority_schema: 4,
        rules_digest: content.authority_digest,
        content_digest: content.presentation_digest,
    };
    let compatibility = item_catalog.as_ref().map_or_else(
        || catalog.bind_compatibility(base),
        |items| items.bind_compatibility(base),
    );
    let (schemas, compatibility) =
        ComponentSchemas::assemble(&content, ComponentConfiguration::default(), compatibility)
            .map_err(|error| Fault::invalid_data(error.to_string()))?;
    let mut handlers = if items {
        torse_items::block_handlers()
    } else {
        block_actions::handlers()
    };
    handlers.extend(torse_inventory::handlers(catalog.clone()));
    let mut actions = block_actions::descriptors();
    actions.extend(torse_inventory::descriptors());
    if let Some(catalog) = item_catalog {
        handlers.extend(torse_items::handlers(catalog));
        actions.extend(torse_items::descriptors());
    }
    let service = ActionService::bind(actions, handlers, &schemas)
        .map_err(|e| Fault::invalid_data(e.to_string()))?;
    let mut world = WorldAuthority::new(
        WORLD,
        registry,
        schemas.clone(),
        16,
        WorldBounds::new(0, 32).expect("bounds"),
        compatibility,
    )
    .map_err(|e| Fault::invalid_data(e.to_string()))?;
    for pos in scope().sections {
        world
            .store_mut()
            .request(pos)
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        world
            .store_mut()
            .install(pos, Section::empty(16).expect("side"))
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        world
            .store_mut()
            .finish_load(pos)
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
    }
    for x in [2.0, 3.0] {
        let handle = world
            .entities_mut()
            .spawn(Position::new(x, 2.0, 2.0))
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
        let mut inventory = torse_inventory::Inventory::new(2)
            .map_err(|e| Fault::invalid_data(format!("inventory: {e:?}")))?;
        if x == 2.0 {
            inventory
                .set(
                    0,
                    Some(torse_inventory::ItemStack {
                        item: key("base:stone"),
                        count: 1,
                    }),
                    &catalog,
                )
                .map_err(|e| Fault::invalid_data(format!("stock: {e:?}")))?;
        }
        world
            .entities_mut()
            .components_mut(handle)
            .expect("entity exists")
            .insert(&schemas, &torse_inventory::schema(), inventory.fields())
            .map_err(|e| Fault::invalid_data(e.to_string()))?;
    }
    Ok((world, service))
}
