//! Cooked runtime tables.
//!
//! Author files such as locale text stay outside the hot path. `cook` turns a
//! compiled content pack, image identities, and locale maps into one table.
//! Image bytes stay in the asset catalog; this table stores their guid.
//! Authority rows cover occlusion and collision. Presentation rows cover
//! material slots, image identities, and locale text. The two digests move
//! independently.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest as ShaDigest, Sha256};
use torse_types::{AssetGuid, ContentKey, Digest, valid_asset_path};

use crate::compiler::{CompiledContent, MAX_FINITE_STATES};
use crate::descriptor::{AlphaMode, BoxBounds, Face, FaceSupport, Shape, StateProfile};

const MAGIC: &[u8; 8] = b"TCOOK003";
const VERSION: u32 = 3;

/// Limits checked before allocating tables or accepting a count from a file.
#[derive(Clone, Copy, Debug)]
pub struct CookLimits {
    pub max_file_bytes: usize,
    pub max_owned_bytes: usize,
    pub max_items: usize,
}

impl Default for CookLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 64 * 1024 * 1024,
            max_owned_bytes: 64 * 1024 * 1024,
            max_items: 1_000_000,
        }
    }
}

/// One dense material slot. Alpha modes distinguish shared texture keys.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct CookedMaterial {
    pub key: ContentKey,
    pub alpha: AlphaMode,
}

/// One image identity recorded in the cooked table.
///
/// The pixels themselves are not copied here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CookImage {
    /// Assigned asset guid.
    pub guid: AssetGuid,
    /// SHA-256 of the PNG source; the guid stays stable when bytes change.
    pub source_digest: Digest,
    /// Content-key alias for the image.
    pub key: ContentKey,
    /// Relative pack path alias for the image.
    pub path: String,
}

/// One language table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CookedLocale {
    /// Language tag such as `en` or `zh-Hans`.
    pub language: String,
    /// Message key to display text.
    pub entries: BTreeMap<String, String>,
}

/// One expanded block state.
#[derive(Clone, Debug, PartialEq)]
pub struct CookedState {
    /// Block identity. This remains a content key, not an asset guid.
    pub block: ContentKey,
    /// Index of this state within the block, last field varying fastest.
    pub index: u16,
    /// Finite field values in schema order.
    pub values: Vec<u16>,
    /// Six-bit occlusion mask.
    pub occlusion: u8,
    /// Effective independent support predicates for this state.
    pub face_support: FaceSupport,
    /// Authoritative collision shape, if any.
    pub collision: Option<Shape>,
    /// Dense material slot for each face, in `-x, +x, -y, +y, -z, +z` order.
    pub materials: [u32; 6],
    /// Optional overlay slots; their own alpha modes live in the slot table.
    pub overlays: [Option<u32>; 6],
}

/// Runtime table produced from author input.
#[derive(Clone, Debug, PartialEq)]
pub struct CookedTables {
    /// Full compiler compatibility identities, separate from table checksums.
    pub content_authority_digest: Digest,
    pub content_presentation_digest: Digest,
    /// Digest of occlusion and collision rows.
    pub authority_digest: Digest,
    /// Digest of material slots, image identities, and locale text.
    pub presentation_digest: Digest,
    /// Expanded states in block-key order.
    pub states: Vec<CookedState>,
    pub materials: Vec<CookedMaterial>,
    /// Images sorted by guid.
    pub images: Vec<CookImage>,
    /// Locales sorted by language tag.
    pub locales: Vec<CookedLocale>,
}

/// Why a cook or a read did not produce a table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CookError {
    /// A block names a state schema that is not in the compiled pack.
    MissingSchema(ContentKey),
    /// A finite field has no legal values.
    EmptyField(ContentKey),
    /// The state product exceeds the content limit.
    StateLimit(ContentKey),
    /// Collision uses a visual model.
    VisualCollision(ContentKey),
    /// Two images share a guid or a key.
    DuplicateImage,
    /// Two locale tables share a language tag.
    DuplicateLanguage(String),
    /// A locale line is missing `=` or repeats a key.
    Locale(String),
    /// The byte length does not match a cooked table.
    Truncated,
    /// Bytes remain after a complete table.
    Trailing,
    /// A stored digest does not match the rows.
    DigestMismatch,
    /// The magic or version is not this table.
    Header,
    /// Counts, owned storage, or encoded output exceed the configured ceiling.
    Limit,
    /// Noncanonical rows, invalid geometry, aliases, or material references.
    Invalid(String),
}

impl std::fmt::Display for CookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingSchema(key) => write!(f, "missing state schema {key}"),
            Self::EmptyField(key) => write!(f, "state field of {key} has no values"),
            Self::StateLimit(key) => write!(f, "state expansion of {key} exceeds the limit"),
            Self::VisualCollision(key) => write!(f, "collision of {key} uses a visual model"),
            Self::DuplicateImage => write!(f, "duplicate cooked image identity"),
            Self::DuplicateLanguage(language) => write!(f, "duplicate locale {language}"),
            Self::Locale(message) => write!(f, "{message}"),
            Self::Truncated => write!(f, "cooked table ended early"),
            Self::Trailing => write!(f, "cooked table has trailing bytes"),
            Self::DigestMismatch => write!(f, "cooked table digest does not match its rows"),
            Self::Header => write!(f, "cooked table header is not recognized"),
            Self::Limit => write!(f, "cooked table exceeds its resource limit"),
            Self::Invalid(message) => write!(f, "invalid cooked table: {message}"),
        }
    }
}

impl std::error::Error for CookError {}

