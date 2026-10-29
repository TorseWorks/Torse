//! Bounded newly exposed faces while independent section meshes catch up.
use super::*;
use std::collections::BTreeSet;

const MAX_CELLS: usize = 64;
pub(super) const CONTROL_BYTES: u64 = 256 * 1024;

#[derive(Default)]
pub(super) struct Previews {
    before: BTreeMap<BlockPos, MeshData>,
    dirty: bool,
}
impl Previews {
    pub fn record(&mut self, cells: Vec<(BlockPos, MeshData)>) {
        self.before.extend(cells);
        self.dirty = true;
    }
}

impl Scene {
    pub(super) fn prepare_surfaces(
        &mut self,
        target: BlockPos,
    ) -> Result<Vec<(BlockPos, MeshData)>, String> {
        self.prune_surfaces();
        let cells: BTreeSet<_> = std::iter::once(Some(target))
            .chain(torse_render::Face::ALL.map(|face| {
                let (x, y, z) = face.offset();
                target.checked_add(x, y, z).ok()
            }))
            .flatten()
            .filter(|pos| matches!(self.authority.store().query(*pos), TerrainQuery::Ready(_)))
            .filter(|pos| !self.surfaces.before.contains_key(pos))
            .collect();
        if self.surfaces.before.len() + cells.len() > MAX_CELLS {
            return Err("surface presentation is still catching up; retry the edit".into());
        }
        Ok(cells
            .into_iter()
            .map(|pos| {
                let mesh = torse_render::build_mesh(1, SectionPos::new(pos.x, pos.y, pos.z), |p| {
                    sample_visual(self, None, p)
                });
                (pos, mesh)
            })
            .collect())
    }

    pub fn surfaces_dirty(&self) -> bool {
        self.surfaces.dirty
    }
    pub fn prune_surfaces(&mut self) {
        let count = self.surfaces.before.len();
        self.surfaces
            .before
            .retain(|pos, _| matches!(self.authority.store().query(*pos), TerrainQuery::Ready(_)));
        self.surfaces.dirty |= count != self.surfaces.before.len();
    }
    pub fn surfaces_uploaded(&mut self) {
        self.surfaces.dirty = false;
    }
    pub fn surface_geometry(&self) -> (Vec<Vertex>, Vec<u32>) {
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        let view = self.lighting.as_ref().map(|lighting| lighting.view(self));
        for (pos, before) in &self.surfaces.before {
            let after = torse_render::build_mesh(1, SectionPos::new(pos.x, pos.y, pos.z), |p| {
                sample_visual(self, view.as_ref(), p)
            });
            for (face, quad) in after.positions.chunks_exact(4).enumerate() {
                if before
                    .positions
                    .chunks_exact(4)
                    .enumerate()
                    .any(|(old, old_quad)| {
                        old_quad == quad && before.normals[old * 4] == after.normals[face * 4]
                    })
                {
                    continue;
                }
                let base = vertices.len() as u32;
                for (corner, p) in quad.iter().enumerate() {
                    let index = face * 4 + corner;
                    vertices.push(Vertex {
                        position: [
                            p[0] + pos.x as f32,
                            p[1] + pos.y as f32,
                            p[2] + pos.z as f32,
                        ],
                        normal: after.normals[index],
                        uv: after.uvs[index],
                        material: u32::from(after.materials[face]),
                        overlay_material: if after.overlays[face] == u16::MAX {
                            u32::MAX
                        } else {
                            u32::from(after.overlays[face])
                        },
                        tint: after.tints[index],
                        overlay_tint: after.overlay_tints[index],
                        light: after.shades[index],
                    });
                }
                indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
            }
        }
        (vertices, indices)
    }

