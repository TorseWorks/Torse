//! Data-driven content pack loading for local verification.
//!
//! A game ships one atlas image plus one index that already lists its materials,
//! block shapes, face bindings, alpha modes and biome colours. This module turns
//! that index into kernel content and a texture pack, and generates a layered
//! demonstration world from a small world description, so the real texture set
//! can be inspected in a window before the game has a world generator.
//!
//! Nothing here is game specific: block, material and biome names come from the
//! index and the world file. The loader is a local harness; a durable version
//! belongs in a kernel crate once the pack format is agreed.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use torse_assets::texture::{
    Atlas, MaterialAnimation, MaterialEntry, MipmapStrategy, PositionTintProvider, TexturePack,
    TintSource,
};
use torse_content::{
    AlphaMode, BlockDefinition, BoxBounds, CollisionProfile, ContentPack, Face, FaceMaterials,
    FaceOverlay, FaceOverlays, InteractionProfile, LightTransport, OcclusionProfile, RenderModel,
    RenderProfile, SelectionProfile, Shape, SourceRef, Sourced, StateSchema,
};
use torse_types::{BlockPos, ContentKey, LocalPos, SectionPos};
use torse_world::{BlockStateId, BlockStateRegistry, Section, SectionError, WorldBounds};
use torse_world_services::{GenerationParams, SectionGenerator};

/// One loaded pack: kernel content, texture pack, and demonstration world.
pub struct GamePack {
    /// Content declarations compiled into the kernel registry.
    pub content: ContentPack,
    /// Atlas plus material rectangles.
    pub texture: TexturePack,
    /// Opaque full cubes the pack offers for placement, sorted by key.
    placeable: Vec<ContentKey>,
    /// World description for the demonstration terrain.
    pub world: WorldFile,
    /// Biome colour table keyed by biome name.
    pub biomes: BTreeMap<String, BiomeColours>,
    /// Biome used when the table or the region lookup has no better answer.
    pub default_biome: String,
    /// Limitations that must be shown when this preview pack is opened.
    pub startup_notice: Option<String>,
}

/// Colour set of one biome, in sRGB bytes.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct BiomeColours {
    /// Grass tint.
    pub grass: String,
    /// Foliage tint.
    pub foliage: String,
    /// Water tint.
    pub water: String,
}

/// Parsed world description file.
#[derive(Clone, Debug, Deserialize)]
pub struct WorldFile {
    /// Core strata and shape.
    pub world: WorldCore,
    /// Optional tree scattering.
    #[serde(default)]
    pub trees: Option<TreeSpec>,
    /// Optional plant scattering.
    #[serde(default)]
    pub plants: Option<PlantsSpec>,
}

/// Strata, water line, and shape of the demonstration terrain.
#[derive(Clone, Debug, Deserialize)]
pub struct WorldCore {
    /// Deep layer material.
    pub stone: String,
    /// Layer directly below the surface block.
    pub dirt: String,
    /// Top block of ordinary ground.
    pub surface: String,
    /// Top block at shorelines and under water.
    pub beach: String,
    /// Fluid filling columns below the sea level.
    pub water: String,
    /// Shape seed.
    pub seed: u64,
    /// Surface height average.
    pub base_y: i64,
    /// Maximum vertical deviation from `base_y`.
    pub amplitude: i64,
    /// Height up to which columns are flooded.
    pub sea_level: i64,
    /// Dirt blocks under the surface block.
    pub soil: i64,
    /// Biomes whose colours may vary across the terrain; names come from data.
    #[serde(default)]
    pub biomes: Vec<String>,
}

/// Tree scattering rules.
#[derive(Clone, Debug, Deserialize)]
pub struct TreeSpec {
    /// Trunk block.
    pub log: String,
    /// Canopy block.
    pub leaves: String,
    /// One tree per this many columns.
    pub density: u64,
    /// Trunk height in blocks.
    pub height: i64,
}

/// Plant scattering rules.
#[derive(Clone, Debug, Deserialize)]
pub struct PlantsSpec {
    /// Candidate plant blocks, chosen per column.
    pub blocks: Vec<String>,
    /// One plant per this many columns.
    pub density: u64,
}

fn read_pack_file(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    if length > limit as u64 {
        return Err(format!(
            "{}: source exceeds byte limit {limit}",
            path.display()
        ));
    }
    let bound = u64::try_from(limit)
        .ok()
        .and_then(|n| n.checked_add(1))
        .ok_or("pack source bound overflow")?;
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(bound)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    // The stream may grow after metadata was read; the bounded reader admits
    // at most one sentinel byte beyond the declared limit.
    if bytes.len() > limit {
        return Err(format!(
            "{}: source exceeds byte limit {limit}",
            path.display()
        ));
    }
    Ok(bytes)
}

fn read_pack_text(path: &Path) -> Result<String, String> {
    String::from_utf8(read_pack_file(path, 1024 * 1024)?)
        .map_err(|error| format!("{}: {error}", path.display()))
}

