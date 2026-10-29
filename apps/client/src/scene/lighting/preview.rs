//! Bounded presentation-only propagation into newly opened terrain cells.
use super::*;
use std::collections::{BTreeSet, VecDeque};

const WORK: usize = 4096;

impl Lighting {
    fn emission(scene: &Scene, pos: BlockPos) -> u8 {
        match scene.authority.store().query(pos) {
            TerrainQuery::Ready(state) => {
                scene.authority.registry().definition(state).map_or(0, |d| {
                    (d.light_transport.emission * 15.0).round().clamp(0.0, 15.0) as u8
                })
            }
            _ => 0,
        }
    }

    fn preview_removed(
        &mut self,
        scene: &Scene,
        kind: usize,
        positions: &[BlockPos],
        changed: &mut BTreeSet<(usize, SectionPos)>,
    ) -> Vec<BlockPos> {
        // Stage the entire removal before publishing. A room larger than the
        // fixed allowance keeps its previous image until exact jobs finish.
        let mut removed = BTreeSet::new();
        let mut queue = VecDeque::new();
        for &pos in positions.iter().take(WORK) {
            if let Some(level) = self.level(scene, kind, pos)
                && level > 0
                && removed.insert(pos)
            {
                queue.push_back((pos, level));
            }
        }
        while let Some((pos, source)) = queue.pop_front() {
            for face in torse_render::Face::ALL {
                let (dx, dy, dz) = face.offset();
                let Ok(neighbor) = pos.checked_add(dx, dy, dz) else {
                    continue;
                };
                let Some(level) = self.level(scene, kind, neighbor) else {
                    continue;
                };
                if level > 0
                    && (level < source || (kind == 1 && source == 15 && level == 15 && dy == -1))
                    && !removed.contains(&neighbor)
                {
                    if removed.len() == WORK {
                        return Vec::new();
                    }
                    removed.insert(neighbor);
                    queue.push_back((neighbor, level));
                }
            }
        }
        for &pos in &removed {
            let (section, local) = pos.to_section(scene.side).expect("cached light position");
            let entry = self.entries[kind]
                .get_mut(&section)
                .expect("cached light section");
            entry.levels[torse_world::linear_index(scene.side, local)] = 0;
            changed.insert((kind, section));
        }
        removed.into_iter().collect()
    }

    fn level(&self, scene: &Scene, kind: usize, pos: BlockPos) -> Option<u8> {
        let bounds = scene.authority.store().bounds();
        if !bounds.contains_y(pos.y) {
            return Some(if kind == 1 && pos.y >= bounds.max_y {
                15
            } else {
                0
            });
        }
        let (section, local) = pos.to_section(scene.side).ok()?;
        let entry = self.entries[kind].get(&section)?;
        // A retained snapshot belongs to the same resident target generation.
        (entry.load == scene.load_generations.get(&section).copied())
            .then(|| entry.levels[torse_world::linear_index(scene.side, local)])
    }

    fn absorption(scene: &Scene, pos: BlockPos) -> Option<u8> {
        match scene.authority.store().query(pos) {
            TerrainQuery::Ready(state) if state.is_empty() => Some(0),
            TerrainQuery::Ready(state) => scene.authority.registry().definition(state).map(|d| {
                (d.light_transport.absorption * 15.0)
                    .ceil()
                    .clamp(0.0, 15.0) as u8
            }),
            _ => None,
        }
    }

    fn raise(
        &mut self,
        scene: &Scene,
        kind: usize,
        pos: BlockPos,
        level: u8,
        changed: &mut BTreeSet<(usize, SectionPos)>,
        remaining: &mut usize,
    ) -> bool {
        if *remaining == 0 {
            return false;
        }
        let Ok((section, local)) = pos.to_section(scene.side) else {
            return false;
        };
        let Some(entry) = self.entries[kind].get_mut(&section) else {
            return false;
        };
        if entry.load != scene.load_generations.get(&section).copied() {
            return false;
        }
        let previous = &mut entry.levels[torse_world::linear_index(scene.side, local)];
        if level <= *previous {
            return false;
        }
        *previous = level;
        *remaining -= 1;
        changed.insert((kind, section));
        true
    }

    pub(super) fn preview_opened(
        &mut self,
        scene: &Scene,
        positions: &[BlockPos],
    ) -> Result<(), String> {
        // The fixed queue and at most WORK changed sections per channel
        // fit PREVIEW_BYTES, including conservative tree-node overhead.
        let mut queue = VecDeque::with_capacity(WORK);
        let mut changed = BTreeSet::new();
        for kind in 0..2 {
            queue.clear();
            let mut remaining = WORK;
            let removed = self.preview_removed(scene, kind, positions, &mut changed);
            for &pos in removed.iter().chain(positions).take(WORK) {
                let Some(absorption) = Self::absorption(scene, pos) else {
                    continue;
                };
                let mut level = if kind == 0 {
                    Self::emission(scene, pos)
                } else {
                    0
                };
                for face in torse_render::Face::ALL {
                    let (dx, dy, dz) = face.offset();
                    let Ok(neighbor) = pos.checked_add(dx, dy, dz) else {
                        continue;
                    };
                    let Some(source) = self.level(scene, kind, neighbor) else {
                        continue;
                    };
                    let incoming = if kind == 1 && dy == 1 && source == 15 && absorption == 0 {
                        15
                    } else {
                        source.saturating_sub(absorption.max(1))
                    };
                    level = level.max(incoming);
                }
                if self.raise(scene, kind, pos, level, &mut changed, &mut remaining)
                    && queue.len() < WORK
                {
                    queue.push_back((pos, level));
                }
            }
            for _ in 0..WORK {
                let Some((pos, source)) = queue.pop_front() else {
                    break;
                };
                for face in torse_render::Face::ALL {
                    let (dx, dy, dz) = face.offset();
                    let Ok(neighbor) = pos.checked_add(dx, dy, dz) else {
                        continue;
                    };
                    let Some(absorption) = Self::absorption(scene, neighbor) else {
                        continue;
                    };
                    let level = if kind == 1 && dy == -1 && source == 15 && absorption == 0 {
                        15
                    } else {
                        source.saturating_sub(absorption.max(1))
                    };
                    if self.raise(scene, kind, neighbor, level, &mut changed, &mut remaining)
                        && queue.len() < WORK
                    {
                        queue.push_back((neighbor, level));
                    }
                }
            }
        }
        if !changed.is_empty() {
            self.advance_generation()?;
            for (kind, section) in changed {
                self.entries[kind]
                    .get_mut(&section)
                    .expect("changed light section")
                    .generation = self.generation;
            }
        }
        Ok(())
    }
}