/// Parse one locale file body.
///
/// Blank lines are skipped. Each remaining line is `key=value`.
pub fn parse_locale(text: &str) -> Result<BTreeMap<String, String>, CookError> {
    let mut entries = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(CookError::Locale(format!(
                "line {} is missing a separator",
                index + 1
            )));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(CookError::Locale(format!(
                "line {} has an empty key",
                index + 1
            )));
        }
        if entries
            .insert(key.to_owned(), value.trim().to_owned())
            .is_some()
        {
            return Err(CookError::Locale(format!("duplicate locale key {key}")));
        }
    }
    Ok(entries)
}

/// Build a cooked table. The result is independent of input list order.
pub fn cook(
    content: &CompiledContent,
    images: &[CookImage],
    locales: &[CookedLocale],
) -> Result<CookedTables, CookError> {
    if content.blocks.len() > usize::from(u16::MAX) {
        return Err(CookError::Limit);
    }
    let mut material_set = BTreeSet::new();
    for block in content.blocks.values() {
        for face in Face::ALL {
            material_set.insert(CookedMaterial {
                key: block
                    .render_profile
                    .face_materials
                    .resolve(face, &block.render_profile.material)
                    .clone(),
                alpha: block.render_profile.alpha_mode,
            });
            if let Some(overlay) = block.render_profile.face_overlays.get(face) {
                material_set.insert(CookedMaterial {
                    key: overlay.material.clone(),
                    alpha: overlay.alpha_mode,
                });
            }
            if material_set.len() > usize::from(u16::MAX) {
                return Err(CookError::Limit);
            }
        }
    }
    let materials: Vec<_> = material_set.into_iter().collect();
    let slots: BTreeMap<_, _> = materials
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, material)| (material, index as u32))
        .collect();
    let mut states = Vec::new();
    for block in content.blocks.values() {
        let Some(schema) = content.state_schemas.get(&block.state_schema) else {
            return Err(CookError::MissingSchema(block.state_schema.clone()));
        };
        let count = state_count(&schema.finite_fields).map_err(|_| {
            if schema
                .finite_fields
                .iter()
                .any(|field| field.values.is_empty())
            {
                CookError::EmptyField(block.id.clone())
            } else {
                CookError::StateLimit(block.id.clone())
            }
        })?;
        if states
            .len()
            .checked_add(count)
            .is_none_or(|len| len > usize::from(u16::MAX))
        {
            return Err(CookError::Limit);
        }
        for index in 0..count {
            let values = state_values(&schema.finite_fields, index);
            let profile = matching_profile(&block.state_overrides, &values);
            let collision = match profile.and_then(|item| item.collision_shape.clone()) {
                Some(shape) => Some(shape),
                None => block.collision_profile.shape.clone(),
            };
            if matches!(collision, Some(Shape::Model(_))) {
                return Err(CookError::VisualCollision(block.id.clone()));
            }
            let occlusion = profile
                .and_then(|item| item.occlusion)
                .unwrap_or(block.occlusion_profile.face_mask);
            let face_support = profile
                .and_then(|item| item.face_support)
                .unwrap_or(block.face_support);
            let materials = core::array::from_fn(|slot| {
                let face = Face::ALL[slot];
                let key = block
                    .render_profile
                    .face_materials
                    .resolve(face, &block.render_profile.material)
                    .clone();
                slots[&CookedMaterial {
                    key,
                    alpha: block.render_profile.alpha_mode,
                }]
            });
            let overlays = core::array::from_fn(|slot| {
                block
                    .render_profile
                    .face_overlays
                    .get(Face::ALL[slot])
                    .map(|overlay| {
                        slots[&CookedMaterial {
                            key: overlay.material.clone(),
                            alpha: overlay.alpha_mode,
                        }]
                    })
            });
            states.push(CookedState {
                block: block.id.clone(),
                index: index as u16,
                values,
                occlusion,
                face_support,
                collision,
                materials,
                overlays,
            });
        }
    }

    let mut images = images.to_vec();
    images.sort_by(|left, right| left.guid.cmp(&right.guid).then(left.key.cmp(&right.key)));
    let mut seen_guid = BTreeSet::new();
    let mut seen_key = BTreeSet::new();
    for image in &images {
        if !seen_guid.insert(image.guid) || !seen_key.insert(image.key.clone()) {
            return Err(CookError::DuplicateImage);
        }
    }

    let mut locales = locales.to_vec();
    locales.sort_by(|left, right| left.language.cmp(&right.language));
    let mut seen_language = BTreeSet::new();
    for locale in &locales {
        if !seen_language.insert(locale.language.clone()) {
            return Err(CookError::DuplicateLanguage(locale.language.clone()));
        }
    }

    let mut tables = CookedTables {
        content_authority_digest: content.authority_digest,
        content_presentation_digest: content.presentation_digest,
        authority_digest: Digest([0; 32]),
        presentation_digest: Digest([0; 32]),
        states,
        materials,
        images,
        locales,
    };
    validate_tables(&tables, CookLimits::default())?;
    tables.authority_digest = digest_authority(&tables);
    tables.presentation_digest = digest_presentation(&tables);
    Ok(tables)
}

fn check_digests(tables: &CookedTables) -> Result<(), CookError> {
    if tables.authority_digest != digest_authority(tables)
        || tables.presentation_digest != digest_presentation(tables)
    {
        return Err(CookError::DigestMismatch);
    }
    Ok(())
}