impl GamePack {
    /// Read one pack directory: `atlas.toml`, `atlas.png` and `world.toml`.
    ///
    /// All three files sit in the same directory, so the loader never needs to
    /// know how a game lays out its own repository.
    pub fn load(directory: &Path) -> Result<Self, String> {
        let index = directory.join("atlas.toml");
        let image = directory.join("atlas.png");
        let world = directory.join("world.toml");
        if !index.is_file() || !image.is_file() || !world.is_file() {
            return Err(format!(
                "{}: a pack needs atlas.toml, atlas.png and world.toml in one directory",
                directory.display()
            ));
        }
        let text = read_pack_text(&index)?;
        let descriptor: Descriptor =
            toml::from_str(&text).map_err(|error| format!("{}: {error}", index.display()))?;
        let bytes = read_pack_file(
            &image,
            torse_assets::texture::DecodeLimits::default().max_source_bytes,
        )?;
        let atlas =
            Atlas::decode(&bytes).map_err(|error| format!("{}: {error}", image.display()))?;
        let world_text = read_pack_text(&world)?;
        let world: WorldFile =
            toml::from_str(&world_text).map_err(|error| format!("{}: {error}", world.display()))?;

        let placeable: Vec<ContentKey> = descriptor
            .blocks
            .iter()
            .filter(|(_, spec)| spec.shape == "full_cube" && spec.alpha == "opaque")
            .filter_map(|(name, _)| {
                ContentKey::parse(&format!("{}:{}", descriptor.pack.namespace, name)).ok()
            })
            .collect();
        let source = format!("{}/atlas.toml", descriptor.pack.namespace);
        let texture = texture_pack(&descriptor, atlas)?;
        let content = content_pack(&descriptor, &source)?;
        let startup_notice = model_preview_notice(&descriptor);
        if let Some(notice) = &startup_notice {
            eprintln!("warning: {}: {notice}", index.display());
        }
        Ok(Self {
            content,
            texture,
            placeable,
            world,
            biomes: descriptor.biomes,
            default_biome: descriptor.pack.biome,
            startup_notice,
        })
    }

    /// Build the demonstration terrain for one registry.
    pub fn terrain(
        &self,
        registry: &BlockStateRegistry,
        side: u32,
        bounds: WorldBounds,
    ) -> Result<LayeredTerrain, String> {
        let core = &self.world.world;
        let state = |key: &str| -> Result<BlockStateId, String> {
            let key = ContentKey::parse(key).map_err(|error| format!("`{key}`: {error}"))?;
            registry
                .default_state(&key)
                .ok_or_else(|| format!("content has no block `{key}`"))
        };
        let stone = state(&core.stone)?;
        let dirt = state(&core.dirt)?;
        let surface = state(&core.surface)?;
        let beach = state(&core.beach)?;
        let water = state(&core.water)?;
        let tree = match &self.world.trees {
            Some(tree) => Some((
                state(&tree.log)?,
                state(&tree.leaves)?,
                tree.density.max(1),
                tree.height.max(1),
            )),
            None => None,
        };
        let plants = match &self.world.plants {
            Some(plants) => plants
                .blocks
                .iter()
                .map(|key| state(key))
                .collect::<Result<Vec<_>, _>>()?,
            None => Vec::new(),
        };
        let plant_density = self
            .world
            .plants
            .as_ref()
            .map_or(1, |plants| plants.density.max(1));
        let params = GenerationParams::new(side, bounds, surface, BlockStateId::EMPTY)
            .map_err(|error| format!("generation params: {error}"))?
            .with_version(2);
        Ok(LayeredTerrain {
            params,
            side,
            seed: core.seed,
            base_y: core.base_y,
            amplitude: core.amplitude.max(0),
            sea_level: core.sea_level,
            soil: core.soil.max(0),
            stone,
            dirt,
            surface,
            beach,
            water,
            tree,
            plants,
            plant_density,
        })
    }

    /// Position tint provider sampled from the biome table.
    pub fn tint_provider(&self) -> Arc<dyn PositionTintProvider> {
        Arc::new(BiomeTints::new(
            self.biomes.clone(),
            &self.world.world.biomes,
            self.default_biome.clone(),
        ))
    }

    /// Highest block the description can produce, trunk and canopy included.
    pub fn height_limit(&self) -> i64 {
        let core = &self.world.world;
        let trunk = self
            .world
            .trees
            .as_ref()
            .map_or(0, |tree| tree.height.max(1));
        core.base_y + core.amplitude.max(0) + trunk + 1
    }

    /// Blocks a scene may place and break.
    ///
    /// The world description's own materials come first, because those are the
    /// surfaces the terrain is made of; every other opaque full cube follows in
    /// key order. The client therefore offers real pack content instead of keys
    /// of its own.
    pub fn placeable_blocks(&self) -> Vec<ContentKey> {
        let core = &self.world.world;
        let mut keys = Vec::new();
        for name in [
            &core.stone,
            &core.dirt,
            &core.surface,
            &core.beach,
            &core.water,
        ] {
            if let Ok(key) = ContentKey::parse(name)
                && !keys.contains(&key)
            {
                keys.push(key);
            }
        }
        let extra: Vec<ContentKey> = self
            .placeable
            .iter()
            .filter(|key| !keys.contains(key))
            .cloned()
            .collect();
        keys.extend(extra);
        keys
    }
}

// ---------------------------------------------------------------------------
// Index file

#[derive(Debug, Deserialize)]
struct Descriptor {
    pack: PackInfo,
    atlas: AtlasInfo,
    #[serde(default)]
    material: Vec<MaterialSpec>,
    #[serde(default)]
    blocks: BTreeMap<String, BlockSpec>,
    #[serde(default)]
    biomes: BTreeMap<String, BiomeColours>,
}

