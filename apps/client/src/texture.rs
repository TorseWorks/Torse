//! Material palette for the terrain pass.
//!
//! Content selects one material per block face. This module resolves those keys
//! through the base game's texture pack and lays the result out as a fixed-size
//! material table for the shader. The table is a hard budget: a session with
//! more distinct materials fails at startup instead of silently drawing the
//! wrong surface.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub use torse_assets::texture::TexturePack;
use torse_assets::texture::{Atlas, MaterialAnimation, MaterialEntry, MipLevel, TintSource};
use torse_assets::{AssetAlias, AssetCatalog, AssetGuid, AssetHandle};
use torse_content::{AlphaMode, Face};
use torse_types::{ContentKey, MemoryBudget};
use torse_world::{BlockStateId, BlockStateRegistry};

/// Maximum materials addressable by one terrain vertex.
pub const MAX_MATERIALS: usize = 512;

/// Mip levels uploaded for the terrain atlas, including the base level.
pub const MIP_LEVELS: u32 = 3;

/// One material as consumed by the terrain shader.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Surface {
    /// Atlas rectangle as origin and size in normalised coordinates.
    pub rect: [f32; 4],
    /// Linear RGB tint multiplied with the sampled texture.
    pub tint: [f32; 3],
    /// Optional position-dependent tint source.
    pub tint_source: Option<TintSource>,
    /// Optional vertical-strip animation.
    pub animation: Option<MaterialAnimation>,
    /// Blending mode used by the fragment shader.
    pub alpha: AlphaMode,
}

/// Materials used by one session, addressed by a small integer.
#[derive(Clone, Debug)]
pub struct Palette {
    slots: Arc<BTreeMap<(ContentKey, AlphaMode), u16>>,
    surfaces: Arc<Vec<Surface>>,
    assets: Arc<AssetCatalog>,
    mips: Vec<ResidentMip>,
    memory: Option<Arc<torse_types::MemoryLease>>,
}

#[derive(Clone, Copy, Debug)]
struct ResidentMip {
    width: u32,
    height: u32,
    handle: AssetHandle,
}

pub struct MipView<'a> {
    pub width: u32,
    pub height: u32,
    pub pixels: &'a [u8],
}

fn resident_mips(mips: Vec<MipLevel>) -> Result<(Arc<AssetCatalog>, Vec<ResidentMip>), String> {
    let mut assets = AssetCatalog::new(MemoryBudget::new(8 * 1024 * 1024));
    let mut resident = Vec::new();
    for (index, mip) in mips.into_iter().enumerate() {
        let guid = AssetGuid(0x0002_0000_0000_0000 + index as u128);
        let path = AssetAlias::path(&format!("derived/atlas/mip-{index}.rgba"))
            .map_err(|error| error.to_string())?;
        let handle = assets
            .load(guid, &mip.pixels, &[path], &[])
            .map_err(|error| error.to_string())?;
        resident.push(ResidentMip {
            width: mip.width,
            height: mip.height,
            handle,
        });
    }
    Ok((Arc::new(assets), resident))
}

impl Palette {
    pub fn attach_memory(
        &mut self,
        pool: torse_types::MemoryPool,
    ) -> Result<(), torse_types::Fault> {
        let bytes = self.slots.len() as u64 * 4096
            + self.surfaces.len() as u64 * size_of::<Surface>() as u64
            + 4096;
        let metadata = pool.reserve(
            torse_types::MemoryClass::Resource,
            bytes,
            torse_types::EvictionRank::Pinned,
        )?;
        let catalog = Arc::get_mut(&mut self.assets).ok_or_else(|| {
            torse_types::Fault::invalid_data("palette memory must attach before sharing")
        })?;
        catalog.attach_memory(pool).map_err(|error| match error {
            torse_assets::AssetError::Budget(fault) => fault,
            other => torse_types::Fault::invalid_data(other.to_string()),
        })?;
        self.memory = Some(Arc::new(metadata));
        Ok(())
    }
    /// Resolve every material referenced by the content registry.
    pub fn build(registry: &BlockStateRegistry, pack: &TexturePack) -> Result<Self, String> {
        if u64::from(registry.state_count()) > u64::from(u16::MAX) + 1 {
            return Err(
                "content has more block states than a material slot index can carry".into(),
            );
        }
        let mut materials = BTreeSet::new();
        for index in 0..registry.state_count() {
            let Some(definition) = registry.definition(BlockStateId(index as u16)) else {
                continue;
            };
            let profile = &definition.render_profile;
            for face in Face::ALL {
                let material = profile.face_materials.resolve(face, &profile.material);
                materials.insert((material.clone(), profile.alpha_mode));
                if let Some(overlay) = profile.face_overlays.get(face) {
                    materials.insert((overlay.material.clone(), overlay.alpha_mode));
                }
            }
        }
        if materials.len() + 1 > MAX_MATERIALS {
            return Err(format!(
                "content uses {} materials, above the {} material budget",
                materials.len() + 1,
                MAX_MATERIALS
            ));
        }

        let mut slots = BTreeMap::new();
        let mut surfaces = Vec::with_capacity(materials.len() + 1);
        for (key, alpha) in materials {
            // Uncovered materials show the missing sprite so a pack that lags
            // behind content is visible instead of silently grey.
            let surface = pack.surface(&key).map_or(
                Surface {
                    rect: pack.missing_rect(),
                    tint: [1.0, 1.0, 1.0],
                    tint_source: None,
                    animation: None,
                    alpha,
                },
                |resolved| Surface {
                    rect: resolved.rect,
                    tint: resolved.tint,
                    tint_source: resolved.tint_source,
                    animation: resolved.animation,
                    alpha,
                },
            );
            slots.insert((key, alpha), surfaces.len() as u16);
            surfaces.push(surface);
        }
        // One extra surface draws entities and other tinted debug bodies.
        surfaces.push(Surface {
            rect: pack.white_rect(),
            tint: [0.78, 0.55, 0.35],
            tint_source: None,
            animation: None,
            alpha: AlphaMode::Opaque,
        });

        let (assets, mips) = resident_mips(pack.mips(MIP_LEVELS)?)?;
        Ok(Self {
            slots: Arc::new(slots),
            surfaces: Arc::new(surfaces),
            assets,
            mips,
            memory: None,
        })
    }

