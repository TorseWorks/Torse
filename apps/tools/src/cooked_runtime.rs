//! A headless example host consumes runtime rows and verified resident images.
use std::{io::Read, path::Path, process::ExitCode};
use torse_assets::cooked::{CookedResources, LoadLimits};
use torse_content::Face;
use torse_types::{EvictionRank, MemoryBudget, MemoryClass, MemoryPool};

pub fn command(mut args: impl Iterator<Item = String>) -> ExitCode {
    let (Some(table), Some(root)) = (args.next(), args.next()) else {
        eprintln!("usage: torse-tools run-cooked <table> <assets-directory> [--memory-mib N]");
        return ExitCode::FAILURE;
    };
    let memory = match (args.next(), args.next(), args.next()) {
        (None, None, None) => 32,
        (Some(flag), Some(value), None) if flag == "--memory-mib" => match value.parse::<u64>() {
            Ok(n @ 1..=4096) => n,
            _ => {
                eprintln!("memory budget must be 1..=4096 MiB");
                return ExitCode::FAILURE;
            }
        },
        _ => {
            eprintln!("expected only --memory-mib N after the two paths");
            return ExitCode::FAILURE;
        }
    };
    match run(Path::new(&table), Path::new(&root), memory * 1024 * 1024) {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("runtime load failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(path: &Path, root: &Path, bytes: u64) -> Result<String, String> {
    let memory = MemoryPool::new(MemoryBudget::new(bytes));
    let limits = LoadLimits::default();
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    if len > limits.table.max_file_bytes as u64 {
        return Err("cooked file exceeds byte limit".into());
    }
    let encoded_memory = memory
        .reserve(MemoryClass::Codec, len, EvictionRank::Pinned)
        .map_err(|e| e.to_string())?;
    let mut encoded = vec![0; len as usize];
    file.read_exact(&mut encoded).map_err(|e| e.to_string())?;
    if file.read(&mut [0]).map_err(|e| e.to_string())? != 0 {
        return Err("cooked file changed during read".into());
    }
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let mut sources = 0;
    let resources = CookedResources::load(
        &encoded,
        memory.clone(),
        limits,
        &mut |path: &str, limit| {
            sources += 1;
            crate::cooked_tables::read_source_limit(&root, path, limit as u64)
        },
    )?;
    let content = crate::cooked_tables::base_content()?;
    let registry = torse_world::BlockStateRegistry::build(&content).map_err(|e| e.to_string())?;
    let tables = resources.tables();
    if tables.content_authority_digest != content.authority_digest
        || tables.content_presentation_digest != content.presentation_digest
        || tables.states.len() + 1 != registry.state_count() as usize
    {
        return Err("cooked table does not match the example host content".into());
    }
    for (index, row) in tables.states.iter().enumerate() {
        let id = torse_world::BlockStateId((index + 1) as u16);
        if registry.definition_key(id) != Some(&row.block)
            || registry.state_values(id) != Some(row.values.as_slice())
            || registry.collision_shape(id) != row.collision
            || registry.occlusion_mask(id) != row.occlusion
            || registry.face_support(id) != Some(&row.face_support)
        {
            return Err("cooked authority row differs from the native registry".into());
        }
        let definition = registry.definition(id).ok_or("missing state definition")?;
        for face in Face::ALL {
            let material = resources
                .material(row.materials[face.index()])
                .ok_or("missing material slot")?;
            if material.key
                != *definition
                    .render_profile
                    .face_materials
                    .resolve(face, &definition.render_profile.material)
                || material.alpha != definition.render_profile.alpha_mode
            {
                return Err("cooked material row differs from the native registry".into());
            }
            let expected = definition.render_profile.face_overlays.get(face);
            let actual = row.overlays[face.index()].and_then(|slot| resources.material(slot));
            if actual.map(|m| (&m.key, m.alpha)) != expected.map(|m| (&m.material, m.alpha_mode)) {
                return Err("cooked overlay row differs from the native registry".into());
            }
        }
    }
    let sample = resources
        .message("en", "item_stone")
        .ok_or("missing cooked locale sample")?;
    let mut pixels = 0;
    for row in &tables.images {
        let (image, rgba) = resources.image(row.guid).ok_or("missing resident image")?;
        if resources.catalog().guid_for_key(&row.key) != Some(row.guid)
            || resources.catalog().guid_for_path(&row.path) != Some(row.guid)
            || rgba.len() != image.width as usize * image.height as usize * 4
        {
            return Err("invalid resident image".into());
        }
        pixels += rgba.len();
    }
    let report = format!(
        "states={} materials={} images={} locales={} source_reads={sources} rgba_bytes={pixels} locale_sample={sample} resident_bytes={} peak_bytes={}",
        tables.states.len(),
        tables.materials.len(),
        tables.images.len(),
        tables.locales.len(),
        memory.usage().total,
        memory.usage().total_peak
    );
    drop(resources);
    drop(encoded);
    drop(encoded_memory);
    if memory.usage().total != 0 {
        return Err("runtime resources retained a memory charge".into());
    }
    Ok(format!("{report} remaining_bytes=0"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn base_runtime_uses_only_cooked_locales_and_verified_png_sources() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let source = root.join("games/base/assets");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let file = root.join(format!(".work/cooked-runtime-tests/{nonce}.tcook"));
        assert_eq!(
            crate::cooked_tables::command(
                [
                    source.to_string_lossy().into_owned(),
                    file.to_string_lossy().into_owned()
                ]
                .into_iter()
            ),
            ExitCode::SUCCESS
        );
        let report = run(&file, &source, 32 * 1024 * 1024).unwrap();
        assert!(report.contains("images=4 locales=2 source_reads=4"));
        assert!(report.contains("locale_sample=Stone"));
        assert!(report.contains("remaining_bytes=0"));
        assert!(run(&file, &source, 1024).is_err());
    }
}