#[derive(Debug, Deserialize)]
struct PackInfo {
    namespace: String,
    #[serde(default)]
    biome: String,
}

#[derive(Debug, Deserialize)]
struct AtlasInfo {
    #[allow(dead_code)]
    width: u32,
    #[allow(dead_code)]
    height: u32,
    #[allow(dead_code)]
    mip_levels: u32,
    #[allow(dead_code)]
    padding: u32,
    missing: [u32; 4],
    white: [u32; 4],
}

#[derive(Debug, Deserialize)]
struct MaterialSpec {
    key: String,
    #[allow(dead_code)]
    #[serde(default)]
    texture: String,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    #[serde(default = "one")]
    frames: u32,
    #[serde(default = "one")]
    frametime: u32,
    #[serde(default)]
    interpolate: bool,
    #[serde(default)]
    tint: Option<String>,
    #[serde(default)]
    tint_source: Option<String>,
    #[serde(default)]
    mipmap_strategy: Option<String>,
}

const fn one() -> u32 {
    1
}

#[derive(Clone, Copy, Debug, Deserialize)]
struct Bounds {
    min: [f32; 3],
    max: [f32; 3],
}

#[derive(Debug, Deserialize)]
struct BlockSpec {
    shape: String,
    #[serde(default)]
    bounds: Option<Bounds>,
    #[serde(default = "opaque")]
    alpha: String,
    #[serde(default)]
    overlay_alpha: Option<String>,
    #[serde(default)]
    faces: FaceSpec,
}

fn opaque() -> String {
    "opaque".to_owned()
}

#[derive(Debug, Default, Deserialize)]
struct FaceSpec {
    #[serde(default)]
    all: Option<String>,
    #[serde(default)]
    side: Option<String>,
    #[serde(default)]
    top: Option<String>,
    #[serde(default)]
    bottom: Option<String>,
    #[serde(default)]
    side_overlay: Option<String>,
}

fn texture_pack(descriptor: &Descriptor, atlas: Atlas) -> Result<TexturePack, String> {
    let mut materials = Vec::with_capacity(descriptor.material.len());
    for entry in &descriptor.material {
        let tint = entry.tint.as_deref().map(parse_colour).transpose()?;
        let tint_source = entry
            .tint_source
            .as_deref()
            .map(|source| match source {
                "grass" => Ok(TintSource::Grass),
                "foliage" => Ok(TintSource::Foliage),
                "water" => Ok(TintSource::Water),
                other => Err(format!("{}: unknown tint source `{other}`", entry.key)),
            })
            .transpose()?;
        // The index records a reference colour next to the source that produces
        // the real one. Keeping both would multiply the tint twice.
        let tint = if tint_source.is_some() { None } else { tint };
        let mipmap_strategy = parse_mipmap_strategy(&entry.key, entry.mipmap_strategy.as_deref())?;
        let frames = entry.frames.max(1);
        let animation = (frames > 1).then(|| MaterialAnimation {
            frame_count: frames,
            // The index counts animation frames in ticks, twenty per second.
            frame_duration: Duration::from_millis(u64::from(entry.frametime.max(1)) * 50),
            frame_order: None,
            frame_durations: None,
            interpolate: entry.interpolate,
        });
        materials.push(MaterialEntry {
            key: leak(&entry.key),
            rect: [entry.x, entry.y, entry.width, entry.height],
            tint,
            tint_source,
            animation,
            mipmap_strategy,
        });
    }
    TexturePack::new(
        atlas,
        materials,
        descriptor.atlas.missing,
        descriptor.atlas.white,
    )
}

