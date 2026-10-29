//! Small isolated meshes shared by physical item rendering and UI previews.
use super::*;
impl Scene {
    pub fn block_model(&self, block: &ContentKey) -> Option<(Vec<Vertex>, Vec<u32>)> {
        let state = self.authority.registry().default_state(block)?;
        let shape = self.authority.registry().render_shape(state)?;
        let materials = self.state_materials.get(&state)?;
        let sample = RenderBlock {
            bounds: render_box(&shape),
            cross: matches!(shape, Shape::Cross),
            occlusion: 0,
            material: materials.base[0],
            face_materials: Some(materials.base),
            face_overlays: Some(materials.overlays),
            face_tints: None,
            face_overlay_tints: None,
            face_shades: None,
        };
        let mesh = torse_render::build_mesh(1, SectionPos::ZERO, |pos| {
            MeshSample::Ready(if pos == BlockPos::ZERO {
                sample
            } else {
                RenderBlock::EMPTY
            })
        });
        let vertices = mesh
            .positions
            .iter()
            .enumerate()
            .map(|(i, position)| Vertex {
                position: *position,
                normal: mesh.normals[i],
                uv: mesh.uvs[i],
                material: u32::from(mesh.materials[i / 4]),
                overlay_material: if mesh.overlays[i / 4] == u16::MAX {
                    u32::MAX
                } else {
                    u32::from(mesh.overlays[i / 4])
                },
                tint: mesh.tints[i],
                overlay_tint: mesh.overlay_tints[i],
                light: 1.0,
            })
            .collect();
        Some((vertices, mesh.indices))
    }

    pub(super) fn append_drop_model(
        &self,
        entity: &torse_world::Entity,
        vertices: &mut Vec<Vertex>,
        indices: &mut Vec<u32>,
    ) -> bool {
        let Some(fields) = entity.components.component(&torse_items::drops::schema()) else {
            return false;
        };
        let Some(ComponentValue::Content(item)) = fields.get(&key("items:drop/item")) else {
            return true;
        };
        let block = self
            .items
            .as_ref()
            .and_then(|_| self.item_block(item))
            .unwrap_or_else(|| item.clone());
        let Some((mut model, faces)) = self.block_model(&block) else {
            return true;
        };
        let base = vertices.len() as u32;
        for vertex in &mut model {
            vertex.position = [
                entity.position.x as f32 + (vertex.position[0] - 0.5) * 0.25,
                entity.position.y as f32 - 0.125 + vertex.position[1] * 0.25,
                entity.position.z as f32 + (vertex.position[2] - 0.5) * 0.25,
            ];
            vertex.light = -1.0;
        }
        vertices.extend(model);
        indices.extend(faces.into_iter().map(|i| i + base));
        true
    }
}