    pub fn surface_section_uploaded(&mut self, pos: SectionPos) {
        let Some((stamp, _)) = self.meshes.get(&pos) else {
            return;
        };
        let current = MeshStamp::capture(self, pos);
        if !stamp.same_surface_input(&current) {
            return;
        }
        let before = self.surfaces.before.len();
        self.surfaces.before.retain(|p, _| {
            p.to_section(self.side)
                .is_ok_and(|(section, _)| section != pos)
        });
        self.surfaces.dirty |= before != self.surfaces.before.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wall(vertices: &[Vertex]) -> bool {
        vertices.chunks_exact(4).any(|quad| {
            quad.iter()
                .all(|v| v.position[0] == 16.0 && v.normal == [-1.0, 0.0, 0.0])
                && quad.iter().all(|v| {
                    (1.0..=2.0).contains(&v.position[1]) && (8.0..=9.0).contains(&v.position[2])
                })
        })
    }

    fn dark_scene() -> Scene {
        let mut scene = Scene::new(1).unwrap();
        scene.generator = None;
        let a = SectionPos::ZERO;
        let b = SectionPos::new(1, 0, 0);
        scene.sections = vec![a, b];
        let stone = scene
            .authority
            .registry()
            .default_state(&key("base:stone"))
            .unwrap();
        for x in 8..=18 {
            for y in 0..=4 {
                for z in 6..=10 {
                    scene
                        .authority
                        .store_mut()
                        .edit(BlockPos::new(x, y, z), stone)
                        .unwrap();
                }
            }
        }
        for x in 10..15 {
            scene
                .authority
                .store_mut()
                .edit(BlockPos::new(x, 1, 8), BlockStateId::EMPTY)
                .unwrap();
        }
        scene.enable_lighting().unwrap();
        scene.refresh_meshes();
        scene
    }

    #[test]
    fn dark_border_wall_is_covered_until_its_own_current_mesh_is_uploaded() {
        let mut scene = dark_scene();
        let a = SectionPos::ZERO;
        let b = SectionPos::new(1, 0, 0);
        assert!(!wall(&scene.section_geometry(b).unwrap().0));
        scene
            .apply_edit(false, &key("base:stone"), BlockPos::new(15, 1, 8), 0)
            .unwrap();
        let (patch, _) = scene.surface_geometry();
        assert!(wall(&patch));
        assert!(patch.iter().all(|v| v.light < 0.2));
        scene.surfaces_uploaded();
        assert_eq!(scene.refresh_meshes_bounded(1), [a]);
        scene.surface_section_uploaded(a);
        assert!(!wall(&scene.section_geometry(b).unwrap().0));
        assert!(wall(&scene.surface_geometry().0));
        // An acknowledgement of the still-old neighbor cannot retire the patch.
        scene.surface_section_uploaded(b);
        assert!(wall(&scene.surface_geometry().0));
        assert_eq!(scene.refresh_meshes_bounded(1), [b]);
        scene.surface_section_uploaded(b);
        assert!(wall(&scene.section_geometry(b).unwrap().0));
        assert!(!wall(&scene.surface_geometry().0));
        assert!(scene.surfaces_dirty());
    }

    #[test]
    fn restoring_an_edited_cell_removes_the_preview_without_transparent_duplicates() {
        let mut scene = dark_scene();
        let target = BlockPos::new(15, 1, 8);
        scene
            .apply_edit(false, &key("base:stone"), target, 0)
            .unwrap();
        assert!(wall(&scene.surface_geometry().0));
        scene.surfaces_uploaded();
        scene
            .apply_edit(true, &key("base:stone"), target, 0)
            .unwrap();
        assert!(scene.surfaces_dirty());
        assert!(scene.surface_geometry().0.is_empty());
        let charged: usize = scene
            .surfaces
            .before
            .values()
            .map(|mesh| mesh.allocated_bytes() + size_of::<MeshData>() + 128)
            .sum();
        assert!(charged < CONTROL_BYTES as usize);
    }

    #[cfg(feature = "gpu-tests")]
    #[test]
    fn gpu_preview_occludes_the_gap_before_the_adjacent_mesh_arrives() {
        let mut scene = dark_scene();
        scene
            .apply_edit(false, &key("base:stone"), BlockPos::new(15, 1, 8), 0)
            .unwrap();
        scene.refresh_meshes_bounded(1);
        let mut renderer = crate::renderer::Renderer::new(128, 128).unwrap();
        renderer.set_palette(scene.palette());
        let (vertices, indices) = scene.section_geometry(SectionPos::ZERO).unwrap();
        assert!(renderer.upload_section(
            SectionPos::ZERO,
            &Scene::vertex_bytes(&vertices),
            &indices
        ));
        let camera = crate::camera::Camera::new([13.5, 1.5, 8.5], [20.0, 1.5, 8.5]);
        let directory =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.work/cln011");
        std::fs::create_dir_all(&directory).unwrap();
        let before = directory.join("before.png");
        assert!(renderer.render(camera.view_projection(1.0), camera.eye));
        renderer.save_png(before.to_str().unwrap()).unwrap();
        let (vertices, indices) = scene.surface_geometry();
        assert!(renderer.upload_section(
            crate::renderer::SURFACE_SLOT,
            &Scene::vertex_bytes(&vertices),
            &indices
        ));
        let after = directory.join("after.png");
        assert!(renderer.render(camera.view_projection(1.0), camera.eye));
        renderer.save_png(after.to_str().unwrap()).unwrap();
        let center = |path| {
            let atlas =
                torse_assets::texture::Atlas::decode(&std::fs::read(path).unwrap()).unwrap();
            atlas.pixels()[(64 * 128 + 64) * 4]
        };
        assert!(center(before) > 100);
        assert!(center(after) < 60);
    }

    #[test]
    fn queued_cell_limit_rejects_before_mutation_and_unload_releases_previews() {
        let mut scene = Scene::new(1).unwrap();
        for x in 0..MAX_CELLS as i64 {
            scene
                .surfaces
                .before
                .insert(BlockPos::new(x % 16, 1, x / 16), MeshData::default());
        }
        let before = scene.authority.seq();
        assert!(
            scene
                .apply_edit(false, &key("base:stone"), BlockPos::new(15, 1, 8), 0)
                .is_err()
        );
        assert_eq!(scene.authority.seq(), before);
        scene
            .authority
            .store_mut()
            .unload(SectionPos::ZERO)
            .unwrap();
        scene.prune_surfaces();
        assert!(scene.surfaces.before.is_empty());
        assert!(scene.surfaces_dirty());
    }
}
