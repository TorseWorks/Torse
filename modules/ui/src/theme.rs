//! Validated PNG atlas regions for replaceable nine-slice widget skins.
use crate::{Canvas, Rect, Sprite};
use serde::Deserialize;
use std::collections::BTreeMap;
use torse_assets::AssetGuid;
use torse_types::MemoryPool;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum Part {
    Window,
    Group,
    Button,
    ButtonHover,
    ButtonPressed,
    Field,
    Slot,
    SlotSelected,
    Track,
    Thumb,
    ThumbHover,
    Chip,
    Progress,
    Check,
    CheckActive,
    Close,
    CloseHover,
    Crosshair,
    /// Optional title band; omitted by older skins.
    Title,
}
const PARTS: [Part; 18] = [
    Part::Window,
    Part::Group,
    Part::Button,
    Part::ButtonHover,
    Part::ButtonPressed,
    Part::Field,
    Part::Slot,
    Part::SlotSelected,
    Part::Track,
    Part::Thumb,
    Part::ThumbHover,
    Part::Chip,
    Part::Progress,
    Part::Check,
    Part::CheckActive,
    Part::Close,
    Part::CloseHover,
    Part::Crosshair,
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    width: u32,
    height: u32,
    regions: Vec<Region>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Region {
    part: Part,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    border: u32,
}

pub struct Theme {
    atlas: Sprite,
    regions: BTreeMap<Part, Region>,
}
impl Theme {
    pub fn load_dir(directory: &std::path::Path) -> Result<Self, String> {
        let read = |name: &str, limit: u64| -> Result<Vec<u8>, String> {
            use std::io::Read;
            let mut file =
                std::fs::File::open(directory.join(name)).map_err(|error| error.to_string())?;
            if file.metadata().map_err(|error| error.to_string())?.len() > limit {
                return Err("UI skin file exceeds its source budget".into());
            }
            let mut bytes = Vec::new();
            file.by_ref()
                .take(limit + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            if bytes.len() as u64 > limit {
                return Err("UI skin source grew beyond its budget".into());
            }
            Ok(bytes)
        };
        let png = read("default.png", 8 * 1024 * 1024)?;
        let text = read("default.toml", 32 * 1024)?;
        let manifest = std::str::from_utf8(&text).map_err(|error| error.to_string())?;
        Self::from_png(
            &png,
            manifest,
            AssetGuid(0x746f7273655f75695f736b696e000001),
        )
    }
    pub fn load_default() -> Result<Self, String> {
        Self::from_png(
            include_bytes!("../assets/default.png"),
            include_str!("../assets/default.toml"),
            AssetGuid(0x746f7273655f75695f736b696e000001),
        )
    }
    pub fn load_blue() -> Result<Self, String> {
        Self::from_png(
            include_bytes!("../assets/blue.png"),
            include_str!("../assets/blue.toml"),
            AssetGuid(0x746f7273655f75695f736b696e000002),
        )
    }
    /// Require all base roles once, with an optional title band.
    pub fn from_png(png: &[u8], manifest: &str, guid: AssetGuid) -> Result<Self, String> {
        if png.len() > 8 * 1024 * 1024 || manifest.len() > 32 * 1024 {
            return Err("UI skin source exceeds its budget".into());
        }
        let description: Manifest = toml::from_str(manifest).map_err(|error| error.to_string())?;
        if description.width == 0
            || description.height == 0
            || description.width > 1024
            || description.height > 1024
            || !(PARTS.len()..=PARTS.len() + 1).contains(&description.regions.len())
        {
            return Err("invalid UI skin dimensions or roles".into());
        }
        let mut regions = BTreeMap::new();
        for region in description.regions {
            if region.width == 0
                || region.height == 0
                || u64::from(region.x) + u64::from(region.width) > u64::from(description.width)
                || u64::from(region.y) + u64::from(region.height) > u64::from(description.height)
                || region.border > region.width.min(region.height) / 2
            {
                return Err("invalid UI skin region".into());
            }
            if regions.insert(region.part, region).is_some() {
                return Err("duplicate UI skin region".into());
            }
        }
        if PARTS.iter().any(|part| !regions.contains_key(part)) {
            return Err("missing UI skin role".into());
        }
        let atlas = Sprite::decode(png, (description.width, description.height), guid)?;
        Ok(Self { atlas, regions })
    }
    pub fn attach_memory(&mut self, memory: MemoryPool) -> Result<(), String> {
        self.atlas.attach_memory(memory)
    }
    pub fn paint(&self, canvas: &mut Canvas, part: Part, rect: Rect) {
        let Some(region) = self.regions.get(&part) else {
            return;
        };
        canvas.sprite_region(
            &self.atlas,
            Rect::new(
                region.x as i32,
                region.y as i32,
                region.width,
                region.height,
            ),
            rect,
            region.border,
        );
    }
    pub fn button(&self, canvas: &mut Canvas, rect: Rect, hot: bool, pressed: bool) {
        self.paint(
            canvas,
            if pressed {
                Part::ButtonPressed
            } else if hot {
                Part::ButtonHover
            } else {
                Part::Button
            },
            rect,
        );
    }
    pub fn slot(&self, canvas: &mut Canvas, rect: Rect, selected: bool) {
        self.paint(
            canvas,
            if selected {
                Part::SlotSelected
            } else {
                Part::Slot
            },
            rect,
        );
    }
    pub fn checkbox(&self, canvas: &mut Canvas, rect: Rect, checked: bool) {
        self.paint(
            canvas,
            if checked {
                Part::CheckActive
            } else {
                Part::Check
            },
            rect,
        );
    }
    pub fn progress(&self, canvas: &mut Canvas, rect: Rect, fraction: f32) {
        self.paint(canvas, Part::Track, rect);
        self.paint(
            canvas,
            Part::Progress,
            Rect::new(
                rect.x + 1,
                rect.y + 1,
                (rect.w.saturating_sub(2) as f32 * fraction.clamp(0.0, 1.0)) as u32,
                rect.h.saturating_sub(2),
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torse_types::{MemoryBudget, MemoryPool};
    #[test]
    fn blue_skin_changes_controls_and_optional_title_preserves_legacy_skins() {
        let dark = Theme::load_default().unwrap();
        let blue = Theme::load_blue().unwrap();
        let mut a = Canvas::new(40, 40, 1);
        let mut b = Canvas::new(40, 40, 1);
        let rect = Rect::new(0, 0, 40, 40);
        dark.paint(&mut a, Part::Title, rect);
        assert!(a.as_pixels().iter().all(|v| *v == 0));
        blue.paint(&mut b, Part::Title, rect);
        assert!(b.as_pixels().chunks_exact(4).all(|p| p[2] > p[0]));
        dark.button(&mut a, rect, false, false);
        blue.button(&mut b, rect, false, false);
        assert_ne!(a.as_pixels(), b.as_pixels());
        let invalid =
            include_str!("../assets/blue.toml").replace("part = \"title\"", "part = \"window\"");
        assert!(
            Theme::from_png(include_bytes!("../assets/blue.png"), &invalid, AssetGuid(1)).is_err()
        );
    }
    #[test]
    fn custom_skin_changes_rendering_and_releases_its_shared_reservation() {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 160, 32);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_image_data(&[17, 201, 33, 255].repeat(160 * 32))
                .unwrap();
        }
        let pool = MemoryPool::new(MemoryBudget::new(8 * 1024 * 1024));
        {
            let mut theme =
                Theme::from_png(&bytes, include_str!("../assets/default.toml"), AssetGuid(1))
                    .unwrap();
            theme.attach_memory(pool.clone()).unwrap();
            assert!(pool.usage().total > 0);
            let mut canvas = Canvas::try_new(20, 10, 1, Some(&pool)).unwrap();
            theme.paint(&mut canvas, Part::Window, Rect::new(0, 0, 20, 10));
            assert!(
                canvas
                    .as_pixels()
                    .chunks_exact(4)
                    .all(|p| p == [17, 201, 33, 255])
            );
        }
        assert_eq!(pool.usage().total, 0);
    }
    #[test]
    fn malformed_regions_and_budget_rejection_do_not_replace_a_valid_skin() {
        let png = include_bytes!("../assets/default.png");
        let text = include_str!("../assets/default.toml");
        for manifest in [
            text.replace("width = 160", "width = 2000"),
            text.replace("part = \"window\"", "part = \"button\""),
            text.replacen("x = 0", "x = 4294967295", 1),
        ] {
            assert!(Theme::from_png(png, &manifest, AssetGuid(1)).is_err());
        }
        let pool = MemoryPool::new(MemoryBudget::new(1));
        let mut theme = Theme::load_default().unwrap();
        assert!(theme.attach_memory(pool.clone()).is_err());
        assert_eq!(pool.usage().total, 0);
        let mut canvas = Canvas::new(8, 8, 1);
        theme.paint(&mut canvas, Part::Button, Rect::new(0, 0, 8, 8));
        assert!(canvas.as_pixels().iter().any(|p| *p != 0));
        assert!(Canvas::try_new(u32::MAX, u32::MAX, 8, None).is_err());
    }
}