fn validate_tables(tables: &CookedTables, limits: CookLimits) -> Result<usize, CookError> {
    let invalid = |message: &str| CookError::Invalid(message.into());
    if tables.states.len() > usize::from(u16::MAX) || tables.materials.len() > usize::from(u16::MAX)
    {
        return Err(CookError::Limit);
    }
    let mut budget = Cursor {
        bytes: &[],
        at: 0,
        owned: 0,
        items: 0,
        limits,
    };
    budget.charge(
        tables.states.len(),
        tables.states.len() * std::mem::size_of::<CookedState>(),
    )?;
    budget.charge(
        tables.materials.len(),
        tables.materials.len() * std::mem::size_of::<CookedMaterial>(),
    )?;
    budget.charge(
        tables.images.len(),
        tables
            .images
            .len()
            .checked_mul(std::mem::size_of::<CookImage>())
            .ok_or(CookError::Limit)?,
    )?;
    budget.charge(
        tables.locales.len(),
        tables
            .locales
            .len()
            .checked_mul(std::mem::size_of::<CookedLocale>())
            .ok_or(CookError::Limit)?,
    )?;
    // Four digests, four table counts, magic and version.
    let mut encoded = 8usize + 4 + 4 * 32 + 4 * 4;
    for material in &tables.materials {
        check_string(&material.key.to_string(), &mut budget, &mut encoded)?;
        encoded = encoded.checked_add(1).ok_or(CookError::Limit)?;
    }
    if tables.materials.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(invalid("material slots must be unique and sorted"));
    }
    let mut previous: Option<&CookedState> = None;
    for state in &tables.states {
        if let Some(last) = previous
            && (last.block > state.block
                || (last.block == state.block
                    && (state.index != last.index.checked_add(1).ok_or(CookError::Limit)?
                        || state.values.len() != last.values.len()
                        || state.values <= last.values)))
        {
            return Err(invalid(
                "states must be sorted with consecutive indices and unique values",
            ));
        }
        if previous.is_none_or(|last| last.block != state.block) && state.index != 0 {
            return Err(invalid("a block's state indices must start at zero"));
        }
        if u64::from(state.index) >= MAX_FINITE_STATES {
            return Err(CookError::StateLimit(state.block.clone()));
        }
        previous = Some(state);
        if state.occlusion & !0x3f != 0 {
            return Err(invalid("occlusion mask exceeds six faces"));
        }
        state.face_support.validate().map_err(&invalid)?;
        if let Some(shape) = &state.collision {
            if matches!(shape, Shape::Model(_)) {
                return Err(CookError::VisualCollision(state.block.clone()));
            }
            shape.validate().map_err(&invalid)?;
            if let Shape::Compound(boxes) = shape {
                budget.charge(boxes.len(), boxes.len() * std::mem::size_of::<BoxBounds>())?;
            }
        }
        for slot in state
            .materials
            .into_iter()
            .chain(state.overlays.into_iter().flatten())
        {
            if slot as usize >= tables.materials.len() {
                return Err(invalid("material slot is out of range"));
            }
        }
        check_string(&state.block.to_string(), &mut budget, &mut encoded)?;
        budget.charge(
            state.values.len(),
            state.values.len().checked_mul(2).ok_or(CookError::Limit)?,
        )?;
        let mut shape = Vec::new();
        push_shape(&mut shape, state.collision.as_ref());
        let row_bytes = 2usize
            + 4
            + 1
            + 2
            + 24
            + 6
            + state.overlays.iter().filter(|slot| slot.is_some()).count() * 4;
        encoded = encoded
            .checked_add(row_bytes)
            .and_then(|n| n.checked_add(shape.len()))
            .and_then(|n| n.checked_add(state.values.len() * 2))
            .ok_or(CookError::Limit)?;
    }
    let mut keys = BTreeSet::new();
    let mut paths = BTreeSet::new();
    if tables
        .images
        .windows(2)
        .any(|pair| pair[0].guid >= pair[1].guid)
    {
        return Err(CookError::DuplicateImage);
    }
    for image in &tables.images {
        if !keys.insert(&image.key) || !paths.insert(&image.path) {
            return Err(CookError::DuplicateImage);
        }
        if !valid_asset_path(&image.path) {
            return Err(invalid("invalid image path alias"));
        }
        if !image.path.ends_with(".png") {
            return Err(invalid("only PNG image identities are supported"));
        }
        encoded = encoded.checked_add(48).ok_or(CookError::Limit)?;
        check_string(&image.key.to_string(), &mut budget, &mut encoded)?;
        check_string(&image.path, &mut budget, &mut encoded)?;
    }
    if tables
        .locales
        .windows(2)
        .any(|pair| pair[0].language >= pair[1].language)
    {
        return Err(invalid("locale languages must be unique and sorted"));
    }
    for locale in &tables.locales {
        if locale.language.is_empty()
            || locale.language.len() > 64
            || !locale.language.split('-').all(|part| {
                !part.is_empty()
                    && part.len() <= 8
                    && part.bytes().all(|b| b.is_ascii_alphanumeric())
            })
        {
            return Err(invalid("invalid locale language tag"));
        }
        check_string(&locale.language, &mut budget, &mut encoded)?;
        budget.charge(
            locale.entries.len(),
            locale
                .entries
                .len()
                .checked_mul(
                    std::mem::size_of::<(String, String)>() + 6 * std::mem::size_of::<usize>(),
                )
                .ok_or(CookError::Limit)?,
        )?;
        encoded = encoded.checked_add(4).ok_or(CookError::Limit)?;
        for (key, value) in &locale.entries {
            if key.trim().is_empty() || key.contains(['=', '\n', '\r']) {
                return Err(invalid("invalid locale message key"));
            }
            check_string(key, &mut budget, &mut encoded)?;
            check_string(value, &mut budget, &mut encoded)?;
        }
    }
    if encoded > limits.max_file_bytes {
        return Err(CookError::Limit);
    }
    Ok(budget.owned)
}

impl CookedTables {
    /// Conservative retained table storage, using the reader's own accounting.
    /// This validates canonical rows and checks their digests before reporting.
    pub fn accounted_bytes(&self) -> Result<usize, CookError> {
        let bytes = validate_tables(self, CookLimits::default())?;
        check_digests(self)?;
        Ok(bytes)
    }
}