fn content_pack(descriptor: &Descriptor, source: &str) -> Result<ContentPack, String> {
    let namespace = &descriptor.pack.namespace;
    let schema = key(&format!("{namespace}:state/simple"))?;
    let mut blocks = Vec::with_capacity(descriptor.blocks.len());
    for (name, spec) in &descriptor.blocks {
        let id = format!("{namespace}:{name}");
        let shape =
            parse_shape(&spec.shape, spec.bounds).map_err(|error| format!("{id}: {error}"))?;
        let alpha = parse_alpha(&spec.alpha).map_err(|error| format!("{id}: {error}"))?;
        let overlay_alpha = match &spec.overlay_alpha {
            Some(value) => parse_alpha(value).map_err(|error| format!("{id}: {error}"))?,
            None => AlphaMode::Mask,
        };
        let faces = &spec.faces;
        let fallback = faces
            .all
            .as_ref()
            .or(faces.side.as_ref())
            .or(faces.top.as_ref())
            .or(faces.bottom.as_ref())
            .ok_or_else(|| format!("{id}: block declares no face material"))?;
        let mut face_materials = FaceMaterials::none();
        if faces.all.is_none() {
            let side = faces.side.as_ref().unwrap_or(fallback);
            for face in [Face::NegX, Face::PosX, Face::NegZ, Face::PosZ] {
                face_materials.set(face, key(side)?);
            }
            face_materials.set(Face::NegY, key(faces.bottom.as_ref().unwrap_or(side))?);
            face_materials.set(Face::PosY, key(faces.top.as_ref().unwrap_or(side))?);
        }
        let mut face_overlays = FaceOverlays::none();
        if let Some(overlay) = &faces.side_overlay {
            for face in [Face::NegX, Face::PosX, Face::NegZ, Face::PosZ] {
                face_overlays.set(
                    face,
                    FaceOverlay {
                        material: key(overlay)?,
                        alpha_mode: overlay_alpha,
                    },
                );
            }
        }
        let solid = shape == Shape::FullCube && alpha == AlphaMode::Opaque;
        // Models are visual only. An index that names one without quads still
        // has to load; collision and selection stay unset rather than copying
        // the visual shape into those profiles.
        let selection = match &shape {
            Shape::Model(_) => None,
            other => Some(other.clone()),
        };
        blocks.push(Sourced::new(
            SourceRef::new(source, 1),
            BlockDefinition {
                id: key(&id)?,
                definition_version: 1,
                state_schema: schema.clone(),
                render_profile: RenderProfile {
                    shape: shape.clone(),
                    material: key(fallback)?,
                    face_materials,
                    face_overlays,
                    alpha_mode: alpha,
                },
                collision_profile: CollisionProfile {
                    shape: collision_shape(&shape),
                },
                selection_profile: SelectionProfile { shape: selection },
                occlusion_profile: OcclusionProfile {
                    face_mask: if solid { 0b11_1111 } else { 0 },
                },
                face_support: torse_content::FaceSupport::NONE,
                light_transport: LightTransport {
                    absorption: 0.0,
                    emission: 0.0,
                },
                interaction_profile: InteractionProfile {
                    action: None,
                    completes_to: None,
                },
                component_defaults: Vec::new(),
                capability_bindings: Vec::new(),
                state_overrides: Vec::new(),
                behavior_bindings: Vec::new(),
                tags: Vec::new(),
            },
        ));
    }
    Ok(ContentPack {
        state_schemas: vec![Sourced::new(
            SourceRef::new(source, 1),
            StateSchema {
                id: schema,
                schema_version: 1,
                finite_fields: Vec::new(),
                component_schemas: Vec::new(),
            },
        )],
        capabilities: Vec::new(),
        actions: Vec::new(),
        modules: Vec::new(),
        blocks,
    })
}

fn parse_shape(value: &str, bounds: Option<Bounds>) -> Result<Shape, String> {
    if value != "box" && bounds.is_some() {
        return Err("only box geometry accepts bounds".to_owned());
    }
    match value {
        "full_cube" => Ok(Shape::FullCube),
        "lower_slab" => Ok(Shape::LowerSlab),
        "cross" => Ok(Shape::Cross),
        "panel" => Ok(Shape::Panel),
        // Preset geometry is not in the index. Record an empty visual model
        // so the block still loads; callers must not reuse it for physics.
        "model" => Ok(Shape::Model(Arc::new(RenderModel::default()))),
        "box" => {
            let bounds = bounds.ok_or_else(|| "box geometry needs bounds".to_owned())?;
            if !(0..3).all(|axis| {
                let (minimum, maximum) = (bounds.min[axis], bounds.max[axis]);
                minimum.is_finite()
                    && maximum.is_finite()
                    && minimum >= 0.0
                    && minimum < maximum
                    && maximum <= 1.0
            }) {
                return Err("invalid cell-local box bounds".to_owned());
            }
            Ok(Shape::Box(BoxBounds {
                min: bounds.min,
                max: bounds.max,
            }))
        }
        other => Err(format!("unsupported shape `{other}`")),
    }
}

/// The atlas index contains no model quads. Accepting its names is only a
/// metadata preview; callers must carry this notice to the visible client.
fn model_preview_notice(descriptor: &Descriptor) -> Option<String> {
    let count = descriptor
        .blocks
        .values()
        .filter(|block| block.shape == "model")
        .count();
    (count != 0).then(|| format!(
        "Preview warning: {count} model block(s) are invisible (no geometry, collision or selection)"
    ))
}

/// Map one index name onto the kernel mipmap strategy.
///
/// `cutout` and `strict_cutout` both request coverage-preserving mips. This
/// loader has one coverage strategy, so either name selects it. An omitted
/// name averages texels, the same as `mean` and `average`.
fn parse_mipmap_strategy(key: &str, value: Option<&str>) -> Result<MipmapStrategy, String> {
    match value {
        Some("dark_cutout") => Ok(MipmapStrategy::DarkCutout),
        Some("cutout") | Some("strict_cutout") => Ok(MipmapStrategy::CutoutCoverage),
        Some("mean") | Some("average") | None => Ok(MipmapStrategy::Average),
        Some(other) => Err(format!("{key}: unsupported mipmap strategy `{other}`")),
    }
}

fn parse_alpha(value: &str) -> Result<AlphaMode, String> {
    match value {
        "opaque" => Ok(AlphaMode::Opaque),
        "mask" => Ok(AlphaMode::Mask),
        "blend" => Ok(AlphaMode::Blend),
        other => Err(format!("unsupported alpha mode `{other}`")),
    }
}

fn collision_shape(shape: &Shape) -> Option<Shape> {
    match shape {
        Shape::FullCube | Shape::LowerSlab => Some(shape.clone()),
        Shape::Panel
        | Shape::Cross
        | Shape::Stair
        | Shape::Corner
        | Shape::Box(_)
        | Shape::Model(_)
        | Shape::Compound(_) => None,
    }
}

