//! Bounded, cached isometric rasterization of the scene's actual block geometry.
use super::*;
use crate::scene::{Scene, Vertex};
use crate::texture::{MipView, Palette};
use torse_assets::AssetGuid;
use torse_types::{Compatibility, ContentKey};

#[derive(Default)]
pub(super) struct Models {
    identity: Option<(Compatibility, u64)>,
    sprites: BTreeMap<ContentKey, Sprite>,
}
impl Models {
    pub(super) fn get(&self, item: &ContentKey) -> Option<&Sprite> {
        self.sprites.get(item)
    }
    pub(super) fn attach_memory(&mut self, pool: torse_types::MemoryPool) -> Result<(), String> {
        for sprite in self.sprites.values_mut() {
            sprite.attach_memory(pool.clone())?;
        }
        Ok(())
    }
    pub(super) fn prepare(
        &mut self,
        scene: &Scene,
        items: impl Iterator<Item = ContentKey>,
        pool: Option<&torse_types::MemoryPool>,
    ) -> bool {
        let identity = (
            scene.authority().coordinator().compatibility(),
            scene.palette().asset_generation(),
        );
        let mut changed = self.identity != Some(identity);
        if changed {
            self.sprites.clear();
            self.identity = Some(identity);
        }
        for item in items.take(80) {
            if self.sprites.contains_key(&item) {
                continue;
            }
            if self.sprites.len() >= 128 {
                self.sprites.clear();
                changed = true;
            }
            let block = scene.item_block(&item).unwrap_or_else(|| item.clone());
            if let Some((vertices, indices)) = scene.block_model(&block)
                && let Ok(mut sprite) = raster(&vertices, &indices, scene.palette())
            {
                if let Some(pool) = pool
                    && sprite.attach_memory(pool.clone()).is_err()
                {
                    continue;
                }
                self.sprites.insert(item, sprite);
                changed = true;
            }
        }
        changed
    }
}
fn project(p: [f32; 3]) -> [f32; 3] {
    let [x, y, z] = p.map(|n| n - 0.5);
    [
        16.0 + (0.832 * x - 0.555 * z) * 18.0,
        16.0 + (0.225 * x - 0.914 * y + 0.338 * z) * 18.0,
        0.507 * x + 0.406 * y + 0.761 * z,
    ]
}
fn sample(
    palette: &Palette,
    mip: &MipView<'_>,
    material: u32,
    uv: [f32; 2],
    tint: [f32; 3],
) -> [f32; 4] {
    let Some(surface) = palette.surfaces().get(material as usize) else {
        return [0.0; 4];
    };
    let [u, v, w, h] = surface.rect;
    let x = ((u + uv[0].clamp(0.0, 0.9999) * w) * mip.width as f32) as u32;
    let y = ((v + uv[1].clamp(0.0, 0.9999) * h) * mip.height as f32) as u32;
    let index = ((y.min(mip.height - 1) * mip.width + x.min(mip.width - 1)) * 4) as usize;
    let rgba = &mip.pixels[index..index + 4];
    let alpha = match surface.alpha {
        torse_content::AlphaMode::Opaque => 1.0,
        torse_content::AlphaMode::Mask => {
            if rgba[3] < 128 {
                0.0
            } else {
                1.0
            }
        }
        torse_content::AlphaMode::Blend => f32::from(rgba[3]) / 255.0,
    };
    [
        f32::from(rgba[0]) * surface.tint[0] * tint[0],
        f32::from(rgba[1]) * surface.tint[1] * tint[1],
        f32::from(rgba[2]) * surface.tint[2] * tint[2],
        alpha,
    ]
}
fn edge(a: [f32; 3], b: [f32; 3], p: [f32; 3]) -> f32 {
    (p[0] - a[0]) * (b[1] - a[1]) - (p[1] - a[1]) * (b[0] - a[0])
}
fn raster(vertices: &[Vertex], indices: &[u32], palette: &Palette) -> Result<Sprite, String> {
    if vertices.len() > 4096 || indices.len() > 8192 {
        return Err("UI block model exceeds the preview budget".into());
    }
    let mips = palette.mips();
    let mip = mips.first().ok_or("UI model atlas is empty")?;
    let mut pixels = vec![0; 32 * 32 * 4];
    let mut depth = [f32::NEG_INFINITY; 32 * 32];
    for face in indices.chunks_exact(3) {
        let Some(a) = vertices.get(face[0] as usize) else {
            continue;
        };
        let Some(b) = vertices.get(face[1] as usize) else {
            continue;
        };
        let Some(c) = vertices.get(face[2] as usize) else {
            continue;
        };
        let crossed = a.normal.iter().filter(|n| n.abs() > 0.01).count() > 1;
        if !crossed && a.normal[0] * 0.507 + a.normal[1] * 0.406 + a.normal[2] * 0.761 <= 0.0 {
            continue;
        }
        let [pa, pb, pc] = [a, b, c].map(|v| project(v.position));
        let area = edge(pa, pb, pc);
        if area.abs() < 1e-6 {
            continue;
        }
        let shade = if crossed {
            0.9
        } else {
            0.65 + 0.30 * a.normal[1].max(0.0) + 0.15 * a.normal[0].abs()
        };
        for y in 0..32 {
            for x in 0..32 {
                let p = [x as f32 + 0.5, y as f32 + 0.5, 0.0];
                let wa = edge(pb, pc, p) / area;
                let wb = edge(pc, pa, p) / area;
                let wc = 1.0 - wa - wb;
                if wa < -1e-5 || wb < -1e-5 || wc < -1e-5 {
                    continue;
                }
                let z = wa * pa[2] + wb * pb[2] + wc * pc[2];
                let index = y * 32 + x;
                if z <= depth[index] {
                    continue;
                }
                let uv = [0, 1].map(|axis| wa * a.uv[axis] + wb * b.uv[axis] + wc * c.uv[axis]);
                let mut color = sample(palette, mip, a.material, uv, a.tint);
                if a.overlay_material != u32::MAX {
                    let overlay = sample(palette, mip, a.overlay_material, uv, a.overlay_tint);
                    for channel in 0..3 {
                        color[channel] =
                            color[channel] * (1.0 - overlay[3]) + overlay[channel] * overlay[3];
                    }
                    color[3] = color[3] + overlay[3] * (1.0 - color[3]);
                }
                if color[3] <= 0.01 {
                    continue;
                }
                for channel in 0..3 {
                    pixels[index * 4 + channel] = (color[channel] * shade).clamp(0.0, 255.0) as u8;
                }
                pixels[index * 4 + 3] = (color[3] * 255.0) as u8;
                depth[index] = z;
            }
        }
    }
    Sprite::from_pixels(32, 32, pixels, AssetGuid(0x0005_0000_0000_0001))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixture_shapes_and_custom_content_use_geometry_and_release_memory() {
        let mut scene = Scene::new(0).unwrap();
        scene.enable_items(None).unwrap();
        let pool = torse_types::MemoryPool::new(torse_types::MemoryBudget::new(1024 * 1024));
        let mut models = Models::default();
        models.prepare(
            &scene,
            scene.item_catalog().into_iter().map(|s| s.item),
            Some(&pool),
        );
        assert_eq!(models.sprites.len(), 4);
        let mut silhouettes = Vec::new();
        for sprite in models.sprites.values() {
            let mut canvas = Canvas::new(32, 32, 1);
            canvas.sprite(sprite, Rect::new(0, 0, 32, 32), 0);
            let mask: Vec<_> = canvas
                .as_pixels()
                .chunks_exact(4)
                .map(|p| p[3] > 0)
                .collect();
            assert!(mask.iter().any(|p| *p));
            assert!(mask.iter().any(|p| !*p));
            silhouettes.push(mask);
        }
        assert!(silhouettes.windows(2).all(|pair| pair[0] != pair[1]));
        assert!(pool.usage().total > 0);
        drop(models);
        assert_eq!(pool.usage().total, 0);
    }
}
