//! Public file-world streaming fixtures shared by regressions and measurements.
use crate::support::Directory;
use torse_server::{bootstrap as base, execution::*, section_pages::SectionPages};
use torse_sim::WorldAuthority;
use torse_types::*;
use torse_world::*;
pub fn pages(dir: &Directory) -> SectionPages {
    let root = dir.0.join("sections");
    std::fs::create_dir_all(&root).unwrap();
    SectionPages::new(root, base::budget(), base::codec_limits(), 64).unwrap()
}

pub fn create(dir: &Directory) -> base::Files {
    let (mut world, actions) = base::world().unwrap();
    let schemas = world.schemas().clone();
    let handle = world.entities().iter().next().unwrap().0;
    world
        .entities_mut()
        .components_mut(handle)
        .unwrap()
        .insert(
            &schemas,
            &collider_schema(),
            EntityCollider::Box {
                half: [300_000, 900_000, 300_000],
            }
            .fields(),
        )
        .unwrap();
    let compatibility = world.coordinator().compatibility();
    base::Files::create_file(
        dir.acquire(),
        compatibility,
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            ExecutionHost::new(
                binding,
                world,
                actions,
                Vec::new(),
                base::execution_limits(),
            )
        },
    )
    .unwrap()
}

pub fn open(dir: &Directory, pages: SectionPages) -> Result<base::Files, Fault> {
    let (world, actions) = base::world().unwrap();
    base::Files::open_file(
        dir.acquire(),
        world.coordinator().compatibility(),
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                world.registry(),
                world.schemas(),
                actions,
                Vec::new(),
                base::execution_limits(),
            )
            .with_section_pages(pages))
        },
    )
}

pub fn generated(world: &WorldAuthority, pos: SectionPos) -> Snapshot {
    let mut page = world
        .capture_section(SectionPos::ZERO, base::budget())
        .unwrap();
    page.sections[0].pos = pos;
    page.sections[0].cells = SnapshotCells::Uniform(BlockStateId::EMPTY);
    page.entities.clear();
    page.states.clear();
    page
}
pub fn generated_with_entity(world: &WorldAuthority, pos: SectionPos) -> Snapshot {
    let mut page = generated(world, pos);
    let first = page.next_entity_id;
    page.entities.push(SnapshotEntity {
        id: EntityId(first),
        position_bits: Position::new(pos.x as f64 * 16.0 + 1.5, 1.5, 1.5).to_bits(),
        components: Vec::new(),
    });
    page.next_entity_id = first + 1;
    page
}