fn check_string(
    value: &str,
    budget: &mut Cursor<'_>,
    encoded: &mut usize,
) -> Result<(), CookError> {
    if value.len() > usize::from(u16::MAX) {
        return Err(CookError::Limit);
    }
    budget.charge(0, value.len())?;
    *encoded = encoded
        .checked_add(2)
        .and_then(|n| n.checked_add(value.len()))
        .ok_or(CookError::Limit)?;
    Ok(())
}

/// Encode a table. The bytes are not an author format.
pub fn write_cooked(tables: &CookedTables) -> Result<Vec<u8>, CookError> {
    validate_tables(tables, CookLimits::default())?;
    check_digests(tables)?;
    Ok(encode_unchecked(tables))
}

fn encode_unchecked(tables: &CookedTables) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&tables.authority_digest.0);
    out.extend_from_slice(&tables.presentation_digest.0);
    out.extend_from_slice(&tables.content_authority_digest.0);
    out.extend_from_slice(&tables.content_presentation_digest.0);
    push_u32(&mut out, tables.states.len() as u32);
    for state in &tables.states {
        push_str(&mut out, &state.block.to_string());
        out.extend_from_slice(&state.index.to_le_bytes());
        push_u32(&mut out, state.values.len() as u32);
        for value in &state.values {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.push(state.occlusion);
        out.push(state.face_support.attachment);
        out.push(state.face_support.structural);
        push_shape(&mut out, state.collision.as_ref());
        for material in &state.materials {
            push_u32(&mut out, *material);
        }
        for overlay in &state.overlays {
            out.push(u8::from(overlay.is_some()));
            if let Some(slot) = overlay {
                push_u32(&mut out, *slot);
            }
        }
    }
    push_u32(&mut out, tables.materials.len() as u32);
    for material in &tables.materials {
        push_str(&mut out, &material.key.to_string());
        out.push(alpha_tag(material.alpha));
    }
    push_u32(&mut out, tables.images.len() as u32);
    for image in &tables.images {
        out.extend_from_slice(&image.guid.0.to_le_bytes());
        out.extend_from_slice(&image.source_digest.0);
        push_str(&mut out, &image.key.to_string());
        push_str(&mut out, &image.path);
    }
    push_u32(&mut out, tables.locales.len() as u32);
    for locale in &tables.locales {
        push_str(&mut out, &locale.language);
        push_u32(&mut out, locale.entries.len() as u32);
        for (key, value) in &locale.entries {
            push_str(&mut out, key);
            push_str(&mut out, value);
        }
    }
    out
}

/// Decode a table. A short or extended buffer produces no rows.
pub fn read_cooked(bytes: &[u8]) -> Result<CookedTables, CookError> {
    read_cooked_with_limits(bytes, CookLimits::default())
}

/// Read with explicit input, allocation, and aggregate item ceilings.
pub fn read_cooked_with_limits(
    bytes: &[u8],
    limits: CookLimits,
) -> Result<CookedTables, CookError> {
    if bytes.len() > limits.max_file_bytes {
        return Err(CookError::Limit);
    }
    let mut cursor = Cursor {
        bytes,
        at: 0,
        owned: 0,
        items: 0,
        limits,
    };
    let magic = cursor.bytes_exact(8)?;
    if magic != MAGIC {
        return Err(CookError::Header);
    }
    if cursor.u32()? != VERSION {
        return Err(CookError::Header);
    }
    let mut authority = [0u8; 32];
    authority.copy_from_slice(cursor.bytes_exact(32)?);
    let mut presentation = [0u8; 32];
    presentation.copy_from_slice(cursor.bytes_exact(32)?);
    let content_authority_digest = Digest(cursor.array()?);
    let content_presentation_digest = Digest(cursor.array()?);
    let state_count = cursor.count(45, std::mem::size_of::<CookedState>())?;
    if state_count > usize::from(u16::MAX) {
        return Err(CookError::Limit);
    }
    let mut states = Vec::with_capacity(state_count);
    for _ in 0..state_count {
        let block = ContentKey::parse(&cursor.string()?).map_err(|_| CookError::Header)?;
        let index = cursor.u16()?;
        let value_count = cursor.count(2, std::mem::size_of::<u16>())?;
        let mut values = Vec::with_capacity(value_count);
        for _ in 0..value_count {
            values.push(cursor.u16()?);
        }
        let occlusion = cursor.byte()?;
        let face_support = FaceSupport {
            attachment: cursor.byte()?,
            structural: cursor.byte()?,
        };
        face_support
            .validate()
            .map_err(|message| CookError::Invalid(message.into()))?;
        let collision = cursor.shape()?;
        let mut materials = [0; 6];
        for slot in &mut materials {
            *slot = cursor.u32()?;
        }
        let mut overlays = [None; 6];
        for slot in &mut overlays {
            *slot = match cursor.byte()? {
                0 => None,
                1 => Some(cursor.u32()?),
                _ => return Err(CookError::Header),
            };
        }
        states.push(CookedState {
            block,
            index,
            values,
            occlusion,
            face_support,
            collision,
            materials,
            overlays,
        });
    }
    let material_count = cursor.count(6, std::mem::size_of::<CookedMaterial>())?;
    if material_count > usize::from(u16::MAX) {
        return Err(CookError::Limit);
    }
    let mut materials = Vec::with_capacity(material_count);
    for _ in 0..material_count {
        let key = ContentKey::parse(&cursor.string()?).map_err(|_| CookError::Header)?;
        let alpha = match cursor.byte()? {
            0 => AlphaMode::Opaque,
            1 => AlphaMode::Mask,
            2 => AlphaMode::Blend,
            _ => return Err(CookError::Header),
        };
        materials.push(CookedMaterial { key, alpha });
    }
    let image_count = cursor.count(56, std::mem::size_of::<CookImage>())?;
    let mut images = Vec::with_capacity(image_count);
    for _ in 0..image_count {
        let guid = AssetGuid(u128::from_le_bytes(cursor.array()?));
        let source_digest = Digest(cursor.array()?);
        let key = ContentKey::parse(&cursor.string()?).map_err(|_| CookError::Header)?;
        let path = cursor.string()?;
        images.push(CookImage {
            guid,
            source_digest,
            key,
            path,
        });
    }
    let locale_count = cursor.count(7, std::mem::size_of::<CookedLocale>())?;
    let mut locales = Vec::with_capacity(locale_count);
    for _ in 0..locale_count {
        let language = cursor.string()?;
        let entry_count = cursor.count(
            4,
            std::mem::size_of::<(String, String)>() + 6 * std::mem::size_of::<usize>(),
        )?;
        let mut entries = BTreeMap::new();
        let mut previous = None;
        for _ in 0..entry_count {
            let key = cursor.string()?;
            if previous.as_ref().is_some_and(|last| last >= &key) {
                return Err(CookError::Invalid(
                    "locale keys must be unique and sorted".into(),
                ));
            }
            previous = Some(key.clone());
            let value = cursor.string()?;
            entries.insert(key, value);
        }
        locales.push(CookedLocale { language, entries });
    }
    if cursor.at != bytes.len() {
        return Err(CookError::Trailing);
    }
    let tables = CookedTables {
        content_authority_digest,
        content_presentation_digest,
        authority_digest: Digest(authority),
        presentation_digest: Digest(presentation),
        states,
        materials,
        images,
        locales,
    };
    validate_tables(&tables, limits)?;
    check_digests(&tables)?;
    Ok(tables)
}

