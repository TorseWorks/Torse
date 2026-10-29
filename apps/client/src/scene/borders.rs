//! A fixed neighborhood of section edges, independent of loaded terrain.
use super::*;

const EDGES: [(usize, usize); 12] = [
    (0, 1),
    (2, 3),
    (4, 5),
    (6, 7),
    (0, 2),
    (1, 3),
    (4, 6),
    (5, 7),
    (0, 4),
    (1, 5),
    (2, 6),
    (3, 7),
];

pub(super) fn location(eye: [f32; 3], side: u32) -> Option<(SectionPos, LocalPos)> {
    if eye.iter().any(|n| !n.is_finite()) {
        return None;
    }
    BlockPos::new(
        eye[0].floor() as i64,
        eye[1].floor() as i64,
        eye[2].floor() as i64,
    )
    .to_section(side)
    .ok()
}

pub(super) fn vertices(eye: [f32; 3], side: u32) -> Vec<Vertex> {
    let Some((center, _)) = location(eye, side) else {
        return Vec::new();
    };
    let mut edges = BTreeMap::new();
    for y in -1..=1 {
        for z in -1..=1 {
            for x in -1..=1 {
                let Some(pos) = center
                    .x
                    .checked_add(x)
                    .zip(center.y.checked_add(y))
                    .zip(center.z.checked_add(z))
                    .map(|((x, y), z)| SectionPos::new(x, y, z))
                else {
                    continue;
                };
                let Ok(min) = pos.to_block(LocalPos { x: 0, y: 0, z: 0 }, side) else {
                    continue;
                };
                let Ok(max) = min.checked_add(i64::from(side), i64::from(side), i64::from(side))
                else {
                    continue;
                };
                let corner = |bits| {
                    BlockPos::new(
                        if bits & 1 == 0 { min.x } else { max.x },
                        if bits & 2 == 0 { min.y } else { max.y },
                        if bits & 4 == 0 { min.z } else { max.z },
                    )
                };
                for (a, b) in EDGES {
                    let (a, b) = (corner(a), corner(b));
                    let edge = if a < b { (a, b) } else { (b, a) };
                    *edges.entry(edge).or_insert(false) |= pos == center;
                }
            }
        }
    }
    edges
        .into_iter()
        .flat_map(|((a, b), current)| {
            [a, b].map(|p| Vertex {
                position: [p.x as f32, p.y as f32, p.z as f32],
                tint: if current {
                    [0.95, 0.5, 0.08]
                } else {
                    [0.06, 0.55, 0.85]
                },
                ..Vertex::default()
            })
        })
        .collect()
}

impl Scene {
    pub fn section_location(&self, eye: [f32; 3]) -> Option<(SectionPos, LocalPos)> {
        location(eye, self.side)
    }
    pub fn section_border_bytes(&self, eye: [f32; 3]) -> Vec<u8> {
        Self::vertex_bytes(&vertices(eye, self.side))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negative_edges_use_floor_and_the_grid_is_bounded_and_shared() {
        assert_eq!(
            location([-0.1, 16.0, -16.1], 16),
            Some((SectionPos::new(-1, 1, -2), LocalPos { x: 15, y: 0, z: 15 }))
        );
        assert_eq!(location([f32::NAN, 0.0, 0.0], 16), None);
        let grid = vertices([-0.1, 16.0, -16.1], 16);
        assert_eq!(
            grid.iter().map(|v| v.position).collect::<Vec<_>>(),
            vertices([-0.9, 17.0, -17.0], 16)
                .iter()
                .map(|v| v.position)
                .collect::<Vec<_>>()
        );
        assert_eq!(grid.len(), 288);
        assert_eq!(
            grid.iter().filter(|v| v.tint == [0.95, 0.5, 0.08]).count(),
            24
        );
        assert!(
            grid.iter()
                .flat_map(|v| v.position)
                .all(|v| v % 16.0 == 0.0)
        );
        assert!(vertices([f32::INFINITY, 0.0, 0.0], 16).is_empty());
    }
}
