//! Small retained UI rasterizer. Source sprites remain at their original size;
//! the GPU expands the resulting logical pixels by an integer scale.
use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::PathBuf;

use ab_glyph::{Font, FontVec, ScaleFont, point};
use torse_assets::{AssetCatalog, AssetGuid, AssetHandle};
use torse_types::{EvictionRank, MemoryBudget, MemoryClass, MemoryLease, MemoryPool};

pub struct Sprite {
    pub width: u32,
    pub height: u32,
    assets: AssetCatalog,
    handle: AssetHandle,
}
impl Sprite {
    pub fn attach_memory(&mut self, pool: MemoryPool) -> Result<(), String> {
        self.assets
            .attach_memory(pool)
            .map_err(|error| error.to_string())
    }
    pub fn decode(bytes: &[u8], expected: (u32, u32), guid: AssetGuid) -> Result<Self, String> {
        let mut decoder = png::Decoder::new(Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
        let dimensions = (reader.info().width, reader.info().height);
        if dimensions != expected
            || dimensions.0 == 0
            || dimensions.1 == 0
            || dimensions.0 > 1024
            || dimensions.1 > 1024
        {
            return Err("UI sprite dimensions do not match its bounded declaration".into());
        }
        let mut data = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut data).map_err(|e| e.to_string())?;
        if (info.width, info.height) != expected {
            return Err(format!(
                "UI sprite must be {}x{}, got {}x{}",
                expected.0, expected.1, info.width, info.height
            ));
        }
        let channels = info.color_type.samples();
        let mut pixels = Vec::with_capacity((info.width * info.height * 4) as usize);
        for p in data[..info.buffer_size()].chunks_exact(channels) {
            pixels.extend_from_slice(&match info.color_type {
                png::ColorType::Rgba => [p[0], p[1], p[2], p[3]],
                png::ColorType::Rgb => [p[0], p[1], p[2], 255],
                png::ColorType::Grayscale => [p[0], p[0], p[0], 255],
                png::ColorType::GrayscaleAlpha => [p[0], p[0], p[0], p[1]],
                png::ColorType::Indexed => return Err("unexpanded UI palette".into()),
            });
        }
        Self::from_pixels(info.width, info.height, pixels, guid)
    }

