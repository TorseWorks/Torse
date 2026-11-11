//! Deterministic height/cavity variation for atomic installation measurements.
use torse_server::{bootstrap as base, execution::*, section_pages::SectionPages, *};
use torse_sim::{WorldAuthority, replica::ReplicaScope};
use torse_types::*;
use torse_world::*;
pub const SEED: u64 = 0x7391_210b_9012_82ac;
fn noise(x: i64, y: i64, z: i64) -> u64 {
    let mut n = SEED
        ^ (x as u64).wrapping_mul(0x9e3779b185ebca87)
        ^ (z as u64).wrapping_mul(0xc2b2ae3d27d4eb4f)
        ^ (y as u64).wrapping_mul(0x165667b19e3779f9);
    n ^= n >> 30;
    n = n.wrapping_mul(0xbf58476d1ce4e5b9);
    n ^ (n >> 27)
}
pub fn budget() -> OutputBudget {
    OutputBudget {
        max_items: 2_000_000,
        ..base::budget()
    }
}
pub fn limits() -> ExecutionLimits {
    let mut limits = base::execution_limits();
    limits.runtime.max_items = 200_000;
    limits.replicas.max_sections = 600;
    limits.replicas.snapshot = budget();
    limits.replicas.queue = budget();
    limits
}
pub fn file_policy() -> FileServerConfig {
    let mut config = base::file_policy();
    config.server.snapshot_budget = budget();
    config.checkpoint.codec_budget = budget();
    config
}
pub fn scope(center: i64) -> ReplicaScope {
    ReplicaScope {
        sections: (center - 2..=center + 2)
            .flat_map(|x| {
                (-2..=2).flat_map(move |z| (-4..20).map(move |y| SectionPos::new(x, y, z)))
            })
            .collect(),
    }
}
pub fn create(
    directory: WorldDirectory,
) -> Result<(base::Files, SectionPages, SnapshotEntity), Fault> {
    let (model, actions) = base::world()?;
    let template = model
        .capture_section(SectionPos::ZERO, base::budget())?
        .entities
        .remove(0);
    let compatibility = model.coordinator().compatibility();
    let world = WorldAuthority::new(
        base::WORLD,
        model.registry().clone(),
        model.schemas().clone(),
        16,
        WorldBounds::new(-64, 320).unwrap(),
        compatibility,
    )
    .unwrap();
    let root = directory.root().join("sections");
    std::fs::create_dir(&root).unwrap();
    let pages = SectionPages::new(root, base::budget(), base::codec_limits(), 4096)?;
    let server = base::Files::create_file(
        directory,
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        file_policy(),
        |binding| ExecutionHost::new(binding, world, actions, Vec::new(), limits()),
    )?;
    Ok((server, pages, template))
}
pub fn column(
    host: &ExecutionHost,
    x: i64,
    z: i64,
    template: &SnapshotEntity,
    uniform: bool,
) -> Vec<Snapshot> {
    let world = host.world();
    let mut next = world.entities().next_entity_id();
    let stone = world
        .registry()
        .default_state(&base::key("base:stone"))
        .unwrap();
    (-4..20)
        .map(|sy| {
            let mut cells = Vec::with_capacity(4096);
            for y in 0..16 {
                for lz in 0..16 {
                    for lx in 0..16 {
                        let wx = x * 16 + lx;
                        let wy = sy * 16 + y;
                        let wz = z * 16 + lz;
                        let height = 176 + (noise(wx, 0, wz) % 48) as i64;
                        let cavity = (-32..176).contains(&wy) && noise(wx, wy, wz) % 11 < 3;
                        cells.push(if uniform || (wy < height && !cavity) {
                            stone
                        } else {
                            BlockStateId::EMPTY
                        });
                    }
                }
            }
            let fill = cells[0];
            let all = cells.iter().all(|s| *s == fill);
            let populated = cells.iter().any(|s| !s.is_empty());
            let entities = if sy % 8 == 0 {
                let mut entity = template.clone();
                entity.id = EntityId(next);
                next += 1;
                entity.position_bits = Position::new(
                    x as f64 * 16. + 0.5,
                    sy as f64 * 16. + 0.5,
                    z as f64 * 16. + 0.5,
                )
                .to_bits();
                vec![entity]
            } else {
                vec![]
            };
            Snapshot {
                meta: torse_sim::streaming::page_meta(world),
                side: 16,
                bounds: world.store().bounds(),
                next_entity_id: next,
                states: if populated {
                    vec![StateEntry {
                        id: stone,
                        block: base::key("base:stone"),
                        values: vec![],
                    }]
                } else {
                    vec![]
                },
                sections: vec![SnapshotSection {
                    pos: SectionPos::new(x, sy, z),
                    cells: if all {
                        SnapshotCells::Uniform(fill)
                    } else {
                        SnapshotCells::Dense(cells)
                    },
                }],
                entities,
            }
        })
        .collect()
}
