//! Package format, loading, resolution, and locking (P3 toolchain).
//!
//! A package is a directory with `mod.toml`, optional `content/*.toml` data
//! files, and an optional behavior entry. Loading validates the manifest,
//! enforces that every declared content key lives in the package namespace,
//! resolves dependencies across the loaded set, and produces a compiled
//! content registry plus a lock entry binding exact versions and file hashes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use torse_content::{
    AlphaMode, BlockDefinition, CollisionProfile, ComponentField, ComponentFieldType,
    ComponentSchema as ContentComponentSchema, ContentPack, Face, FaceMaterials, FaceOverlay,
    FaceOverlays, FiniteField, InteractionProfile, LightTransport, ModuleDescriptor, ModuleSide,
    OcclusionProfile, PersistencePolicy, RenderProfile, RequestedPermission, SelectionProfile,
    Shape, SourceRef, Sourced, StateSchema, compile,
};
use torse_types::{ContentKey, ReplicationDeclaration};
use torse_world::{BlockStateRegistry, ComponentSchemas};

/// The behavior API version this host implements; manifests must match.
pub const SUPPORTED_API: (u32, u32) = (0, 3);

/// A semantic version restricted to `major.minor.patch`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut parts = text.split('.');
        let mut next = || {
            parts
                .next()
                .and_then(|part| part.parse::<u32>().ok())
                .ok_or_else(|| format!("invalid version `{text}`"))
        };
        let major = next()?;
        let minor = next()?;
        let patch = next()?;
        if parts.next().is_some() {
            return Err(format!("invalid version `{text}`"));
        }
        Ok(Self {
            major,
            minor,
            patch,
        })
    }

    pub fn as_str(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

/// A dependency version requirement: `0.1` accepts any compatible minor,
/// `0.1.0` pins an exact version.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VersionReq {
    MinorCompatible { major: u32, minor: u32 },
    Exact(Version),
}

impl VersionReq {
    pub fn parse(text: &str) -> Result<Self, String> {
        let dots = text.matches('.').count();
        match dots {
            1 => {
                let mut parts = text.split('.');
                let major = parts
                    .next()
                    .and_then(|part| part.parse::<u32>().ok())
                    .ok_or_else(|| format!("invalid requirement `{text}`"))?;
                let minor = parts
                    .next()
                    .and_then(|part| part.parse::<u32>().ok())
                    .ok_or_else(|| format!("invalid requirement `{text}`"))?;
                Ok(Self::MinorCompatible { major, minor })
            }
            2 => Ok(Self::Exact(Version::parse(text)?)),
            _ => Err(format!("invalid requirement `{text}`")),
        }
    }

    pub fn accepts(self, version: Version) -> bool {
        match self {
            Self::MinorCompatible { major, minor } => {
                version.major == major && version.minor == minor
            }
            Self::Exact(exact) => version == exact,
        }
    }
}

/// Which sides a package ships content or code for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    Server,
    Client,
    Both,
}

impl Side {
    fn parse(text: &str) -> Result<Self, String> {
        match text {
            "server" => Ok(Self::Server),
            "client" => Ok(Self::Client),
            "both" => Ok(Self::Both),
            _ => Err(format!("unknown side `{text}`")),
        }
    }

    pub fn includes_server(self) -> bool {
        matches!(self, Self::Server | Self::Both)
    }
}

/// The parsed `mod.toml`.
#[derive(Clone, Debug)]
pub struct Manifest {
    pub id: String,
    pub version: Version,
    pub api: (u32, u32),
    pub side: Side,
    pub permissions: Vec<ContentKey>,
    pub server_entry: Option<String>,
    pub client_entry: Option<String>,
    pub build_crate: Option<String>,
    pub build_workspace: Option<String>,
    pub dependencies: Vec<(String, VersionReq)>,
}

/// A load or validation failure, located at a package and file.
#[derive(Clone, Debug)]
pub struct PackageError {
    pub package: String,
    pub path: String,
    pub detail: String,
}

impl PackageError {
    fn new(package: &str, path: &str, detail: impl Into<String>) -> Self {
        Self {
            package: package.to_string(),
            path: path.to_string(),
            detail: detail.into(),
        }
    }
}

impl fmt::Display for PackageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}:{}] {}", self.package, self.path, self.detail)
    }
}

fn table<'a>(value: &'a toml::Value, key: &str) -> Option<&'a toml::map::Map<String, toml::Value>> {
    value.get(key).and_then(|entry| entry.as_table())
}