    fn from_pixels(
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        guid: AssetGuid,
    ) -> Result<Self, String> {
        let mut assets = AssetCatalog::new(MemoryBudget::new(8 * 1024 * 1024));
        let handle = assets
            .load(guid, &pixels, &[], &[])
            .map_err(|error| error.to_string())?;
        Ok(Self {
            width,
            height,
            assets,
            handle,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}
impl Rect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }
    pub fn contains(self, p: [i32; 2]) -> bool {
        p[0] >= self.x
            && p[1] >= self.y
            && i64::from(p[0]) < i64::from(self.x) + i64::from(self.w)
            && i64::from(p[1]) < i64::from(self.y) + i64::from(self.h)
    }
}
/// Retained UI raster over physical pixels.
///
/// Draw calls take logical rects; every logical pixel lands as a
/// `scale`x`scale` block so the nine-slice and fill math stay unchanged while
/// the texture itself matches the surface one to one. Glyphs rasterize at the
/// physical size and blend through [`Canvas::blend_px`], which keeps text
/// crisp instead of magnifying anti-aliased logical pixels.
pub struct Canvas {
    /// Logical width; the pixel buffer is `width * scale` wide.
    pub width: u32,
    /// Logical height.
    pub height: u32,
    /// Physical pixels per logical pixel.
    pub scale: u32,
    pixels: Vec<u8>,
    _memory: Option<MemoryLease>,
}
impl Canvas {
    pub fn new(width: u32, height: u32, scale: u32) -> Self {
        Self::try_new(width, height, scale, None).expect("bounded UI canvas")
    }
    /// Allocate at most 64 MiB, optionally charged to the host's shared pool.
    pub fn try_new(
        width: u32,
        height: u32,
        scale: u32,
        memory: Option<&MemoryPool>,
    ) -> Result<Self, String> {
        let scale = scale.max(1);
        let bytes = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|n| n.checked_mul(u64::from(scale).pow(2)))
            .and_then(|n| n.checked_mul(4))
            .ok_or("UI canvas size overflow")?;
        if width == 0 || height == 0 || scale > 8 || bytes > 64 * 1024 * 1024 {
            return Err("UI canvas dimensions exceed the raster budget".into());
        }
        let lease = memory
            .map(|pool| {
                pool.reserve(MemoryClass::Codec, bytes, EvictionRank::Pinned)
                    .map_err(|error| error.to_string())
            })
            .transpose()?;
        Ok(Self {
            width,
            height,
            scale,
            pixels: vec![0; bytes as usize],
            _memory: lease,
        })
    }
    /// Physical buffer width.
    pub fn pixel_width(&self) -> u32 {
        self.width * self.scale
    }
    /// Physical buffer height.
    pub fn pixel_height(&self) -> u32 {
        self.height * self.scale
    }
    /// Physical pixel bytes, for direct GPU upload.
    pub fn as_pixels(&self) -> &[u8] {
        &self.pixels
    }
    /// Clear retained pixels without allocating another frame buffer.
    pub fn clear(&mut self) {
        self.pixels.fill(0);
    }
    fn blend_px(&mut self, x: i32, y: i32, source: [u8; 4]) {
        if x < 0
            || y < 0
            || x >= self.pixel_width() as i32
            || y >= self.pixel_height() as i32
            || source[3] == 0
        {
            return;
        }
        let i = ((y as u32 * self.pixel_width() + x as u32) * 4) as usize;
        let dst = &mut self.pixels[i..i + 4];
        let sa = u32::from(source[3]);
        let da = u32::from(dst[3]);
        let a = sa * 255 + da * (255 - sa);
        for c in 0..3 {
            dst[c] =
                ((u32::from(source[c]) * sa * 255 + u32::from(dst[c]) * da * (255 - sa)) / a) as u8;
        }
        dst[3] = a.div_ceil(255) as u8;
    }
    fn blend(&mut self, x: i32, y: i32, source: [u8; 4]) {
        let s = self.scale as i32;
        for dy in 0..s {
            for dx in 0..s {
                self.blend_px(x * s + dx, y * s + dy, source);
            }
        }
    }
    pub fn fill(&mut self, rect: Rect, color: [u8; 4]) {
        let end_x = (i64::from(rect.x) + i64::from(rect.w)).clamp(0, i64::from(self.width)) as i32;
        let end_y = (i64::from(rect.y) + i64::from(rect.h)).clamp(0, i64::from(self.height)) as i32;
        for y in rect.y.max(0)..end_y {
            for x in rect.x.max(0)..end_x {
                self.blend(x, y, color);
            }
        }
    }
    pub fn sprite(&mut self, sprite: &Sprite, rect: Rect, border: u32) {
        self.sprite_region(
            sprite,
            Rect::new(0, 0, sprite.width, sprite.height),
            rect,
            border,
        );
    }

    /// Nine-slice one validated atlas region, clipping work to the canvas.
    pub fn sprite_region(&mut self, sprite: &Sprite, source: Rect, rect: Rect, border: u32) {
        if source.x < 0
            || source.y < 0
            || source.w == 0
            || source.h == 0
            || u64::from(source.x as u32) + u64::from(source.w) > u64::from(sprite.width)
            || u64::from(source.y as u32) + u64::from(source.h) > u64::from(sprite.height)
        {
            return;
        }
        let pixels = sprite
            .assets
            .get(sprite.handle)
            .expect("sprite owns its resident pixels");
        // Nine-slice mappings preserve corners and stretch only the middle.
        let map = |p: u32, dst: u32, src: u32| {
            let b = border.min(src / 2).min(dst / 2);
            if p < b {
                p
            } else if p >= dst - b {
                src - (dst - p)
            } else {
                b + (p - b) * (src - 2 * b) / (dst - 2 * b).max(1)
            }
        };
        let start_x = (-i64::from(rect.x)).max(0).min(i64::from(rect.w)) as u32;
        let start_y = (-i64::from(rect.y)).max(0).min(i64::from(rect.h)) as u32;
        let end_x = (i64::from(self.width) - i64::from(rect.x)).clamp(0, i64::from(rect.w)) as u32;
        let end_y = (i64::from(self.height) - i64::from(rect.y)).clamp(0, i64::from(rect.h)) as u32;
        for y in start_y..end_y {
            let sy = source.y as u32 + map(y, rect.h, source.h);
            for x in start_x..end_x {
                let sx = source.x as u32 + map(x, rect.w, source.w);
                let i = ((sy * sprite.width + sx) * 4) as usize;
                self.blend(
                    (i64::from(rect.x) + i64::from(x)) as i32,
                    (i64::from(rect.y) + i64::from(y)) as i32,
                    pixels[i..i + 4].try_into().unwrap(),
                );
            }
        }
    }
}

