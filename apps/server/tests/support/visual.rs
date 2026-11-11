//! Saved, edited terrain outside a two-section simulation window.
#[path = "interest.rs"]
#[allow(dead_code)]
mod interest;
#[path = "streaming.rs"]
#[allow(dead_code)]
mod streaming;
use crate::support::Directory;
pub use interest::{actor, connect, drain, send};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use torse_net::{
    MemoryTransport,
    terrain::{TerrainLimits, TerrainMirror},
};
use torse_server::{
    NoReplicas, bootstrap as base,
    network::*,
    section_pages::SectionPages,
    visual::{VisualTerrainApproval, VisualTerrainLimits},
};
use torse_sim::replica::*;
use torse_types::*;
use torse_world::*;

pub fn scope(start: i64, count: i64) -> ReplicaScope {
    ReplicaScope {
        sections: (start..start + count)
            .map(|x| SectionPos::new(x, 0, 0))
            .collect(),
    }
}
pub const APPROVAL_MIN: SectionPos = SectionPos::new(-32, -4, -32);
pub const APPROVAL_MAX: SectionPos = SectionPos::new(32, 19, 32);

pub fn approval() -> VisualTerrainApproval {
    VisualTerrainApproval::rectangular(APPROVAL_MIN, APPROVAL_MAX).unwrap()
}

pub fn limits() -> VisualTerrainLimits {
    VisualTerrainLimits {
        max_sections: 8,
        cache_bytes: 256 * 1024,
        io_bytes: base::budget().max_bytes * 4,
        batch_bytes: 128 * 1024,
        pages_per_poll: 2,
    }
}
pub fn create(
    dir: &Directory,
) -> (
    base::Files,
    SectionPages,
    BTreeMap<SectionPos, ReplicaSnapshot>,
) {
    let (world, actions) = base::world().unwrap();
    let mut snapshot = world
        .capture_snapshot(torse_sim::streaming::page_meta(&world))
        .unwrap();
    snapshot.bounds = WorldBounds::new(-64, 320).unwrap();
    let mut recovery = world.begin_restore();
    recovery.restore_snapshot(&snapshot).unwrap();
    let mut server = streaming::create_with_world(dir, (recovery.finish().unwrap(), actions));
    let pages = streaming::pages(dir);
    let mut expected = BTreeMap::new();
    for x in 2..26 {
        let pos = SectionPos::new(x, 0, 0);
        let world = server.authority().unwrap().world();
        let mut page = streaming::generated_with_entity(world, pos);
        let stone = world
            .registry()
            .default_state(&base::key("base:stone"))
            .unwrap();
        page.states = vec![StateEntry {
            id: stone,
            block: base::key("base:stone"),
            values: Vec::new(),
        }];
        let mut cells = vec![BlockStateId::EMPTY; 4096];
        for (i, cell) in cells.iter_mut().enumerate() {
            if (i + x as usize) % 7 < 3 {
                *cell = stone;
            }
        }
        page.sections[0].cells = SnapshotCells::Dense(cells);
        expected.insert(
            pos,
            ReplicaSnapshot {
                side: page.side,
                bounds: page.bounds,
                states: page.states.clone(),
                sections: page.sections.clone(),
                entities: vec![],
            },
        );
        server
            .install_section(&pages, page, &mut NoReplicas)
            .unwrap();
        server.unload_section(&pages, pos, &mut NoReplicas).unwrap();
    }
    (server, pages, expected)
}
pub fn mirror(server: &base::Files) -> TerrainMirror {
    let host = server.authority().unwrap();
    TerrainMirror::new(
        server.binding(),
        16,
        host.world().store().bounds(),
        TerrainLimits {
            max_sections: 8,
            max_bytes: 256 * 1024,
        },
        None,
    )
    .unwrap()
}
pub fn reopen(dir: &Directory, pages: SectionPages) -> base::Files {
    streaming::open(dir, pages).unwrap()
}
/// CPU mirror availability, not a GPU presentation timestamp.
pub fn fill(
    hub: &mut NetworkHub<MemoryTransport>,
    server: &mut base::Files,
    client: &mut MemoryTransport,
    mirror: &mut TerrainMirror,
) -> (Duration, Duration, usize, Duration) {
    let start = Instant::now();
    let mut first = None;
    let mut bytes = 0;
    let mut max_poll = Duration::ZERO;
    loop {
        let poll = Instant::now();
        hub.poll(server).unwrap();
        max_poll = max_poll.max(poll.elapsed());
        let (messages, n) = interest::drain(client);
        bytes += n;
        for msg in messages {
            if let ServerMessage::Terrain(update) = msg {
                let changes = mirror.apply(update).unwrap();
                if !changes.changed.is_empty() {
                    first.get_or_insert(start.elapsed());
                }
            }
        }
        if mirror.pages().count() == mirror.desired().len() && !mirror.desired().is_empty() {
            return (
                first.unwrap_or(start.elapsed()),
                start.elapsed(),
                bytes,
                max_poll,
            );
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "display terrain did not converge"
        );
        std::thread::yield_now();
    }
}