fn parse_colour(value: &str) -> Result<[f32; 3], String> {
    let digits = value
        .strip_prefix('#')
        .ok_or_else(|| format!("colour `{value}` must start with `#`"))?;
    if digits.len() != 6 {
        return Err(format!("colour `{value}` must be six hex digits"));
    }
    let channel = |offset: usize| -> Result<f32, String> {
        u8::from_str_radix(&digits[offset..offset + 2], 16)
            .map(|value| f32::from(value) / 255.0)
            .map_err(|_| format!("colour `{value}` has a non-hex digit"))
    };
    Ok([channel(0)?, channel(2)?, channel(4)?])
}

fn key(value: &str) -> Result<ContentKey, String> {
    ContentKey::parse(value).map_err(|error| format!("content key `{value}`: {error}"))
}

fn leak(value: &str) -> &'static str {
    Box::leak(value.to_owned().into_boxed_str())
}

// ---------------------------------------------------------------------------
// Biome tinting

/// Tint used when a pack declares no usable biome colour.
const NEUTRAL_TINT: ([u8; 3], [u8; 3], [u8; 3]) =
    ([255, 255, 255], [255, 255, 255], [255, 255, 255]);

/// Position tint sampled from the pack's biome table.
pub struct BiomeTints {
    order: Vec<([u8; 3], [u8; 3], [u8; 3])>,
    fallback: ([u8; 3], [u8; 3], [u8; 3]),
}

impl BiomeTints {
    /// Build from the pack's biome table.
    ///
    /// `selected` names the biomes the terrain may draw a tint from and comes
    /// from the world description, so no biome name is fixed in kernel code.
    fn new(biomes: BTreeMap<String, BiomeColours>, selected: &[String], default: String) -> Self {
        let order: Vec<_> = selected
            .iter()
            .filter_map(|name| biomes.get(name))
            .map(entry)
            .collect();
        let fallback = biomes
            .get(&default)
            .or_else(|| biomes.values().next())
            .map_or(NEUTRAL_TINT, entry);
        Self { order, fallback }
    }

    fn pick(&self, position: BlockPos) -> &([u8; 3], [u8; 3], [u8; 3]) {
        if self.order.is_empty() {
            return &self.fallback;
        }
        // One biome region every twenty-four blocks: small enough that a
        // nearby view still shows more than one grass colour, large enough that
        // the borders read as regions instead of noise.
        let cell = 24;
        let x = position.x.div_euclid(cell);
        let z = position.z.div_euclid(cell);
        let hash = mix(self.hash(x, z));
        &self.order[(hash % self.order.len() as u64) as usize]
    }

    fn hash(&self, x: i64, z: i64) -> u64 {
        let mut value = 0x9E37_79B9_7F4A_7C15u64;
        value ^= (x as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value ^= (z as u64).wrapping_mul(0x94D0_49BB_1331_11EB);
        value
    }
}

impl PositionTintProvider for BiomeTints {
    fn sample(&self, source: TintSource, position: BlockPos) -> [u8; 3] {
        let colours = self.pick(position);
        match source {
            TintSource::Grass => colours.0,
            TintSource::Foliage => colours.1,
            TintSource::Water => colours.2,
        }
    }

    fn revision(&self, _section: SectionPos) -> u64 {
        1
    }
}

fn entry(colours: &BiomeColours) -> ([u8; 3], [u8; 3], [u8; 3]) {
    (
        parse_rgb(&colours.grass),
        parse_rgb(&colours.foliage),
        parse_rgb(&colours.water),
    )
}

fn parse_rgb(value: &str) -> [u8; 3] {
    match parse_colour(value) {
        Ok(channels) => channels.map(|channel| (channel * 255.0).round() as u8),
        Err(_) => [255, 255, 255],
    }
}

fn mix(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    value ^= value >> 33;
    value = value.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    value ^ (value >> 33)
}

// ---------------------------------------------------------------------------
// Demonstration terrain

type Tree = (BlockStateId, BlockStateId, u64, i64);

/// Layered terrain: stone, dirt, surface block, water, plants and small trees.
#[derive(Clone, Debug)]
pub struct LayeredTerrain {
    params: GenerationParams,
    side: u32,
    seed: u64,
    base_y: i64,
    amplitude: i64,
    sea_level: i64,
    soil: i64,
    stone: BlockStateId,
    dirt: BlockStateId,
    surface: BlockStateId,
    beach: BlockStateId,
    water: BlockStateId,
    tree: Option<Tree>,
    plants: Vec<BlockStateId>,
    plant_density: u64,
}

impl LayeredTerrain {
    /// Ground height at one column. The top ground block sits at `height - 1`.
    pub fn height_at(&self, x: i64, z: i64) -> i64 {
        let coarse = column_noise(self.seed ^ 0x51ED_2701, x, z, 24);
        let fine = column_noise(self.seed ^ 0x2545_F491, x, z, 7);
        let mixed = (coarse * 3 + fine) / 4;
        self.base_y + (mixed as i64 - 128) * self.amplitude / 128
    }