    /// Slot of one material declared with a specific alpha behavior.
    pub fn slot_for(&self, material: &ContentKey, alpha: AlphaMode) -> Option<u16> {
        self.slots.get(&(material.clone(), alpha)).copied()
    }

    /// Position tint source assigned to one material slot.
    pub fn tint_source(&self, slot: u16) -> Option<TintSource> {
        self.surfaces.get(usize::from(slot))?.tint_source
    }

    /// Material table written to the GPU.
    pub fn surfaces(&self) -> &[Surface] {
        &self.surfaces
    }

    /// Uploaded mip chain; the first level is the base atlas.
    pub fn mips(&self) -> Vec<MipView<'_>> {
        self.mips
            .iter()
            .map(|mip| MipView {
                width: mip.width,
                height: mip.height,
                pixels: self
                    .assets
                    .get(mip.handle)
                    .expect("palette owns its resident mip"),
            })
            .collect()
    }

    pub fn asset_generation(&self) -> u64 {
        self.assets.asset_generation()
    }

    /// Slot reserved for entity debug bodies.
    pub fn entity_slot(&self) -> u16 {
        (self.surfaces.len() - 1) as u16
    }
}

/// Texture pack shipped with the base game.
///
/// The atlas is self-drawn by `make-textures.py` next to it. Tiles sit on a
/// 24-pixel pitch so mip extrusion has a gutter; the last two tiles are the
/// missing sprite and a white sprite for tinted debug surfaces.
pub fn base_pack() -> Result<TexturePack, String> {
    let sources = resident_base_assets()?;
    let handle = sources
        .handle(BASE_ATLAS)
        .ok_or("base atlas is not resident")?;
    let atlas = Atlas::decode(sources.get(handle).ok_or("base atlas handle is stale")?)?;
    TexturePack::new(
        atlas,
        vec![
            MaterialEntry {
                key: "base:material/stone",
                rect: [4, 4, 16, 16],
                tint: None,
                ..MaterialEntry::new("", [0; 4])
            },
            MaterialEntry {
                key: "base:material/glass",
                rect: [28, 4, 16, 16],
                tint: None,
                ..MaterialEntry::new("", [0; 4])
            },
            MaterialEntry {
                key: "base:material/plant",
                rect: [52, 4, 16, 16],
                tint: None,
                ..MaterialEntry::new("", [0; 4])
            },
        ],
        [76, 4, 16, 16],
        [100, 4, 16, 16],
    )
}

/// Assigned identities for the base texture sources.
pub const BASE_ATLAS: AssetGuid = AssetGuid(0x0001_0000_0000_0001);
/// Assigned identity for the stone item icon.
pub const BASE_ICON_STONE: AssetGuid = AssetGuid(0x0001_0000_0000_0002);
/// Assigned identity for the glass slab item icon.
pub const BASE_ICON_GLASS: AssetGuid = AssetGuid(0x0001_0000_0000_0003);
/// Assigned identity for the plant item icon.
pub const BASE_ICON_PLANT: AssetGuid = AssetGuid(0x0001_0000_0000_0004);