fn state_count(fields: &[crate::descriptor::FiniteField]) -> Result<usize, ()> {
    if fields.is_empty() {
        return Ok(1);
    }
    let mut count = 1u64;
    for field in fields {
        if field.values.is_empty() {
            return Err(());
        }
        count = count.checked_mul(field.values.len() as u64).ok_or(())?;
        if count > MAX_FINITE_STATES {
            return Err(());
        }
    }
    Ok(count as usize)
}

fn state_values(fields: &[crate::descriptor::FiniteField], index: usize) -> Vec<u16> {
    if fields.is_empty() {
        return Vec::new();
    }
    let mut remainder = index;
    let mut row = vec![0u16; fields.len()];
    for (slot, field) in fields.iter().enumerate().rev() {
        let size = field.values.len();
        row[slot] = field.values[remainder % size];
        remainder /= size;
    }
    row
}

fn matching_profile<'a>(profiles: &'a [StateProfile], values: &[u16]) -> Option<&'a StateProfile> {
    profiles.iter().find(|profile| profile.values == values)
}

fn digest_authority(tables: &CookedTables) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(b"torse-cooked-authority-v3");
    hasher.update(tables.content_authority_digest.0);
    hasher.update((tables.states.len() as u64).to_le_bytes());
    for state in &tables.states {
        hash_str(&mut hasher, &state.block.to_string());
        hasher.update(state.index.to_le_bytes());
        hasher.update((state.values.len() as u64).to_le_bytes());
        for value in &state.values {
            hasher.update(value.to_le_bytes());
        }
        hasher.update([state.occlusion]);
        hasher.update([state.face_support.attachment, state.face_support.structural]);
        let mut encoded = Vec::new();
        push_shape(&mut encoded, state.collision.as_ref());
        hasher.update(encoded);
    }
    Digest(hasher.finalize().into())
}

fn digest_presentation(tables: &CookedTables) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(b"torse-cooked-presentation-v2");
    hasher.update(tables.content_presentation_digest.0);
    hasher.update((tables.states.len() as u64).to_le_bytes());
    for state in &tables.states {
        hash_str(&mut hasher, &state.block.to_string());
        hasher.update(state.index.to_le_bytes());
        for material in &state.materials {
            hasher.update(material.to_le_bytes());
        }
        for overlay in &state.overlays {
            hasher.update([u8::from(overlay.is_some())]);
            if let Some(slot) = overlay {
                hasher.update(slot.to_le_bytes());
            }
        }
    }
    hasher.update((tables.materials.len() as u64).to_le_bytes());
    for material in &tables.materials {
        hash_str(&mut hasher, &material.key.to_string());
        hasher.update([alpha_tag(material.alpha)]);
    }
    hasher.update((tables.images.len() as u64).to_le_bytes());
    for image in &tables.images {
        hasher.update(image.guid.0.to_le_bytes());
        hasher.update(image.source_digest.0);
        hash_str(&mut hasher, &image.key.to_string());
        hash_str(&mut hasher, &image.path);
    }
    hasher.update((tables.locales.len() as u64).to_le_bytes());
    for locale in &tables.locales {
        hash_str(&mut hasher, &locale.language);
        hasher.update((locale.entries.len() as u64).to_le_bytes());
        for (key, value) in &locale.entries {
            hash_str(&mut hasher, key);
            hash_str(&mut hasher, value);
        }
    }
    Digest(hasher.finalize().into())
}