    fn column_hash(&self, x: i64, z: i64) -> u64 {
        mix(self.seed
            ^ (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (z as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
    }

    fn fill(&self, section: SectionPos) -> Result<Section, SectionError> {
        let side = self.side;
        let origin = section.to_block(LocalPos { x: 0, y: 0, z: 0 }, side)?;
        section.to_block(
            LocalPos {
                x: side - 1,
                y: side - 1,
                z: side - 1,
            },
            side,
        )?;
        let mut result = Section::new(side, self.params.air)?;
        let side_i = i64::from(side);
        for local_z in 0..side_i {
            let world_z = origin.z + local_z;
            for local_x in 0..side_i {
                let world_x = origin.x + local_x;
                let height = self.height_at(world_x, world_z);
                let shoreline = height <= self.sea_level + 1;
                for local_y in 0..side_i {
                    let world_y = origin.y + local_y;
                    if !self.params.bounds.contains_y(world_y) {
                        continue;
                    }
                    let state = if world_y < height {
                        let depth = height - 1 - world_y;
                        if depth == 0 || shoreline {
                            if shoreline { self.beach } else { self.surface }
                        } else if depth <= self.soil {
                            self.dirt
                        } else {
                            self.stone
                        }
                    } else if world_y < self.sea_level {
                        self.water
                    } else {
                        continue;
                    };
                    result.set(
                        LocalPos {
                            x: local_x as u32,
                            y: local_y as u32,
                            z: local_z as u32,
                        },
                        state,
                    )?;
                }
                self.scatter_plants(&mut result, local_x, local_z, height, side_i)?;
            }
        }
        self.scatter_trees(&mut result, origin.x, origin.z)?;
        Ok(result)
    }

    /// Whether one column carries a tree.
    ///
    /// A tree depends only on its column and the generator seed, so the
    /// neighbouring section reaches the same answer and can draw the part of
    /// the canopy that crosses the boundary.
    fn has_tree(&self, x: i64, z: i64) -> bool {
        let Some(tree) = self.tree else {
            return false;
        };
        if self.height_at(x, z) <= self.sea_level + 1 {
            return false;
        }
        self.column_hash(x, z).is_multiple_of(tree.2)
    }

    /// Draw every tree whose canopy can reach this section.
    ///
    /// Strata are independent per column, but a canopy is two blocks wide, so
    /// the origin scan reaches two columns past each edge and each section keeps
    /// only the part inside its own bounds.
    fn scatter_trees(
        &self,
        result: &mut Section,
        base_x: i64,
        base_z: i64,
    ) -> Result<(), SectionError> {
        let Some(tree) = self.tree else {
            return Ok(());
        };
        let side = i64::from(self.side);
        for z in (base_z - 2)..(base_z + side + 2) {
            for x in (base_x - 2)..(base_x + side + 2) {
                if !self.has_tree(x, z) {
                    continue;
                }
                let height = self.height_at(x, z);
                self.place_tree(result, x - base_x, z - base_z, height, tree)?;
            }
        }
        Ok(())
    }

    fn scatter_plants(
        &self,
        result: &mut Section,
        local_x: i64,
        local_z: i64,
        height: i64,
        _side: i64,
    ) -> Result<(), SectionError> {
        if height <= self.sea_level {
            return Ok(());
        }
        let hash = self.column_hash(local_x, local_z);
        if !self.plants.is_empty() && hash.is_multiple_of(self.plant_density) {
            let plant =
                self.plants[(hash / self.plant_density % self.plants.len() as u64) as usize];
            self.put(result, local_x, height, local_z, plant)?;
        }
        Ok(())
    }

    fn place_tree(
        &self,
        result: &mut Section,
        local_x: i64,
        local_z: i64,
        height: i64,
        tree: Tree,
    ) -> Result<(), SectionError> {
        let (log, leaves, _, trunk) = tree;
        for step in 0..trunk {
            self.put(result, local_x, height + step, local_z, log)?;
        }
        let crown = height + trunk;
        for dy in -1..=1 {
            let half = if dy == 0 { 2 } else { 1 };
            for dx in -half..=half {
                for dz in -half..=half {
                    if dx == 0 && dz == 0 && dy < 0 {
                        continue;
                    }
                    self.put(result, local_x + dx, crown + dy, local_z + dz, leaves)?;
                }
            }
        }
        Ok(())
    }

    fn put(
        &self,
        result: &mut Section,
        x: i64,
        y: i64,
        z: i64,
        state: BlockStateId,
    ) -> Result<(), SectionError> {
        if x < 0 || z < 0 || y < 0 {
            return Ok(());
        }
        let side = i64::from(self.side);
        if x >= side || z >= side || y >= side {
            return Ok(());
        }
        result.set(
            LocalPos {
                x: x as u32,
                y: y as u32,
                z: z as u32,
            },
            state,
        )?;
        Ok(())
    }
}

impl SectionGenerator for LayeredTerrain {
    fn side(&self) -> u32 {
        self.side
    }

    fn generation_version(&self) -> u32 {
        self.params.generation_version
    }

    fn generate(&self, section: SectionPos) -> Result<Section, SectionError> {
        self.fill(section)
    }
}

/// Integer value noise on a lattice of `cell` blocks, in `0..=255`.
fn column_noise(seed: u64, x: i64, z: i64, cell: i64) -> u64 {
    let cell = cell.max(1);
    let x0 = x.div_euclid(cell);
    let z0 = z.div_euclid(cell);
    let fx = x.rem_euclid(cell) as f64 / cell as f64;
    let fz = z.rem_euclid(cell) as f64 / cell as f64;
    let corner = |cx: i64, cz: i64| -> f64 {
        let hash = mix(seed
            ^ (cx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (cz as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F));
        (hash % 256) as f64
    };
    let smooth = |t: f64| t * t * (3.0 - 2.0 * t);
    let (sx, sz) = (smooth(fx), smooth(fz));
    let top = corner(x0, z0) * (1.0 - sx) + corner(x0 + 1, z0) * sx;
    let bottom = corner(x0, z0 + 1) * (1.0 - sx) + corner(x0 + 1, z0 + 1) * sx;
    let value = top * (1.0 - sz) + bottom * sz;
    value.round().clamp(0.0, 255.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_sources_are_bounded_before_reading() {
        let directory =
            std::env::temp_dir().join(format!("torse-pack-source-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("source");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(read_pack_file(&path, 3).unwrap(), b"abc");
        assert!(
            read_pack_file(&path, 2)
                .unwrap_err()
                .contains("source exceeds")
        );
        std::fs::File::create(&path)
            .unwrap()
            .set_len(1024 * 1024 + 1)
            .unwrap();
        assert!(
            read_pack_text(&path)
                .unwrap_err()
                .contains("source exceeds")
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn coverage_names_map_and_unknown_names_do_not() {
        assert_eq!(
            parse_mipmap_strategy("base:material/flower", Some("strict_cutout")).unwrap(),
            MipmapStrategy::CutoutCoverage
        );
        assert_eq!(
            parse_mipmap_strategy("base:material/flower", Some("cutout")).unwrap(),
            MipmapStrategy::CutoutCoverage
        );
        assert_eq!(
            parse_mipmap_strategy("base:material/stone", Some("mean")).unwrap(),
            MipmapStrategy::Average
        );
        assert_eq!(
            parse_mipmap_strategy("base:material/stone", Some("average")).unwrap(),
            MipmapStrategy::Average
        );
        assert_eq!(
            parse_mipmap_strategy("base:material/stone", None).unwrap(),
            MipmapStrategy::Average
        );
        assert_eq!(
            parse_mipmap_strategy("base:material/leaves", Some("dark_cutout")).unwrap(),
            MipmapStrategy::DarkCutout
        );
        let error = parse_mipmap_strategy("base:material/flower", Some("blurry")).unwrap_err();
        assert_eq!(
            error,
            "base:material/flower: unsupported mipmap strategy `blurry`"
        );
    }

    #[test]
    fn strict_cutout_material_is_kept_and_a_later_unknown_name_rejects_the_pack() {
        let atlas = Atlas::decode(&solid_png(16, 16)).expect("png decodes");
        let mut descriptor = empty_descriptor();
        descriptor.material.push(material(
            "base:material/flower",
            Some("strict_cutout"),
            2,
            2,
        ));
        descriptor
            .material
            .push(material("base:material/stone", Some("mean"), 8, 2));
        let pack = texture_pack(&descriptor, atlas.clone()).expect("known names build a pack");
        assert!(
            pack.surface(&key("base:material/flower").unwrap())
                .is_some()
        );
        assert!(pack.surface(&key("base:material/stone").unwrap()).is_some());

        descriptor
            .material
            .push(material("base:material/bad", Some("blurry"), 2, 8));
        let error = texture_pack(&descriptor, atlas).unwrap_err();
        assert_eq!(
            error,
            "base:material/bad: unsupported mipmap strategy `blurry`"
        );
    }

    fn empty_descriptor() -> Descriptor {
        Descriptor {
            pack: PackInfo {
                namespace: "base".to_owned(),
                biome: "plains".to_owned(),
            },
            atlas: AtlasInfo {
                width: 16,
                height: 16,
                mip_levels: 1,
                padding: 1,
                missing: [2, 14, 1, 1],
                white: [8, 14, 1, 1],
            },
            material: Vec::new(),
            blocks: BTreeMap::new(),
            biomes: BTreeMap::new(),
        }
    }

    fn material(key: &str, strategy: Option<&str>, x: u32, y: u32) -> MaterialSpec {
        MaterialSpec {
            key: key.to_owned(),
            texture: String::new(),
            x,
            y,
            width: 4,
            height: 4,
            frames: 1,
            frametime: 1,
            interpolate: false,
            tint: None,
            tint_source: None,
            mipmap_strategy: strategy.map(str::to_owned),
        }
    }

    #[test]
    fn model_shape_stays_visual_and_a_later_unknown_shape_rejects_the_pack() {
        let shape = parse_shape("model", None).expect("model is a shape");
        assert!(matches!(shape, Shape::Model(ref model) if model.quads.is_empty()));
        assert!(collision_shape(&shape).is_none());

        let mut descriptor = empty_descriptor();
        descriptor.blocks.insert(
            "stone".to_owned(),
            block_spec("full_cube", "opaque", "base:material/stone"),
        );
        descriptor.blocks.insert(
            "log".to_owned(),
            block_spec("model", "opaque", "base:material/log"),
        );
        let pack = content_pack(&descriptor, "base/atlas.toml").expect("known shapes build");
        let stone = pack
            .blocks
            .iter()
            .find(|block| block.value.id.to_string() == "base:stone")
            .expect("stone");
        assert_eq!(stone.value.render_profile.shape, Shape::FullCube);
        assert_eq!(stone.value.selection_profile.shape, Some(Shape::FullCube));
        let log = pack
            .blocks
            .iter()
            .find(|block| block.value.id.to_string() == "base:log")
            .expect("log");
        assert!(matches!(log.value.render_profile.shape, Shape::Model(_)));
        assert!(log.value.collision_profile.shape.is_none());
        assert!(log.value.selection_profile.shape.is_none());
        torse_content::compile(pack).expect("empty visual model compiles");

        descriptor.blocks.insert(
            "zz_bad".to_owned(),
            block_spec("mesh", "opaque", "base:material/stone"),
        );
        let error = content_pack(&descriptor, "base/atlas.toml").unwrap_err();
        assert_eq!(error, "base:zz_bad: unsupported shape `mesh`");
    }

    #[test]
    fn model_preview_reports_missing_geometry_and_ordinary_shapes_are_quiet() {
        let mut descriptor = empty_descriptor();
        descriptor.blocks.insert(
            "stone".into(),
            block_spec("full_cube", "opaque", "base:material/stone"),
        );
        assert!(model_preview_notice(&descriptor).is_none());
        descriptor.blocks.insert(
            "log".into(),
            block_spec("model", "opaque", "base:material/log"),
        );
        descriptor.blocks.insert(
            "plant".into(),
            block_spec("model", "mask", "base:material/plant"),
        );
        let notice = model_preview_notice(&descriptor).expect("model warning");
        assert!(notice.contains("2 model block(s)"));
        for limitation in ["invisible", "geometry", "collision", "selection"] {
            assert!(notice.contains(limitation));
        }
    }

    #[test]
    fn box_shape_keeps_its_bounds_and_rejects_a_partial_declaration() {
        let bounds = Bounds {
            min: [0.0, 0.0, 0.0],
            max: [1.0, 0.0625, 1.0],
        };
        let shape = parse_shape("box", Some(bounds)).expect("box");
        assert_eq!(
            shape,
            Shape::Box(BoxBounds {
                min: bounds.min,
                max: bounds.max,
            })
        );

        for invalid in [
            Bounds {
                min: [f32::NAN, 0.0, 0.0],
                max: [1.0; 3],
            },
            Bounds {
                min: [0.0; 3],
                max: [f32::INFINITY, 1.0, 1.0],
            },
            Bounds {
                min: [-0.1, 0.0, 0.0],
                max: [1.0; 3],
            },
            Bounds {
                min: [0.0; 3],
                max: [1.1, 1.0, 1.0],
            },
            Bounds {
                min: [0.0; 3],
                max: [0.0, 1.0, 1.0],
            },
        ] {
            assert!(parse_shape("box", Some(invalid)).is_err());
        }
        assert_eq!(
            parse_shape("box", None).unwrap_err(),
            "box geometry needs bounds"
        );
        assert_eq!(
            parse_shape("full_cube", Some(bounds)).unwrap_err(),
            "only box geometry accepts bounds"
        );
        assert_eq!(
            parse_shape(
                "box",
                Some(Bounds {
                    min: [0.0, 0.5, 0.0],
                    max: [1.0, 0.25, 1.0],
                })
            )
            .unwrap_err(),
            "invalid cell-local box bounds"
        );

        let mut descriptor = empty_descriptor();
        let mut carpet = block_spec("box", "opaque", "base:material/wool");
        carpet.bounds = Some(bounds);
        descriptor.blocks.insert("carpet".to_owned(), carpet);
        descriptor.blocks.insert(
            "stone".to_owned(),
            block_spec("full_cube", "opaque", "base:material/stone"),
        );
        let pack = content_pack(&descriptor, "base/atlas.toml").expect("box builds");
        let carpet = pack
            .blocks
            .iter()
            .find(|block| block.value.id.to_string() == "base:carpet")
            .expect("carpet");
        assert_eq!(carpet.value.render_profile.shape, shape);
        assert_eq!(carpet.value.selection_profile.shape, Some(shape));
        assert!(carpet.value.collision_profile.shape.is_none());
        let stone = pack
            .blocks
            .iter()
            .find(|block| block.value.id.to_string() == "base:stone")
            .expect("stone");
        assert_eq!(stone.value.render_profile.shape, Shape::FullCube);
        assert_eq!(
            stone.value.collision_profile.shape.as_ref(),
            Some(&Shape::FullCube)
        );

        descriptor.blocks.insert(
            "open".to_owned(),
            block_spec("box", "opaque", "base:material/wool"),
        );
        assert_eq!(
            content_pack(&descriptor, "base/atlas.toml").unwrap_err(),
            "base:open: box geometry needs bounds"
        );
    }

    fn block_spec(shape: &str, alpha: &str, material: &str) -> BlockSpec {
        BlockSpec {
            shape: shape.to_owned(),
            bounds: None,
            alpha: alpha.to_owned(),
            overlay_alpha: None,
            faces: FaceSpec {
                all: Some(material.to_owned()),
                ..FaceSpec::default()
            },
        }
    }

    fn solid_png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        let pixels = vec![255u8; (width * height * 4) as usize];
        writer.write_image_data(&pixels).expect("png pixels");
        drop(writer);
        bytes
    }
}