fn str_field(
    entry: &toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<Option<String>, String> {
    match entry.get(key) {
        None => Ok(None),
        Some(toml::Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("field `{key}` must be a string")),
    }
}

fn parse_key(text: &str) -> Result<ContentKey, String> {
    ContentKey::parse(text).map_err(|error| format!("invalid content key `{text}`: {error}"))
}

impl Manifest {
    /// Parses and validates a manifest from its TOML text.
    pub fn parse(text: &str) -> Result<Self, Vec<PackageError>> {
        let mut errors = Vec::new();
        let value: toml::Value = toml::from_str(text).map_err(|error| {
            vec![PackageError::new(
                "?",
                "mod.toml",
                format!("toml parse failed: {error}"),
            )]
        })?;

        let id = str_field(value.as_table().unwrap_or(&toml::map::Map::new()), "id")
            .map_err(|detail| errors.push(PackageError::new("?", "mod.toml", detail)))
            .ok()
            .flatten();
        let Some(id) = id else {
            errors.push(PackageError::new("?", "mod.toml", "field `id` is required"));
            return Err(errors);
        };
        let fail = |errors: &mut Vec<PackageError>, detail: String| {
            errors.push(PackageError::new(&id, "mod.toml", detail));
        };

        let root = value.as_table().unwrap();
        let version_text = match str_field(root, "version") {
            Ok(Some(text)) => text,
            Ok(None) => {
                fail(&mut errors, "field `version` is required".to_string());
                String::new()
            }
            Err(detail) => {
                fail(&mut errors, detail);
                String::new()
            }
        };
        let version = Version::parse(&version_text).ok();

        let api = str_field(root, "api").ok().flatten().and_then(|text| {
            let mut parts = text.split('.');
            let major = parts.next()?.parse::<u32>().ok()?;
            let minor = parts.next()?.parse::<u32>().ok()?;
            parts.next().is_none().then_some((major, minor))
        });

        let side = str_field(root, "side")
            .ok()
            .flatten()
            .and_then(|text| Side::parse(&text).ok());

        let permission_texts: Vec<String> = value
            .get("permissions")
            .and_then(|entry| entry.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let mut permissions = Vec::new();
        for text in &permission_texts {
            match parse_key(text) {
                Ok(key) => permissions.push(key),
                Err(detail) => {
                    errors.push(PackageError::new(&id, "mod.toml", detail));
                }
            }
        }

        let server_entry =
            table(&value, "server").and_then(|section| str_field(section, "entry").ok().flatten());
        let client_entry =
            table(&value, "client").and_then(|section| str_field(section, "entry").ok().flatten());
        let build = table(&value, "build");
        let build_crate = build.and_then(|section| str_field(section, "crate").ok().flatten());
        let build_workspace =
            build.and_then(|section| str_field(section, "workspace").ok().flatten());

        let dependencies = value
            .get("dependencies")
            .and_then(|entry| entry.as_table())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|(dep, req)| {
                        let req = req.as_str()?;
                        Some((
                            dep.clone(),
                            VersionReq::parse(req).map_err(|detail| {
                                PackageError::new(
                                    &id,
                                    "mod.toml",
                                    format!("dependency `{dep}`: {detail}"),
                                )
                            }),
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (dep, result) in &dependencies {
            let _ = dep;
            if let Err(error) = result {
                errors.push(error.clone());
            }
        }

        if let Some(api) = api {
            if api != SUPPORTED_API {
                fail(
                    &mut errors,
                    format!(
                        "api {api:?} is not supported by this host (expected {SUPPORTED_API:?})"
                    ),
                );
            }
        } else {
            fail(&mut errors, "field `api` must be `major.minor`".to_string());
        }
        if side.is_none() {
            fail(
                &mut errors,
                "field `side` must be server, client, or both".to_string(),
            );
        }
        if version.is_none() {
            fail(
                &mut errors,
                "field `version` must be major.minor.patch".to_string(),
            );
        }
        // Validate the id as a content namespace.
        if parse_key(&format!("{id}:probe")).is_err() {
            fail(
                &mut errors,
                format!("id `{id}` is not a valid lowercase content namespace"),
            );
        }
        let (Some(version), Some(side)) = (version, side) else {
            return Err(errors);
        };
        if !errors.is_empty() {
            return Err(errors);
        }

        Ok(Self {
            id,
            version,
            api: SUPPORTED_API,
            side,
            permissions,
            server_entry,
            client_entry,
            build_crate,
            build_workspace,
            dependencies: dependencies
                .into_iter()
                .filter_map(|(dep, result)| result.ok().map(|req| (dep, req)))
                .collect(),
        })
    }

    /// The module principal under which this package commits transactions.
    pub fn principal(&self) -> ContentKey {
        parse_key(&format!("{}:module", self.id)).expect("validated namespace")
    }
}

/// One loaded package directory with its hashed file list.
#[derive(Clone, Debug)]
pub struct LoadedPackage {
    pub manifest: Manifest,
    pub dir: PathBuf,
    /// Relative path to hex sha256, sorted by path.
    pub files: Vec<(String, String)>,
    /// Merkle digest over the sorted file list.
    pub merkle: String,
    /// Component schema ids declared by this package, in declaration order.
    pub owned_schemas: Vec<ContentKey>,
}

impl LoadedPackage {
    /// Reads and validates one package directory.
    pub fn load(dir: &Path) -> Result<Self, Vec<PackageError>> {
        let manifest_path = dir.join("mod.toml");
        let text = std::fs::read_to_string(&manifest_path).map_err(|error| {
            vec![PackageError::new(
                "?",
                &manifest_path.display().to_string(),
                format!("manifest unreadable: {error}"),
            )]
        })?;
        let manifest = Manifest::parse(&text)?;
        let mut errors = Vec::new();

        let mut files = Vec::new();
        hash_dir(dir, dir, &mut files, &mut errors, &manifest.id);
        files.sort();
        let mut hasher = Sha256::new();
        for (path, hash) in &files {
            hasher.update(path.as_bytes());
            hasher.update(hash.as_bytes());
        }
        let merkle = hex(&hasher.finalize());

        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(Self {
            manifest,
            dir: dir.to_path_buf(),
            files,
            merkle,
            owned_schemas: Vec::new(),
        })
    }
}

fn hash_dir(
    root: &Path,
    dir: &Path,
    files: &mut Vec<(String, String)>,
    errors: &mut Vec<PackageError>,
    package: &str,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            hash_dir(root, &path, files, errors, package);
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        match std::fs::read(&path) {
            Ok(bytes) => files.push((
                relative.to_string_lossy().replace('\\', "/"),
                hex(&Sha256::digest(&bytes)),
            )),
            Err(error) => errors.push(PackageError::new(
                package,
                &relative.display().to_string(),
                format!("unreadable: {error}"),
            )),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Content declarations parsed from one content file; `assemble` turns them
/// into pack entries, resolving component references into state schemas.
struct ContentContributions {
    schemas: Vec<ContentComponentSchema>,
    states: Vec<ParsedState>,
    blocks: Vec<BlockDefinition>,
}

/// A `[[state]]` entry before its component references are resolved.
struct ParsedState {
    schema: StateSchema,
    component_refs: Vec<ContentKey>,
}

fn parse_content_value(
    package: &str,
    value: &toml::Value,
    errors: &mut Vec<PackageError>,
    owned_schemas: &mut Vec<ContentKey>,
) -> ContentContributions {
    let mut schemas = Vec::new();
    let mut states = Vec::new();
    let mut blocks = Vec::new();

    for component in array(value, "component", package, errors) {
        match parse_component(package, component) {
            Ok(schema) => {
                owned_schemas.push(schema.id.clone());
                schemas.push(schema);
            }
            Err(mut component_errors) => errors.append(&mut component_errors),
        }
    }
    for state in array(value, "state", package, errors) {
        match parse_state(package, state) {
            Ok(state) => states.push(state),
            Err(mut state_errors) => errors.append(&mut state_errors),
        }
    }
    for block in array(value, "block", package, errors) {
        match parse_block(package, block) {
            Ok(block) => blocks.push(block),
            Err(mut block_errors) => errors.append(&mut block_errors),
        }
    }
    ContentContributions {
        schemas,
        states,
        blocks,
    }
}

fn array<'a>(
    value: &'a toml::Value,
    key: &str,
    package: &str,
    errors: &mut Vec<PackageError>,
) -> Vec<&'a toml::map::Map<String, toml::Value>> {
    match value.get(key).map(toml::Value::as_array) {
        None | Some(None) => Vec::new(),
        Some(Some(entries)) => entries
            .iter()
            .filter_map(|entry| match entry.as_table() {
                Some(table) => Some(table),
                None => {
                    errors.push(PackageError::new(
                        package,
                        "content",
                        format!("`{key}` entries must be tables"),
                    ));
                    None
                }
            })
            .collect(),
    }
}

fn parse_component(
    package: &str,
    entry: &toml::map::Map<String, toml::Value>,
) -> Result<ContentComponentSchema, Vec<PackageError>> {
    let mut errors = Vec::new();
    let id = required_key(package, entry, "id", &mut errors, parse_key);
    let version = optional_u32(entry, "version", &mut errors, package).unwrap_or(1);
    let owner_text = str_field(entry, "owner").ok().flatten();
    let owner = match owner_text {
        Some(text) => match parse_key(&text) {
            Ok(key) => key,
            Err(detail) => {
                errors.push(PackageError::new(package, "component", detail));
                parse_key(&format!("{package}:module")).expect("validated namespace")
            }
        },
        None => parse_key(&format!("{package}:module")).expect("validated namespace"),
    };
    let persistence = match str_field(entry, "persistence").ok().flatten().as_deref() {
        None | Some("always") => PersistencePolicy::Always,
        Some("never") => PersistencePolicy::Never,
        Some("owner-defined") => PersistencePolicy::OwnerDefined,
        Some(other) => {
            errors.push(PackageError::new(
                package,
                "component",
                format!("unknown persistence `{other}`"),
            ));
            PersistencePolicy::Always
        }
    };
    let replication = match str_field(entry, "replication").ok().flatten().as_deref() {
        Some("server-authorized") => ReplicationDeclaration {
            visibility: torse_types::ReplicationVisibility::Authorized,
            ..ReplicationDeclaration::SERVER_PRIVATE
        },
        None | Some("server-private") => ReplicationDeclaration::SERVER_PRIVATE,
        Some(other) => {
            errors.push(PackageError::new(
                package,
                "component",
                format!("unknown replication `{other}`"),
            ));
            ReplicationDeclaration::SERVER_PRIVATE
        }
    };

    let mut fields = Vec::new();
    if let Some(field_entries) = entry.get("field").and_then(toml::Value::as_array) {
        for field in field_entries.iter().filter_map(toml::Value::as_table) {
            let id = required_key(package, field, "id", &mut errors, parse_key);
            let type_text = str_field(field, "type").ok().flatten().unwrap_or_default();
            let field_type = match type_text.as_str() {
                "bool" => Some(ComponentFieldType::Bool),
                "u32" => Some(ComponentFieldType::Unsigned32),
                "i64" => Some(ComponentFieldType::Signed64),
                "content" => Some(ComponentFieldType::ContentReference),
                "entity" => Some(ComponentFieldType::EntityReference),
                "text" => Some(ComponentFieldType::String {
                    max_bytes: optional_u32(field, "max-bytes", &mut errors, package).unwrap_or(64),
                }),
                "list" => Some(ComponentFieldType::List {
                    max_items: optional_u32(field, "max-items", &mut errors, package).unwrap_or(64),
                }),
                "record" => Some(ComponentFieldType::Record),
                other => {
                    errors.push(PackageError::new(
                        package,
                        "component.field",
                        format!("unknown field type `{other}`"),
                    ));
                    None
                }
            };
            let required = field
                .get("required")
                .and_then(toml::Value::as_bool)
                .unwrap_or(true);
            if let (Some(id), Some(field_type)) = (id, field_type) {
                fields.push(ComponentField {
                    id,
                    field_type,
                    required,
                });
            }
        }
    }
    let Some(id) = id else {
        return Err(errors);
    };
    if id.namespace() != package {
        errors.push(PackageError::new(
            package,
            "component",
            format!("id `{id}` is outside the package namespace `{package}`"),
        ));
    }
    if errors.is_empty() {
        Ok(ContentComponentSchema {
            id,
            schema_version: version,
            owner_module: owner,
            fields,
            persistence,
            replication,
            migration_binding: None,
        })
    } else {
        Err(errors)
    }
}

fn parse_state(
    package: &str,
    entry: &toml::map::Map<String, toml::Value>,
) -> Result<ParsedState, Vec<PackageError>> {
    let mut errors = Vec::new();
    let id = required_key(package, entry, "id", &mut errors, parse_key);
    let version = optional_u32(entry, "version", &mut errors, package).unwrap_or(1);

    let mut finite_fields = Vec::new();
    if let Some(field_entries) = entry.get("field").and_then(toml::Value::as_array) {
        for field in field_entries.iter().filter_map(toml::Value::as_table) {
            let field_id = required_key(package, field, "id", &mut errors, parse_key);
            let default = optional_u32(field, "default", &mut errors, package).unwrap_or(0) as u16;
            let values = field
                .get("values")
                .and_then(toml::Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(toml::Value::as_integer)
                        .map(|value| value as u16)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if let Some(field_id) = field_id {
                if field_id.namespace() != package {
                    errors.push(PackageError::new(
                        package,
                        "state.field",
                        format!("id `{field_id}` is outside the package namespace"),
                    ));
                }
                finite_fields.push(FiniteField {
                    id: field_id,
                    default_value: default,
                    values,
                });
            }
        }
    }

    let mut component_refs = Vec::new();
    if let Some(component_entries) = entry.get("component").and_then(toml::Value::as_array) {
        for component in component_entries.iter().filter_map(toml::Value::as_table) {
            if let Some(text) = str_field(component, "id").ok().flatten() {
                match parse_key(&text) {
                    Ok(key) => component_refs.push(key),
                    Err(detail) => {
                        errors.push(PackageError::new(package, "state.component", detail));
                    }
                }
            }
        }
    }
    let Some(id) = id else {
        return Err(errors);
    };
    if id.namespace() != package {
        errors.push(PackageError::new(
            package,
            "state",
            format!("id `{id}` is outside the package namespace `{package}`"),
        ));
    }
    if errors.is_empty() {
        Ok(ParsedState {
            schema: StateSchema {
                id,
                schema_version: version,
                finite_fields,
                component_schemas: Vec::new(),
            },
            component_refs,
        })
    } else {
        Err(errors)
    }
}

fn parse_shape(text: &str) -> Option<Shape> {
    match text {
        "full-cube" => Some(Shape::FullCube),
        "lower-slab" => Some(Shape::LowerSlab),
        "panel" => Some(Shape::Panel),
        "cross" => Some(Shape::Cross),
        "corner" => Some(Shape::Corner),
        "stair" => Some(Shape::Stair),
        _ => None,
    }
}

fn parse_block(
    package: &str,
    entry: &toml::map::Map<String, toml::Value>,
) -> Result<BlockDefinition, Vec<PackageError>> {
    let mut errors = Vec::new();
    let id = required_key(package, entry, "id", &mut errors, parse_key);
    let version = optional_u32(entry, "version", &mut errors, package).unwrap_or(1);
    let schema_text = required_key(package, entry, "schema", &mut errors, |text| {
        parse_key(text)
    });
    let render = entry.get("render").and_then(toml::Value::as_table);
    let shape_text = render
        .and_then(|render| str_field(render, "shape").ok().flatten())
        .unwrap_or_else(|| "full-cube".to_string());
    let shape = parse_shape(&shape_text).unwrap_or_else(|| {
        errors.push(PackageError::new(
            package,
            "block.render",
            format!("unknown shape `{shape_text}`"),
        ));
        Shape::FullCube
    });
    let material = render
        .and_then(|render| str_field(render, "material").ok().flatten())
        .map(|text| match parse_key(&text) {
            Ok(key) => key,
            Err(detail) => {
                errors.push(PackageError::new(package, "block.render", detail));
                parse_key("base:material/default").expect("fixture material")
            }
        })
        .unwrap_or_else(|| parse_key("base:material/default").expect("fixture material"));
    let alpha = match render
        .and_then(|render| str_field(render, "alpha").ok().flatten())
        .as_deref()
    {
        None | Some("opaque") => AlphaMode::Opaque,
        Some("mask") => AlphaMode::Mask,
        Some("blend") => AlphaMode::Blend,
        Some(other) => {
            errors.push(PackageError::new(
                package,
                "block.render",
                format!("unknown alpha mode `{other}`"),
            ));
            AlphaMode::Opaque
        }
    };
    let mut face_overlays = FaceOverlays::none();
    if let Some(overlays) = render.and_then(|render| render.get("overlays")) {
        let Some(overlays) = overlays.as_table() else {
            errors.push(PackageError::new(
                package,
                "block.render.overlays",
                "expected a table keyed by face name",
            ));
            return Err(errors);
        };
        for (face_name, value) in overlays {
            let Some(face) = parse_face(face_name) else {
                errors.push(PackageError::new(
                    package,
                    "block.render.overlays",
                    format!("unknown face `{face_name}`"),
                ));
                continue;
            };
            let Some(value) = value.as_table() else {
                errors.push(PackageError::new(
                    package,
                    "block.render.overlays",
                    format!("`{face_name}` must be a table with material and alpha fields"),
                ));
                continue;
            };
            let Some(material_text) = value.get("material").and_then(toml::Value::as_str) else {
                errors.push(PackageError::new(
                    package,
                    "block.render.overlays",
                    format!("`{face_name}.material` must be a material key"),
                ));
                continue;
            };
            let material = match parse_key(material_text) {
                Ok(key) => key,
                Err(detail) => {
                    errors.push(PackageError::new(
                        package,
                        "block.render.overlays",
                        format!("`{face_name}.material`: {detail}"),
                    ));
                    continue;
                }
            };
            let alpha_value = value.get("alpha");
            if alpha_value.is_some_and(|alpha| alpha.as_str().is_none()) {
                errors.push(PackageError::new(
                    package,
                    "block.render.overlays",
                    format!("`{face_name}.alpha` must be a string"),
                ));
                continue;
            }
            let alpha = match alpha_value.and_then(toml::Value::as_str).unwrap_or("mask") {
                "opaque" => AlphaMode::Opaque,
                "mask" => AlphaMode::Mask,
                "blend" => AlphaMode::Blend,
                other => {
                    errors.push(PackageError::new(
                        package,
                        "block.render.overlays",
                        format!("`{face_name}.alpha` has unknown alpha mode `{other}`"),
                    ));
                    continue;
                }
            };
            face_overlays.set(
                face,
                FaceOverlay {
                    material,
                    alpha_mode: alpha,
                },
            );
        }
    }
    let mut optional_shape = |section: &str| {
        entry
            .get(section)
            .and_then(toml::Value::as_table)
            .and_then(|table| str_field(table, "shape").ok().flatten())
            .and_then(|text| match parse_shape(&text) {
                Some(shape) => Some(shape),
                None => {
                    errors.push(PackageError::new(
                        package,
                        "block",
                        format!("unknown {section} shape `{text}`"),
                    ));
                    None
                }
            })
    };
    let collision = optional_shape("collision");
    let selection = optional_shape("selection");
    let face_mask = entry
        .get("occlusion")
        .and_then(toml::Value::as_table)
        .and_then(|table| table.get("face-mask"))
        .and_then(toml::Value::as_integer)
        .map(|mask| mask as u8)
        .unwrap_or(if matches!(shape, Shape::FullCube) {
            63
        } else {
            0
        });
    let light = entry.get("light").and_then(toml::Value::as_table);
    let absorption = light
        .and_then(|light| light.get("absorption"))
        .and_then(toml::Value::as_float)
        .unwrap_or(0.0) as f32;
    let emission = light
        .and_then(|light| light.get("emission"))
        .and_then(toml::Value::as_float)
        .unwrap_or(0.0) as f32;
    let interaction = entry
        .get("interaction")
        .and_then(toml::Value::as_table)
        .and_then(|table| str_field(table, "action").ok().flatten())
        .and_then(|text| parse_key(&text).ok());
    let tags = entry
        .get("tags")
        .and_then(toml::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(toml::Value::as_str)
                .filter_map(|text| parse_key(text).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let Some(id) = id else {
        return Err(errors);
    };
    let Some(schema) = schema_text else {
        return Err(errors);
    };
    if entry.get("behavior").is_some_and(|value| !value.is_bool()) {
        errors.push(PackageError::new(
            package,
            "block.behavior",
            "expected a boolean",
        ));
    }
    if id.namespace() != package {
        errors.push(PackageError::new(
            package,
            "block",
            format!("id `{id}` is outside the package namespace `{package}`"),
        ));
    }
    if errors.is_empty() {
        Ok(BlockDefinition {
            id,
            definition_version: version,
            state_schema: schema,
            render_profile: RenderProfile {
                shape,
                material,
                face_materials: FaceMaterials::none(),
                face_overlays,
                alpha_mode: alpha,
            },
            collision_profile: CollisionProfile { shape: collision },
            selection_profile: SelectionProfile { shape: selection },
            occlusion_profile: OcclusionProfile { face_mask },
            face_support: torse_content::FaceSupport::NONE,
            light_transport: LightTransport {
                absorption,
                emission,
            },
            interaction_profile: InteractionProfile {
                action: interaction,
                completes_to: None,
            },
            component_defaults: Vec::new(),
            capability_bindings: Vec::new(),
            state_overrides: Vec::new(),
            behavior_bindings: if entry.get("behavior").and_then(toml::Value::as_bool) == Some(true)
            {
                vec![parse_key(&format!("{package}:rule/behavior")).expect("package key")]
            } else {
                Vec::new()
            },
            tags,
        })
    } else {
        Err(errors)
    }
}

fn parse_face(text: &str) -> Option<Face> {
    match text {
        "neg-x" => Some(Face::NegX),
        "pos-x" => Some(Face::PosX),
        "neg-y" => Some(Face::NegY),
        "pos-y" => Some(Face::PosY),
        "neg-z" => Some(Face::NegZ),
        "pos-z" => Some(Face::PosZ),
        _ => None,
    }
}

fn required_key<T>(
    package: &str,
    entry: &toml::map::Map<String, toml::Value>,
    key: &str,
    errors: &mut Vec<PackageError>,
    parse: fn(&str) -> Result<T, String>,
) -> Option<T> {
    match str_field(entry, key) {
        Ok(Some(text)) => match parse(&text) {
            Ok(value) => Some(value),
            Err(detail) => {
                errors.push(PackageError::new(package, "content", detail));
                None
            }
        },
        Ok(None) => {
            errors.push(PackageError::new(
                package,
                "content",
                format!("field `{key}` is required"),
            ));
            None
        }
        Err(detail) => {
            errors.push(PackageError::new(package, "content", detail));
            None
        }
    }
}

fn optional_u32(
    entry: &toml::map::Map<String, toml::Value>,
    key: &str,
    errors: &mut Vec<PackageError>,
    package: &str,
) -> Option<u32> {
    match entry.get(key) {
        None => None,
        Some(toml::Value::Integer(value)) if (0..=u32::MAX as i64).contains(value) => {
            Some(*value as u32)
        }
        Some(_) => {
            errors.push(PackageError::new(
                package,
                "content",
                format!("field `{key}` must be an integer"),
            ));
            None
        }
    }
}

/// A fully resolved, compiled set of packages.
#[derive(Debug)]
pub struct ResolvedModSet {
    /// Packages in dependency order, dependencies first.
    pub packages: Vec<LoadedPackage>,
    /// Component schemas by owning package principal.
    pub owned_schemas: BTreeMap<ContentKey, Vec<ContentKey>>,
    /// The compiled content registry for the whole set plus engine base.
    pub content: torse_content::CompiledContent,
}

/// Loads, resolves, and compiles a set of package directories on top of an
/// engine base pack. All failures are reported together.
pub fn resolve(
    dirs: &[PathBuf],
    engine_base: &ContentPack,
) -> Result<ResolvedModSet, Vec<PackageError>> {
    let mut errors = Vec::new();
    let mut by_id: BTreeMap<String, LoadedPackage> = BTreeMap::new();
    for dir in dirs {
        match LoadedPackage::load(dir) {
            Ok(package) => {
                if by_id.insert(package.manifest.id.clone(), package).is_some() {
                    errors.push(PackageError::new(
                        "?",
                        &dir.display().to_string(),
                        "duplicate package id in the loaded set",
                    ));
                }
            }
            Err(mut package_errors) => errors.append(&mut package_errors),
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    // Dependency edges and version checks.
    let mut order = Vec::new();
    let mut visiting: BTreeSet<String> = BTreeSet::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    for id in by_id.keys() {
        visit(
            id,
            &by_id,
            &mut visiting,
            &mut visited,
            &mut order,
            &mut errors,
        );
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    // Cross-package content duplicate detection before compilation.
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut owned_schemas: BTreeMap<ContentKey, Vec<ContentKey>> = BTreeMap::new();
    let mut pack = engine_base.clone();
    for id in &order {
        let package = &by_id[id];
        let mut schemas = Vec::new();
        let mut states = Vec::new();
        let mut blocks = Vec::new();

        let mut paths: Vec<PathBuf> = std::fs::read_dir(package.dir.join("content"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
                    .collect()
            })
            .unwrap_or_default();
        paths.sort();
        for path in paths {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => {
                    errors.push(PackageError::new(
                        id,
                        &path.display().to_string(),
                        format!("content unreadable: {error}"),
                    ));
                    continue;
                }
            };
            let value: toml::Value = match toml::from_str(&text) {
                Ok(value) => value,
                Err(error) => {
                    errors.push(PackageError::new(
                        id,
                        &path.display().to_string(),
                        format!("toml parse failed: {error}"),
                    ));
                    continue;
                }
            };
            let contributions = parse_content_value(id, &value, &mut errors, &mut Vec::new());
            schemas.extend(contributions.schemas);
            states.extend(contributions.states);
            blocks.extend(contributions.blocks);
        }

        let schema_map: BTreeMap<ContentKey, ContentComponentSchema> = schemas
            .iter()
            .cloned()
            .map(|schema| (schema.id.clone(), schema))
            .collect();
        for schema in &schemas {
            if !seen.insert(("component".to_string(), schema.id.to_string())) {
                errors.push(PackageError::new(
                    id,
                    "content",
                    format!("duplicate component schema `{}`", schema.id),
                ));
            }
        }
        // Every component must be attached to a declared state schema; the
        // registry only collects components through state schemas.
        let mut attached: BTreeSet<ContentKey> = BTreeSet::new();
        for state in &states {
            if !seen.insert(("state".to_string(), state.schema.id.to_string())) {
                errors.push(PackageError::new(
                    id,
                    "content",
                    format!("duplicate state schema `{}`", state.schema.id),
                ));
            }
            for reference in &state.component_refs {
                attached.insert(reference.clone());
            }
        }
        for schema in &schemas {
            if !attached.contains(&schema.id) {
                errors.push(PackageError::new(
                    id,
                    "content",
                    format!(
                        "component schema `{}` is not attached to any state schema",
                        schema.id
                    ),
                ));
            }
        }
        for block in &blocks {
            if !seen.insert(("block".to_string(), block.id.to_string())) {
                errors.push(PackageError::new(
                    id,
                    "content",
                    format!("duplicate block `{}`", block.id),
                ));
            }
        }
        if !errors.is_empty() {
            continue;
        }

        let principal = package.manifest.principal();
        owned_schemas.insert(
            principal.clone(),
            schemas.iter().map(|schema| schema.id.clone()).collect(),
        );
        let source = SourceRef::new(format!("mod:{}", package.manifest.id), 1);
        pack.modules.push(Sourced::new(
            source.clone(),
            ModuleDescriptor {
                id: principal.clone(),
                module_version: 1,
                framework_api: 1,
                side: ModuleSide::Server,
                required_modules: package
                    .manifest
                    .dependencies
                    .iter()
                    .map(|(dep, _)| {
                        parse_key(&format!("{dep}:module")).expect("validated namespace")
                    })
                    .collect(),
                optional_modules: Vec::new(),
                provided_services: Vec::new(),
                required_services: Vec::new(),
                registered_schemas: states.iter().map(|state| state.schema.id.clone()).collect(),
                registered_capabilities: Vec::new(),
                registered_actions: Vec::new(),
                registered_systems: Vec::new(),
                requested_permissions: package
                    .manifest
                    .permissions
                    .iter()
                    .map(|permission| RequestedPermission {
                        id: permission.clone(),
                    })
                    .collect(),
                configuration_schema: None,
                lifecycle_binding: None,
            },
        ));
        // Resolve state component references into the declared schemas.
        for state in states {
            let mut resolved = state.schema;
            let mut component_schemas = Vec::new();
            for reference in &state.component_refs {
                match schema_map.get(reference) {
                    Some(schema) => component_schemas.push(schema.clone()),
                    None => errors.push(PackageError::new(
                        id,
                        "content",
                        format!("state references unknown component `{reference}`"),
                    )),
                }
            }
            resolved.component_schemas = component_schemas;
            pack.state_schemas
                .push(Sourced::new(source.clone(), resolved));
        }
        for block in blocks {
            pack.blocks.push(Sourced::new(source.clone(), block));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let content = compile(pack).map_err(|diagnostics| {
        diagnostics
            .iter()
            .map(|diagnostic| {
                PackageError::new("?", &diagnostic.to_string(), "content compilation failed")
            })
            .collect::<Vec<_>>()
    })?;

    let packages = order
        .into_iter()
        .map(|id| by_id.remove(&id).expect("loaded"))
        .collect();
    Ok(ResolvedModSet {
        packages,
        owned_schemas,
        content,
    })
}

fn visit(
    id: &str,
    by_id: &BTreeMap<String, LoadedPackage>,
    visiting: &mut BTreeSet<String>,
    visited: &mut BTreeSet<String>,
    order: &mut Vec<String>,
    errors: &mut Vec<PackageError>,
) {
    if visited.contains(id) {
        return;
    }
    if !visiting.insert(id.to_string()) {
        errors.push(PackageError::new(
            id,
            "mod.toml",
            "dependency cycle detected",
        ));
        return;
    }
    let package = &by_id[id];
    for (dep, req) in &package.manifest.dependencies {
        match by_id.get(dep) {
            Some(dep_package) => {
                if !req.accepts(dep_package.manifest.version) {
                    errors.push(PackageError::new(
                        id,
                        "mod.toml",
                        format!(
                            "dependency `{dep}` requires a version matching `{req:?}` but {} is loaded",
                            dep_package.manifest.version
                        ),
                    ));
                }
                visit(dep, by_id, visiting, visited, order, errors);
            }
            None => {
                errors.push(PackageError::new(
                    id,
                    "mod.toml",
                    format!("missing dependency `{dep}` in the loaded set"),
                ));
            }
        }
    }
    visiting.remove(id);
    visited.insert(id.to_string());
    order.push(id.to_string());
}

/// Serializes a lock file for the resolved set.
pub fn write_lock(set: &ResolvedModSet) -> String {
    let mut out =
        String::from("# Torse modpack lock. Generated by torse-mod pack; do not edit.\n\n");
    for package in &set.packages {
        let manifest = &package.manifest;
        out.push_str("[[package]]\n");
        out.push_str(&format!("id = \"{}\"\n", manifest.id));
        out.push_str(&format!("version = \"{}\"\n", manifest.version));
        out.push_str(&format!("merkle = \"{}\"\n", package.merkle));
        if let Some(entry) = &manifest.server_entry {
            let hash = package
                .files
                .iter()
                .find(|(path, _)| path == entry)
                .map(|(_, hash)| hash.clone());
            if let Some(hash) = hash {
                out.push_str(&format!("entry = \"{entry}\"\n"));
                out.push_str(&format!("entry-hash = \"{hash}\"\n"));
            }
        }
        out.push_str("permissions = [");
        for (index, permission) in manifest.permissions.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("\"{permission}\""));
        }
        out.push_str("]\n");
        if !manifest.dependencies.is_empty() {
            out.push_str("\n[package.dependencies]\n");
            for (dep, req) in &manifest.dependencies {
                let resolved = set
                    .packages
                    .iter()
                    .find(|candidate| candidate.manifest.id == *dep)
                    .map(|candidate| candidate.manifest.version.as_str())
                    .unwrap_or_else(|| req.to_string_repr());
                out.push_str(&format!("{dep} = \"{resolved}\"\n"));
            }
        }
        out.push('\n');
    }
    out
}

impl VersionReq {
    fn to_string_repr(self) -> String {
        match self {
            Self::MinorCompatible { major, minor } => format!("{major}.{minor}"),
            Self::Exact(version) => version.as_str(),
        }
    }
}

/// Verifies a package directory against its lock entry.
pub fn verify_lock(set: &ResolvedModSet, lock_text: &str) -> Result<(), Vec<PackageError>> {
    let value: toml::Value = toml::from_str(lock_text).map_err(|error| {
        vec![PackageError::new(
            "?",
            "mod.lock",
            format!("parse failed: {error}"),
        )]
    })?;
    let entries = value
        .get("package")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| vec![PackageError::new("?", "mod.lock", "no [[package]] entries")])?;
    let mut errors = Vec::new();
    for entry in entries.iter().filter_map(toml::Value::as_table) {
        let Some(id) = str_field(entry, "id").ok().flatten() else {
            errors.push(PackageError::new("?", "mod.lock", "entry without id"));
            continue;
        };
        let Some(package) = set.packages.iter().find(|p| p.manifest.id == id) else {
            errors.push(PackageError::new(
                &id,
                "mod.lock",
                "locked package is not in the loaded set",
            ));
            continue;
        };
        if let Some(merkle) = str_field(entry, "merkle").ok().flatten()
            && merkle != package.merkle
        {
            errors.push(PackageError::new(
                &id,
                "mod.lock",
                format!(
                    "merkle mismatch: lock has {merkle}, directory has {}",
                    package.merkle
                ),
            ));
        }
        if let Some(version) = str_field(entry, "version").ok().flatten()
            && version != package.manifest.version.as_str()
        {
            errors.push(PackageError::new(
                &id,
                "mod.lock",
                format!(
                    "version mismatch: lock has {version}, manifest has {}",
                    package.manifest.version
                ),
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Builds the registries for a resolved set.
pub fn registries(
    content: &torse_content::CompiledContent,
) -> Result<(BlockStateRegistry, ComponentSchemas), String> {
    let registry = BlockStateRegistry::build(content)
        .map_err(|error| format!("registry build failed: {error:?}"))?;
    let schemas = ComponentSchemas::build(content);
    Ok((registry, schemas))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_text(id: &str, version: &str) -> String {
        format!(
            "id = \"{id}\"\nversion = \"{version}\"\napi = \"0.3\"\nside = \"server\"\npermissions = []\n"
        )
    }

    #[test]
    fn manifest_parses_minimal() {
        let manifest =
            Manifest::parse(&manifest_text("machine", "0.1.0")).expect("manifest parses");
        assert_eq!(manifest.id, "machine");
        assert_eq!(manifest.version, Version::parse("0.1.0").unwrap());
        assert!(manifest.side.includes_server());
        assert_eq!(manifest.principal().to_string(), "machine:module");
    }

    #[test]
    fn manifest_rejects_bad_api_and_side() {
        let errors = Manifest::parse(
            "id = \"machine\"\nversion = \"0.1.0\"\napi = \"0.1\"\nside = \"upside\"\npermissions = []\n",
        )
        .unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error.detail.contains("api (0, 1) is not supported"))
        );
        assert!(
            errors
                .iter()
                .any(|error| error.detail.contains("`side` must be"))
        );
    }

    #[test]
    fn version_requirements() {
        let req = VersionReq::parse("0.1").unwrap();
        assert!(req.accepts(Version::parse("0.1.7").unwrap()));
        assert!(!req.accepts(Version::parse("0.2.0").unwrap()));
        let exact = VersionReq::parse("0.1.3").unwrap();
        assert!(exact.accepts(Version::parse("0.1.3").unwrap()));
        assert!(!exact.accepts(Version::parse("0.1.4").unwrap()));
    }

    #[test]
    fn block_parser_accepts_cross_shape_and_face_overlay() {
        let value = toml::from_str::<toml::Value>(
            r#"
id = "example:plant"
schema = "example:plant"

[render]
shape = "cross"
material = "base:material/plant_base"

[render.overlays.pos-x]
material = "base:material/plant_overlay"
alpha = "mask"
"#,
        )
        .expect("block parses as TOML");
        let block = parse_block("example", value.as_table().expect("block table"))
            .expect("block descriptor parses");
        assert_eq!(block.render_profile.shape, Shape::Cross);
        let overlay = block
            .render_profile
            .face_overlays
            .get(Face::PosX)
            .expect("overlay exists");
        assert_eq!(overlay.material.to_string(), "base:material/plant_overlay");
        assert_eq!(overlay.alpha_mode, AlphaMode::Mask);
    }
}