struct Raster {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    advance: i32,
    _memory: Option<MemoryLease>,
}
pub struct Text {
    font: FontVec,
    glyphs: BTreeMap<(char, u16), Raster>,
    pub chinese: bool,
    /// Physical pixels per logical point of requested text size.
    scale: u16,
    font_bytes: u64,
    pressure: bool,
    memory: Option<(MemoryPool, MemoryLease)>,
}
impl Text {
    /// Load a host-supplied font without depending on platform font discovery.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        if bytes.len() > 32 * 1024 * 1024 {
            return Err("UI font exceeds its source budget".into());
        }
        let font_bytes = bytes.len() as u64;
        let font = FontVec::try_from_vec(bytes).map_err(|error| error.to_string())?;
        Ok(Self {
            chinese: font.glyph_id('\u{52a0}').0 != 0,
            font,
            glyphs: BTreeMap::new(),
            scale: 1,
            font_bytes,
            pressure: false,
            memory: None,
        })
    }
    pub fn load() -> Result<Self, String> {
        let paths = if let Some(path) = std::env::var_os("TORSE_UI_FONT") {
            vec![PathBuf::from(path)]
        } else {
            let mut paths = Vec::new();
            if let Some(windir) = std::env::var_os("WINDIR") {
                paths.push(PathBuf::from(windir).join("Fonts/msyh.ttc"));
            }
            paths.extend(
                [
                    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                    "/System/Library/Fonts/PingFang.ttc",
                    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                ]
                .map(PathBuf::from),
            );
            paths
        };
        for path in paths {
            if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() <= 32 * 1024 * 1024)
                && let Ok(bytes) = std::fs::read(path)
            {
                let font_bytes = bytes.len() as u64;
                let Ok(font) = FontVec::try_from_vec(bytes) else {
                    continue;
                };
                return Ok(Self {
                    chinese: font.glyph_id('\u{52a0}').0 != 0,
                    font,
                    glyphs: BTreeMap::new(),
                    scale: 1,
                    font_bytes,
                    pressure: false,
                    memory: None,
                });
            }
        }
        Err("UI font unavailable; set TORSE_UI_FONT to a TTF, OTF or TTC file".into())
    }
    pub fn attach_memory(&mut self, pool: MemoryPool) -> Result<(), String> {
        if self.memory.is_some() || !self.glyphs.is_empty() {
            return Err("text memory requires a fresh cache".into());
        }
        let lease = pool
            .reserve(
                MemoryClass::Resource,
                self.font_bytes.saturating_add(1024 * 4096),
                EvictionRank::Pinned,
            )
            .map_err(|error| error.to_string())?;
        self.memory = Some((pool, lease));
        Ok(())
    }
    pub fn clear_cache(&mut self) {
        self.glyphs.clear();
    }
    /// Set the physical pixel multiplier for subsequent measure and draw.
    pub fn set_scale(&mut self, scale: u16) {
        if self.pressure {
            self.glyphs.clear();
            self.pressure = false;
        }
        self.scale = scale.max(1);
    }

    fn raster(&mut self, c: char, size: u16) -> &Raster {
        if self.glyphs.len() >= 1024 {
            self.glyphs.clear();
        }
        self.glyphs.entry((c, size)).or_insert_with(|| {
            let scaled = self.font.as_scaled(f32::from(size));
            let id = self.font.glyph_id(c);
            let advance = scaled.h_advance(id).ceil() as i32;
            let glyph =
                id.with_scale_and_position(f32::from(size), point(0.0, scaled.ascent().ceil()));
            if let Some(outline) = self.font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                let (width, height) = (bounds.width() as u32, bounds.height() as u32);
                let memory = self
                    .memory
                    .as_ref()
                    .map(|(pool, _)| {
                        pool.reserve(
                            MemoryClass::Mesh,
                            u64::from(width) * u64::from(height),
                            EvictionRank::UnsubscribedDerived,
                        )
                    })
                    .transpose();
                let Ok(memory) = memory else {
                    self.pressure = true;
                    return Raster {
                        pixels: Vec::new(),
                        width: 0,
                        height: 0,
                        x: 0,
                        y: 0,
                        advance,
                        _memory: None,
                    };
                };
                let mut pixels = vec![0; width as usize * height as usize];
                outline.draw(|x, y, coverage| {
                    // Steepen the coverage ramp: anti-aliased edges at
                    // small CJK sizes turn to mush once the logical canvas
                    // is magnified, so keep only firm pixels and one thin
                    // transition step.
                    let sharpened = ((coverage - 0.35) / 0.30).clamp(0.0, 1.0);
                    pixels[(y * width + x) as usize] = (sharpened * 255.0) as u8
                });
                Raster {
                    pixels,
                    width,
                    height,
                    x: bounds.min.x as i32,
                    y: bounds.min.y as i32,
                    advance,
                    _memory: memory,
                }
            } else {
                Raster {
                    pixels: Vec::new(),
                    width: 0,
                    height: 0,
                    x: 0,
                    y: 0,
                    advance,
                    _memory: None,
                }
            }
        })
    }
    pub fn measure(&mut self, value: &str, size: u16) -> u32 {
        let scaled = size.saturating_mul(self.scale);
        let total: u32 = value
            .chars()
            .take(160)
            .map(|c| self.raster(c, scaled).advance.max(0) as u32)
            .sum();
        total.div_ceil(u32::from(self.scale))
    }
    pub fn draw(
        &mut self,
        canvas: &mut Canvas,
        value: &str,
        rect: Rect,
        size: u16,
        color: [u8; 4],
    ) {
        let scale = u16::try_from(canvas.scale).unwrap_or(1).max(self.scale);
        let scaled = size.saturating_mul(scale);
        let s = i32::try_from(canvas.scale).unwrap_or(1);
        let prect = Rect {
            x: rect.x * s,
            y: rect.y * s,
            w: rect.w * canvas.scale,
            h: rect.h * canvas.scale,
        };
        let mut x = prect.x;
        for c in value.chars().take(160) {
            let glyph = self.raster(c, scaled);
            if x + glyph.advance > prect.x + prect.w as i32 {
                break;
            }
            for gy in 0..glyph.height {
                for gx in 0..glyph.width {
                    let y = prect.y + glyph.y + gy as i32;
                    if y < prect.y || y >= prect.y + prect.h as i32 {
                        continue;
                    }
                    let mut ink = color;
                    ink[3] = (u16::from(color[3])
                        * u16::from(glyph.pixels[(gy * glyph.width + gx) as usize])
                        / 255) as u8;
                    canvas.blend_px(x + glyph.x + gx as i32, y, ink);
                }
            }
            x += glyph.advance;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nine_slice_preserves_corners_and_transparency() {
        let sprite = Sprite::from_pixels(
            3,
            3,
            [
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [0, 0, 0, 0],
                [40, 40, 40, 255],
                [0, 0, 0, 0],
                [255, 255, 0, 255],
                [0, 255, 255, 255],
                [255, 0, 255, 255],
            ]
            .concat(),
            AssetGuid(1),
        )
        .unwrap();
        let mut canvas = Canvas::new(9, 9, 1);
        canvas.sprite(&sprite, Rect::new(0, 0, 9, 9), 1);
        assert_eq!(&canvas.pixels[..4], &[255, 0, 0, 255]);
        assert_eq!(&canvas.pixels[32..36], &[0, 0, 255, 255]);
        assert_eq!(&canvas.pixels[160..164], &[40, 40, 40, 255]);
        assert_eq!(&canvas.pixels[36..40], &[0, 0, 0, 0]);
    }
    #[test]
    fn scaled_canvas_maps_logical_pixels_to_blocks() {
        let mut canvas = Canvas::new(2, 1, 2);
        assert_eq!(canvas.pixel_width(), 4);
        assert_eq!(canvas.pixel_height(), 2);
        canvas.fill(Rect::new(0, 0, 1, 1), [255, 0, 0, 255]);
        // One logical pixel becomes a 2x2 block; the rest stays transparent.
        assert_eq!(
            canvas
                .pixels
                .chunks_exact(4)
                .filter(|p| *p == [255, 0, 0, 255])
                .count(),
            4
        );
        // Row 0: the red block covers columns 0 and 1, columns 2 and 3 stay
        // transparent; row 1 mirrors row 0.
        assert_eq!(&canvas.pixels[..4], &[255, 0, 0, 255]);
        assert_eq!(&canvas.pixels[4..8], &[255, 0, 0, 255]);
        assert_eq!(&canvas.pixels[8..12], &[0, 0, 0, 0]);
        assert_eq!(&canvas.pixels[12..16], &[0, 0, 0, 0]);
        assert_eq!(&canvas.pixels[16..20], &[255, 0, 0, 255]);
        assert_eq!(&canvas.pixels[20..24], &[255, 0, 0, 255]);
        assert_eq!(&canvas.pixels[24..28], &[0, 0, 0, 0]);
    }
}
