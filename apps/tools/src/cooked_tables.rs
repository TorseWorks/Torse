//! Offline base-game cook entry point. Runtime clients do not parse this manifest.
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use sha2::{Digest as ShaDigest, Sha256};
use torse_assets::{AssetAlias, AssetGuid, texture::Atlas};
use torse_content::{CookImage, CookedLocale, cook, parse_locale, write_cooked};
use torse_types::{ContentKey, Digest};

const SOURCE_LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    #[serde(default)]
    image: Vec<Image>,
    #[serde(default)]
    locale: Vec<Locale>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    guid: String,
    key: String,
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Locale {
    language: String,
    path: String,
}

pub fn command(mut arguments: impl Iterator<Item = String>) -> ExitCode {
    let (Some(source), Some(output)) = (arguments.next(), arguments.next()) else {
        eprintln!("usage: torse-tools cook-base <assets-directory> <output-file>");
        return ExitCode::FAILURE;
    };
    if arguments.next().is_some() {
        eprintln!("cook-base accepts exactly two paths");
        return ExitCode::FAILURE;
    }
    match build(Path::new(&source)).and_then(|bytes| {
        atomic_write(Path::new(&output), &bytes)?;
        Ok(bytes.len())
    }) {
        Ok(bytes) => {
            println!("cooked_bytes={bytes} output={output}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("cook failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn build(source: &Path) -> Result<Vec<u8>, String> {
    let source = source.canonicalize().map_err(|error| error.to_string())?;
    let manifest_bytes = read_source(&source, "cook.toml")?;
    let manifest: Manifest =
        toml::from_str(std::str::from_utf8(&manifest_bytes).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    let mut images = Vec::new();
    for image in manifest.image {
        AssetAlias::path(&image.path).map_err(|error| error.to_string())?;
        if image.guid.len() != 32 || !image.guid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("image guid must contain exactly 32 hexadecimal digits".into());
        }
        let guid =
            AssetGuid(u128::from_str_radix(&image.guid, 16).map_err(|error| error.to_string())?);
        let key = ContentKey::parse(&image.key).map_err(|error| error.to_string())?;
        let bytes = read_source(&source, &image.path)?;
        validate_png(&bytes)?;
        let source_digest = Digest(Sha256::digest(&bytes).into());
        images.push(CookImage {
            guid,
            source_digest,
            key,
            path: image.path,
        });
    }
    let mut locales = Vec::new();
    for locale in manifest.locale {
        let bytes = read_source(&source, &locale.path)?;
        let text = std::str::from_utf8(&bytes).map_err(|error| error.to_string())?;
        locales.push(CookedLocale {
            language: locale.language,
            entries: parse_locale(text).map_err(|error| error.to_string())?,
        });
    }
    // Use the same base-domain registrations as the client's fixture scene.
    let content = base_content()?;
    let tables = cook(&content, &images, &locales).map_err(|error| error.to_string())?;
    write_cooked(&tables).map_err(|error| error.to_string())
}

fn read_source(root: &Path, relative: &str) -> Result<Vec<u8>, String> {
    read_source_limit(root, relative, SOURCE_LIMIT)
}

pub(crate) fn base_content() -> Result<torse_content::CompiledContent, String> {
    let mut pack = torse_content::m0_fixture_pack();
    torse_sim::block_actions::register_content(&mut pack);
    torse_inventory::register_content(&mut pack);
    torse_content::compile(pack).map_err(|errors| format!("content failed: {errors:?}"))
}

pub(crate) fn read_source_limit(
    root: &Path,
    relative: &str,
    limit: u64,
) -> Result<Vec<u8>, String> {
    if relative.is_empty()
        || relative.contains(['\\', ':'])
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("source path must be relative to the pack".into());
    }
    let path = root
        .join(relative)
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !path.starts_with(root) {
        return Err("source path escapes the pack".into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|error| error.to_string())?
        .take(limit.checked_add(1).ok_or("source limit overflow")?)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("author file exceeds its byte limit".into());
    }
    Ok(bytes)
}

fn validate_png(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return Err("image source is not a PNG".into());
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("PNG header length"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("PNG header length"));
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|n| n.checked_mul(4));
    if pixels.is_none_or(|n| n > 64 * 1024 * 1024) {
        return Err("decoded image exceeds its byte limit".into());
    }
    Atlas::decode(bytes).map(|_| ())
}

fn atomic_write(output: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let name = output.file_name().ok_or("output needs a file name")?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let temporary: PathBuf = parent.join(format!(
        ".{}.{}.{nonce}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    let result = (|| {
        file.write_all(bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        fs::rename(&temporary, output).map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_sources_cook_deterministically_and_roundtrip_without_png_bytes() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../games/base/assets");
        let first = build(&root).expect("cook base sources");
        assert_eq!(first, build(&root).unwrap());
        let tables = torse_content::read_cooked(&first).unwrap();
        assert_eq!(tables.images.len(), 4);
        assert_eq!(tables.locales.len(), 2);
        assert_eq!(tables.images[0].guid, AssetGuid(0x0001_0000_0000_0001));
        assert_eq!(
            tables.images[0].source_digest,
            Digest(Sha256::digest(fs::read(root.join("textures/atlas.png")).unwrap()).into())
        );
        assert!(!first.windows(8).any(|bytes| bytes == b"\x89PNG\r\n\x1a\n"));
        assert!(!tables.materials.is_empty());
        let mut pack = torse_content::m0_fixture_pack();
        torse_sim::block_actions::register_content(&mut pack);
        torse_inventory::register_content(&mut pack);
        let content = torse_content::compile(pack).unwrap();
        let registry = torse_world::BlockStateRegistry::build(&content).unwrap();
        assert_eq!(tables.content_authority_digest, content.authority_digest);
        assert_eq!(
            tables.content_presentation_digest,
            content.presentation_digest
        );
        assert_eq!(tables.states.len() + 1, registry.state_count() as usize);
        for (index, row) in tables.states.iter().enumerate() {
            let state = torse_world::BlockStateId((index + 1) as u16);
            assert_eq!(registry.definition_key(state), Some(&row.block));
            assert_eq!(registry.state_values(state), Some(row.values.as_slice()));
            assert_eq!(registry.collision_shape(state), row.collision);
            assert_eq!(registry.occlusion_mask(state), row.occlusion);
            assert_eq!(registry.face_support(state), Some(&row.face_support));
        }
    }

    #[test]
    fn source_paths_cannot_escape_the_pack() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .canonicalize()
            .unwrap();
        for path in ["../Cargo.toml", "/Cargo.toml", "a/./b", "a//b", "X:outside"] {
            assert!(read_source(&root, path).is_err());
        }
    }

    #[test]
    fn command_writes_a_real_file_and_source_failure_preserves_it() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let source = root.join("games/base/assets");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let output = root.join(format!(".work/cook-tool-tests/{nonce}.tcook"));
        let arguments = || {
            [
                source.to_string_lossy().into_owned(),
                output.to_string_lossy().into_owned(),
            ]
            .into_iter()
        };
        assert_eq!(command(arguments()), ExitCode::SUCCESS);
        let original = fs::read(&output).unwrap();
        torse_content::read_cooked(&original).unwrap();
        assert_eq!(command(arguments()), ExitCode::SUCCESS);
        assert_eq!(fs::read(&output).unwrap(), original);
        assert_eq!(
            command(
                [
                    source.join("missing").to_string_lossy().into_owned(),
                    output.to_string_lossy().into_owned()
                ]
                .into_iter()
            ),
            ExitCode::FAILURE
        );
        assert_eq!(fs::read(&output).unwrap(), original);
    }
}