fn hash_str(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn alpha_tag(alpha: AlphaMode) -> u8 {
    match alpha {
        AlphaMode::Opaque => 0,
        AlphaMode::Mask => 1,
        AlphaMode::Blend => 2,
    }
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_str(out: &mut Vec<u8>, value: &str) {
    let len = u16::try_from(value.len()).expect("validated cooked string");
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&value.as_bytes()[..usize::from(len)]);
}

fn push_box(out: &mut Vec<u8>, bounds: &BoxBounds) {
    for value in bounds.min.into_iter().chain(bounds.max) {
        out.extend_from_slice(&value.to_le_bytes());
    }
}

fn push_shape(out: &mut Vec<u8>, shape: Option<&Shape>) {
    match shape {
        None => out.push(0),
        Some(Shape::FullCube) => out.push(1),
        Some(Shape::LowerSlab) => out.push(2),
        Some(Shape::Panel) => out.push(3),
        Some(Shape::Cross) => out.push(4),
        Some(Shape::Corner) => out.push(5),
        Some(Shape::Stair) => out.push(6),
        Some(Shape::Box(bounds)) => {
            out.push(7);
            push_box(out, bounds);
        }
        Some(Shape::Compound(boxes)) => {
            out.push(8);
            let count = u16::try_from(boxes.len()).expect("validated compound box count");
            out.extend_from_slice(&count.to_le_bytes());
            for bounds in boxes.iter().take(usize::from(count)) {
                push_box(out, bounds);
            }
        }
        Some(Shape::Model(_)) => out.push(9),
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
    owned: usize,
    items: usize,
    limits: CookLimits,
}

impl<'a> Cursor<'a> {
    fn charge(&mut self, items: usize, bytes: usize) -> Result<(), CookError> {
        self.items = self.items.checked_add(items).ok_or(CookError::Limit)?;
        self.owned = self.owned.checked_add(bytes).ok_or(CookError::Limit)?;
        if self.items > self.limits.max_items || self.owned > self.limits.max_owned_bytes {
            return Err(CookError::Limit);
        }
        Ok(())
    }

    fn count(&mut self, minimum_wire: usize, owned_item: usize) -> Result<usize, CookError> {
        let count = self.u32()? as usize;
        self.charge(
            count,
            count.checked_mul(owned_item).ok_or(CookError::Limit)?,
        )?;
        if count > (self.bytes.len() - self.at) / minimum_wire {
            return Err(CookError::Truncated);
        }
        Ok(count)
    }
    fn bytes_exact(&mut self, len: usize) -> Result<&'a [u8], CookError> {
        let end = self.at.checked_add(len).ok_or(CookError::Truncated)?;
        let slice = self.bytes.get(self.at..end).ok_or(CookError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8, CookError> {
        Ok(self.bytes_exact(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, CookError> {
        let mut bytes = [0u8; 2];
        bytes.copy_from_slice(self.bytes_exact(2)?);
        Ok(u16::from_le_bytes(bytes))
    }

    fn u32(&mut self) -> Result<u32, CookError> {
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(self.bytes_exact(4)?);
        Ok(u32::from_le_bytes(bytes))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], CookError> {
        let mut bytes = [0u8; N];
        bytes.copy_from_slice(self.bytes_exact(N)?);
        Ok(bytes)
    }

    fn string(&mut self) -> Result<String, CookError> {
        let len = usize::from(self.u16()?);
        self.charge(0, len)?;
        let bytes = self.bytes_exact(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| CookError::Header)
    }

    fn bounds(&mut self) -> Result<BoxBounds, CookError> {
        let mut min = [0f32; 3];
        let mut max = [0f32; 3];
        for value in min.iter_mut().chain(max.iter_mut()) {
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(self.bytes_exact(4)?);
            *value = f32::from_le_bytes(bytes);
        }
        Ok(BoxBounds { min, max })
    }

    fn shape(&mut self) -> Result<Option<Shape>, CookError> {
        Ok(match self.byte()? {
            0 => None,
            1 => Some(Shape::FullCube),
            2 => Some(Shape::LowerSlab),
            3 => Some(Shape::Panel),
            4 => Some(Shape::Cross),
            5 => Some(Shape::Corner),
            6 => Some(Shape::Stair),
            7 => Some(Shape::Box(self.bounds()?)),
            8 => {
                let count = usize::from(self.u16()?);
                if count > crate::MAX_COMPOUND_BOXES {
                    return Err(CookError::Limit);
                }
                self.charge(count, count * std::mem::size_of::<BoxBounds>())?;
                if count > (self.bytes.len() - self.at) / 24 {
                    return Err(CookError::Truncated);
                }
                let mut boxes = Vec::with_capacity(count);
                for _ in 0..count {
                    boxes.push(self.bounds()?);
                }
                Some(Shape::Compound(boxes))
            }
            _ => return Err(CookError::Header),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{compile, m0_fixture_pack};

    #[test]
    fn empty_v3_file_has_fixed_bytes_and_older_versions_are_rejected() {
        // Fixed vector independent of the encoder and digest helpers.
        let authority = "083573c550968d2c75612d105439827ae0b482349c5d7ae80ca81bc4f97ca8b0";
        let presentation = "c26506fcffa61995ba5577d7d97ab9776149f0e9d7a342122102c63000d2b372";
        let mut bytes = b"TCOOK003\x03\x00\x00\x00".to_vec();
        for hex in [authority, presentation] {
            bytes.extend(
                (0..hex.len())
                    .step_by(2)
                    .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap()),
            );
        }
        bytes.extend([0; 80]); // Compiler identities and four zero counts.
        assert_eq!(bytes.len(), 156);
        let tables = read_cooked(&bytes).unwrap();
        assert!(tables.states.is_empty());
        assert_eq!(write_cooked(&tables).unwrap(), bytes);
        for version in [1, 2] {
            bytes[7] = b'0' + version;
            bytes[8] = version;
            assert_eq!(read_cooked(&bytes), Err(CookError::Header));
        }
    }

    fn image() -> CookImage {
        CookImage {
            guid: AssetGuid(0x0001_0000_0000_0001),
            source_digest: Digest([1; 32]),
            key: ContentKey::parse("base:texture/atlas").unwrap(),
            path: "textures/atlas.png".to_owned(),
        }
    }

    fn locales() -> Vec<CookedLocale> {
        let text = include_str!("../../../games/base/assets/locales/en.txt");
        vec![CookedLocale {
            language: "en".to_owned(),
            entries: parse_locale(text).unwrap(),
        }]
    }

    #[test]
    fn locale_text_changes_only_the_presentation_digest() {
        let content = compile(m0_fixture_pack()).unwrap();
        let original = cook(&content, &[image()], &locales()).unwrap();
        let mut changed = locales();
        changed[0]
            .entries
            .insert("extra".to_owned(), "value".to_owned());
        let presentation = cook(&content, &[image()], &changed).unwrap();
        assert_eq!(original.authority_digest, presentation.authority_digest);
        assert_ne!(
            original.presentation_digest,
            presentation.presentation_digest
        );
        assert!(
            original
                .states
                .iter()
                .any(|state| state.block.to_string() == "base:stone")
        );
        assert!(
            write_cooked(&original)
                .unwrap()
                .windows(8)
                .any(|window| window == MAGIC)
        );
        assert!(
            !write_cooked(&original)
                .unwrap()
                .windows(8)
                .any(|window| window == b"\x89PNG\r\n\x1a\n")
        );
    }

    #[test]
    fn occlusion_changes_only_the_authority_digest_and_bytes_roundtrip() {
        let mut pack = m0_fixture_pack();
        let before = cook(&compile(pack.clone()).unwrap(), &[image()], &[]).unwrap();
        pack.blocks[0].value.occlusion_profile.face_mask = 0;
        let after = cook(&compile(pack).unwrap(), &[image()], &[]).unwrap();
        assert_ne!(before.authority_digest, after.authority_digest);
        assert_eq!(before.presentation_digest, after.presentation_digest);
        let bytes = write_cooked(&before).unwrap();
        assert_eq!(read_cooked(&bytes).unwrap(), before);
        assert_eq!(
            read_cooked(&bytes[..bytes.len() - 1]),
            Err(CookError::Truncated)
        );
        let mut extra = bytes.clone();
        extra.push(0);
        assert_eq!(read_cooked(&extra), Err(CookError::Trailing));
        assert!(parse_locale("a=1\na=2").is_err());
    }

    #[test]
    fn support_roundtrips_in_expanded_rows_and_preserves_presentation_identity() {
        let mut pack = m0_fixture_pack();
        let before = cook(&compile(pack.clone()).unwrap(), &[], &[]).unwrap();
        let block = pack
            .blocks
            .iter_mut()
            .find(|block| !block.value.state_overrides.is_empty())
            .unwrap();
        let id = block.value.id.clone();
        block.value.face_support = FaceSupport {
            attachment: 8,
            structural: 1,
        };
        block.value.state_overrides[0].face_support = Some(FaceSupport::NONE);
        let tables = cook(&compile(pack).unwrap(), &[], &[]).unwrap();
        let rows: Vec<_> = tables.states.iter().filter(|row| row.block == id).collect();
        assert_eq!(rows[0].face_support, FaceSupport::NONE);
        assert_eq!(
            rows[1].face_support,
            FaceSupport {
                attachment: 8,
                structural: 1
            }
        );
        assert_ne!(before.authority_digest, tables.authority_digest);
        assert_eq!(before.presentation_digest, tables.presentation_digest);
        assert_eq!(
            read_cooked(&write_cooked(&tables).unwrap()).unwrap(),
            tables
        );
    }

    #[test]
    fn support_bits_are_validated_on_read_and_write_and_protected_by_the_digest() {
        let content = compile(m0_fixture_pack()).unwrap();
        let tables = cook(&content, &[], &[]).unwrap();
        let mut bad = tables.clone();
        bad.states[0].face_support.structural = 0x80;
        assert!(matches!(write_cooked(&bad), Err(CookError::Invalid(_))));
        // Deliberately bypass the writer to check untrusted decoding as well.
        assert!(matches!(
            read_cooked(&encode_unchecked(&bad)),
            Err(CookError::Invalid(_))
        ));
        bad.states[0].face_support.structural = 8;
        assert_eq!(write_cooked(&bad), Err(CookError::DigestMismatch));
        assert_eq!(
            read_cooked(&encode_unchecked(&bad)),
            Err(CookError::DigestMismatch)
        );
    }

    #[test]
    fn state_values_are_covered_by_the_digest() {
        let content = compile(m0_fixture_pack()).unwrap();
        let mut tables = cook(&content, &[image()], &[]).unwrap();
        let state = tables
            .states
            .iter_mut()
            .find(|state| !state.values.is_empty())
            .unwrap();
        state.values[0] ^= 1;
        assert!(write_cooked(&tables).is_err());
        assert!(read_cooked(&encode_unchecked(&tables)).is_err());
    }

    #[test]
    fn locale_boundaries_cannot_collide_in_the_presentation_digest() {
        let content = compile(m0_fixture_pack()).unwrap();
        let make = |language: &str| CookedLocale {
            language: language.into(),
            entries: BTreeMap::new(),
        };
        let first = cook(&content, &[], &[make("ab"), make("cdef")]).unwrap();
        let second = cook(&content, &[], &[make("abc"), make("def")]).unwrap();
        assert_ne!(first.presentation_digest, second.presentation_digest);
    }

    #[test]
    fn oversized_strings_and_ambiguous_image_paths_are_rejected() {
        let content = compile(m0_fixture_pack()).unwrap();
        let huge = CookedLocale {
            language: "en".into(),
            entries: [("message".into(), "x".repeat(65_536))].into(),
        };
        assert!(cook(&content, &[], &[huge]).is_err());
        let first = image();
        let mut second = first.clone();
        second.guid = AssetGuid(2);
        second.key = ContentKey::parse("base:another").unwrap();
        assert!(cook(&content, &[first, second], &[]).is_err());
        let mut invalid = image();
        invalid.path = "../outside.png".into();
        assert!(cook(&content, &[invalid], &[]).is_err());
    }

    #[test]
    fn hostile_counts_and_configured_budgets_fail_before_reservation() {
        let tables = cook(&compile(m0_fixture_pack()).unwrap(), &[image()], &locales()).unwrap();
        let bytes = write_cooked(&tables).unwrap();
        let mut hostile = bytes.clone();
        hostile[140..144].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(read_cooked(&hostile), Err(CookError::Limit));
        let key_len = usize::from(u16::from_le_bytes(bytes[144..146].try_into().unwrap()));
        let field_count = 146 + key_len + 2;
        hostile = bytes.clone();
        hostile[field_count..field_count + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(read_cooked(&hostile), Err(CookError::Limit));
        for limits in [
            CookLimits {
                max_file_bytes: bytes.len() - 1,
                ..CookLimits::default()
            },
            CookLimits {
                max_owned_bytes: 1,
                ..CookLimits::default()
            },
            CookLimits {
                max_items: 1,
                ..CookLimits::default()
            },
        ] {
            assert_eq!(
                read_cooked_with_limits(&bytes, limits),
                Err(CookError::Limit)
            );
        }
    }

    #[test]
    fn all_prefixes_and_single_bit_corruption_are_rejected() {
        let tables = cook(&compile(m0_fixture_pack()).unwrap(), &[image()], &locales()).unwrap();
        let bytes = write_cooked(&tables).unwrap();
        for end in 0..bytes.len() {
            assert!(read_cooked(&bytes[..end]).is_err(), "prefix {end}");
        }
        for at in 0..bytes.len() {
            let mut corrupt = bytes.clone();
            corrupt[at] ^= 1;
            assert!(read_cooked(&corrupt).is_err(), "corruption at {at}");
        }
    }

    #[test]
    fn correct_checksums_do_not_authorize_invalid_rows() {
        let original = cook(&compile(m0_fixture_pack()).unwrap(), &[image()], &[]).unwrap();
        let mut cases = Vec::new();
        let mut bad = original.clone();
        bad.states[0].occlusion = 0x80;
        cases.push(bad);
        let mut bad = original.clone();
        bad.states[0].index = 8;
        cases.push(bad);
        let mut bad = original.clone();
        bad.states[0].materials[0] = u32::MAX;
        cases.push(bad);
        let mut bad = original.clone();
        bad.states[0].collision = Some(Shape::Box(BoxBounds {
            min: [f32::NAN, 0.0, 0.0],
            max: [1.0; 3],
        }));
        cases.push(bad);
        let mut bad = original.clone();
        bad.states[0].collision = Some(Shape::Compound(vec![
            BoxBounds {
                min: [0.0; 3],
                max: [1.0; 3]
            };
            crate::MAX_COMPOUND_BOXES + 1
        ]));
        cases.push(bad);
        let mut bad = original.clone();
        bad.images[0].path = "../secret.png".into();
        cases.push(bad);
        for mut bad in cases {
            bad.authority_digest = digest_authority(&bad);
            bad.presentation_digest = digest_presentation(&bad);
            assert!(write_cooked(&bad).is_err());
            assert!(read_cooked(&encode_unchecked(&bad)).is_err());
        }
    }

    #[test]
    fn duplicate_serialized_locale_keys_are_not_silently_collapsed() {
        let locale = CookedLocale {
            language: "en".into(),
            entries: [("a".into(), "b".into())].into(),
        };
        let table = cook(&compile(m0_fixture_pack()).unwrap(), &[], &[locale]).unwrap();
        let mut bytes = write_cooked(&table).unwrap();
        let len = bytes.len();
        let entry = bytes[len - 6..].to_vec();
        bytes[len - 10..len - 6].copy_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&entry);
        assert!(read_cooked(&bytes).is_err());
    }

    #[test]
    fn png_changes_and_compiler_behavior_move_only_their_own_digest() {
        let mut pack = m0_fixture_pack();
        let original = cook(&compile(pack.clone()).unwrap(), &[image()], &[]).unwrap();
        let mut changed_image = image();
        changed_image.source_digest.0[0] ^= 1;
        let changed = cook(&compile(pack.clone()).unwrap(), &[changed_image], &[]).unwrap();
        assert_eq!(original.authority_digest, changed.authority_digest);
        assert_ne!(original.presentation_digest, changed.presentation_digest);
        pack.blocks[0].value.light_transport.emission = 0.25;
        let changed = cook(&compile(pack).unwrap(), &[image()], &[]).unwrap();
        assert_ne!(original.authority_digest, changed.authority_digest);
        assert_eq!(original.presentation_digest, changed.presentation_digest);
    }

    #[test]
    fn slots_are_dense_sorted_alpha_specific_and_overlays_survive() {
        let mut pack = m0_fixture_pack();
        pack.blocks[0].value.render_profile.face_overlays.set(
            Face::PosY,
            crate::FaceOverlay {
                material: ContentKey::parse("base:material/overlay").unwrap(),
                alpha_mode: AlphaMode::Mask,
            },
        );
        let content = compile(pack).unwrap();
        let tables = cook(&content, &[image()], &[]).unwrap();
        for row in &tables.states {
            let profile = &content.blocks[&row.block].render_profile;
            for face in Face::ALL {
                let material = &tables.materials[row.materials[face.index()] as usize];
                assert_eq!(
                    &material.key,
                    profile.face_materials.resolve(face, &profile.material)
                );
                assert_eq!(material.alpha, profile.alpha_mode);
                assert_eq!(
                    row.overlays[face.index()].map(|slot| &tables.materials[slot as usize].key),
                    profile
                        .face_overlays
                        .get(face)
                        .map(|overlay| &overlay.material)
                );
            }
        }
        let mut images = vec![image()];
        let mut second = image();
        second.guid = AssetGuid(2);
        second.key = ContentKey::parse("base:icon/x").unwrap();
        second.path = "icons/x.png".into();
        images.push(second);
        let first = cook(&content, &images, &locales()).unwrap();
        images.reverse();
        let second = cook(&content, &images, &locales()).unwrap();
        assert_eq!(
            write_cooked(&first).unwrap(),
            write_cooked(&second).unwrap()
        );
    }
}