/// Load the base atlas and item icons into `catalog`.
///
/// The bytes still come from the compiled pack. The catalog owns the resident
/// CPU copies and accounts them. GPU upload stays with the palette and UI.
pub fn install_base_assets(catalog: &mut AssetCatalog) -> Result<(), String> {
    macro_rules! asset {
        ($name:literal) => {
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../games/base/assets/",
                $name
            )) as &[u8]
        };
    }
    let sources = [
        (
            BASE_ATLAS,
            asset!("textures/atlas.png"),
            "base:texture/atlas",
            "textures/atlas.png",
        ),
        (
            BASE_ICON_STONE,
            asset!("icons/items/stone.png"),
            "base:icon/stone",
            "icons/items/stone.png",
        ),
        (
            BASE_ICON_GLASS,
            asset!("icons/items/glass_slab.png"),
            "base:icon/glass_slab",
            "icons/items/glass_slab.png",
        ),
        (
            BASE_ICON_PLANT,
            asset!("icons/items/plant.png"),
            "base:icon/plant",
            "icons/items/plant.png",
        ),
    ];
    for (guid, bytes, key, path) in sources {
        let aliases = [
            AssetAlias::key(key).map_err(|error| error.to_string())?,
            AssetAlias::path(path).map_err(|error| error.to_string())?,
        ];
        catalog
            .load(guid, bytes, &aliases, &[])
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Source catalog used during synchronous atlas and icon decoding.
/// Processed CPU pixels have their own resident catalog; dropping this source
/// catalog frees its encoded copies after decoding.
pub fn resident_base_assets() -> Result<AssetCatalog, String> {
    let mut catalog = AssetCatalog::new(MemoryBudget::new(8 * 1024 * 1024));
    install_base_assets(&mut catalog)?;
    Ok(catalog)
}

#[cfg(test)]
impl Palette {
    fn test_mips() -> Vec<MipLevel> {
        vec![MipLevel {
            width: 1,
            height: 1,
            pixels: vec![255; 4],
        }]
    }

    /// Palette without materials, for table-shape tests.
    pub fn empty_for_tests() -> Self {
        let (assets, mips) = resident_mips(Self::test_mips()).unwrap();
        Self {
            slots: Arc::new(BTreeMap::new()),
            surfaces: Arc::new(Vec::new()),
            assets,
            mips,
            memory: None,
        }
    }

    /// Palette with a single surface, for byte-layout tests.
    pub fn single_for_tests(rect: [f32; 4], tint: [f32; 3], alpha: AlphaMode) -> Self {
        let (assets, mips) = resident_mips(Self::test_mips()).unwrap();
        Self {
            slots: Arc::new(BTreeMap::new()),
            surfaces: Arc::new(vec![Surface {
                rect,
                tint,
                tint_source: None,
                animation: None,
                alpha,
            }]),
            assets,
            mips,
            memory: None,
        }
    }

    #[cfg(feature = "gpu-tests")]
    pub fn mips_for_tests(surfaces: Vec<Surface>, levels: Vec<MipLevel>) -> Self {
        let (assets, mips) = resident_mips(levels).unwrap();
        Self {
            slots: Arc::new(BTreeMap::new()),
            surfaces: Arc::new(surfaces),
            assets,
            mips,
            memory: None,
        }
    }

    #[cfg(feature = "gpu-tests")]
    pub fn pixels_for_tests(surfaces: Vec<Surface>, pixel: [u8; 4]) -> Self {
        let (assets, mips) = resident_mips(vec![MipLevel {
            width: 1,
            height: 1,
            pixels: pixel.to_vec(),
        }])
        .unwrap();
        Self {
            slots: Arc::new(BTreeMap::new()),
            surfaces: Arc::new(surfaces),
            assets,
            mips,
            memory: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use torse_content::{ContentPack, compile, m0_fixture_pack};
    use torse_world::BlockStateRegistry;

    use super::*;

    fn key(value: &str) -> ContentKey {
        ContentKey::parse(value).expect("test key")
    }

    fn registry() -> (ContentPack, BlockStateRegistry) {
        let content = m0_fixture_pack();
        let compiled = compile(content.clone()).expect("fixture compiles");
        let registry = BlockStateRegistry::build(&compiled).expect("registry builds");
        (content, registry)
    }

    fn base_atlas() -> Atlas {
        Atlas::decode(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../games/base/assets/textures/atlas.png"
        )))
        .expect("atlas decodes")
    }

    #[test]
    fn palette_charges_actual_pixels_and_clones_share_the_same_copy() {
        let (_, registry) = registry();
        let pack = base_pack().unwrap();
        let expected = pack.mips(MIP_LEVELS).unwrap();
        let palette = Palette::build(&registry, &pack).unwrap();
        let bytes: u64 = expected.iter().map(|mip| mip.pixels.len() as u64).sum();
        assert_eq!(palette.assets.charged_bytes(), bytes);
        for (actual, expected) in palette.mips().iter().zip(&expected) {
            assert_eq!(actual.pixels, expected.pixels);
        }
        let clone = palette.clone();
        assert!(Arc::ptr_eq(&palette.assets, &clone.assets));
        let weak = Arc::downgrade(&palette.assets);
        drop(palette);
        assert_eq!(clone.assets.charged_bytes(), bytes);
        drop(clone);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn source_alias_handles_are_consumed_and_unload_releases_the_copy() {
        let mut sources = resident_base_assets().unwrap();
        let guid = sources.guid_for_path("textures/atlas.png").unwrap();
        let handle = sources.handle(guid).unwrap();
        let before = sources.charged_bytes();
        let len = sources.get(handle).unwrap().len() as u64;
        assert!(Atlas::decode(sources.get(handle).unwrap()).is_ok());
        sources.unload(handle).unwrap();
        assert_eq!(sources.charged_bytes(), before - len);
        assert!(sources.get(handle).is_none());
        assert!(sources.handle(guid).is_none());
    }

    #[test]
    fn base_pack_covers_the_base_materials() {
        let pack = base_pack().expect("base pack decodes");
        let (_, registry) = registry();
        let palette = Palette::build(&registry, &pack).expect("palette builds");

        for material in [
            "base:material/stone",
            "base:material/glass",
            "base:material/plant",
        ] {
            let slot = palette
                .slot_for(
                    &key(material),
                    match material {
                        "base:material/glass" => AlphaMode::Mask,
                        "base:material/plant" => AlphaMode::Mask,
                        _ => AlphaMode::Opaque,
                    },
                )
                .unwrap_or_else(|| panic!("missing {material}"));
            let surface = palette.surfaces()[usize::from(slot)];
            assert_ne!(
                surface.rect,
                pack.missing_rect(),
                "{material} used the missing sprite"
            );
        }
        assert_eq!(
            palette.surfaces()[usize::from(
                palette
                    .slot_for(&key("base:material/glass"), AlphaMode::Mask)
                    .unwrap(),
            )]
            .alpha,
            AlphaMode::Mask
        );
        assert_eq!(
            palette.surfaces()[usize::from(
                palette
                    .slot_for(&key("base:material/plant"), AlphaMode::Mask)
                    .unwrap(),
            )]
            .alpha,
            AlphaMode::Mask
        );
        assert_eq!(palette.entity_slot() as usize, palette.surfaces().len() - 1);
        assert_eq!(palette.mips().len(), MIP_LEVELS as usize);
        assert_eq!(
            (palette.mips()[1].width, palette.mips()[1].height),
            (palette.mips()[0].width / 2, palette.mips()[0].height / 2)
        );
    }

    #[test]
    fn uncovered_material_shows_the_missing_sprite() {
        let pack = TexturePack::new(base_atlas(), Vec::new(), [76, 4, 16, 16], [100, 4, 16, 16])
            .expect("pack builds");
        let (_, registry) = registry();
        let palette = Palette::build(&registry, &pack).expect("palette builds");
        let stone = palette
            .slot_for(&key("base:material/stone"), AlphaMode::Opaque)
            .unwrap();
        let surface = palette.surfaces()[usize::from(stone)];
        assert_eq!(surface.rect, pack.missing_rect());
        assert_eq!(surface.tint, [1.0, 1.0, 1.0]);
    }

    #[test]
    fn shared_material_has_independent_alpha_slots() {
        let (mut content, _) = registry();
        let stone_material = content
            .blocks
            .iter()
            .find(|block| block.value.id.to_string() == "base:stone")
            .unwrap()
            .value
            .render_profile
            .material
            .clone();
        let glass = content
            .blocks
            .iter_mut()
            .find(|block| block.value.id.to_string() == "base:glass_slab")
            .unwrap();
        glass.value.render_profile.material = stone_material;
        let compiled = compile(content).expect("content compiles");
        let registry = BlockStateRegistry::build(&compiled).expect("registry builds");
        let pack = base_pack().expect("base pack decodes");
        let palette = Palette::build(&registry, &pack).expect("palette builds");
        let opaque = palette
            .slot_for(&key("base:material/stone"), AlphaMode::Opaque)
            .unwrap();
        let blended = palette
            .slot_for(&key("base:material/stone"), AlphaMode::Mask)
            .unwrap();
        assert_ne!(opaque, blended);
        assert_eq!(
            palette.surfaces()[usize::from(opaque)].rect,
            palette.surfaces()[usize::from(blended)].rect
        );
    }
}
