//! Torse client.
//!
//! Two modes share the scene, mesher, and pipeline. `--capture` renders one
//! frame offscreen into a PNG without a window; `--window` opens an interactive
//! winit window with a fly camera, streaming sections, and continuous
//! rendering.

mod camera;
mod frame_pacing;
mod game;
mod input;
mod net;
mod pick;
mod renderer;
mod scene;
mod settings;
mod texture;
mod ui;
mod window;

use std::process::ExitCode;
use std::time::Instant;

use camera::Camera;
use renderer::Renderer;
use scene::Scene;
use window::{WindowConfig, run_window};

const USAGE: &str = "usage: torse-client --capture <png> [--width N] [--height N] [--radius N] [--view overview|player]\n\
                    [--place KEY:X,Y,Z]...\n\
       torse-client --window [--width N] [--height N] [--radius N] [--frames N]\n\
                        [--screenshot <png>] [--autoplay] [--sensitivity F] [--fps-limit 30..1000]\n\
                        [--benchmark; requires --frames] [--ui-preview hud|pause|machine|settings|inventory]\n\
                        [--benchmark-motion stationary|local|travel]\n\
                        [--settings-tab debug|gameplay|graphics|controls|ui]\n\
                        [--connect ADDRESS] [--walk] [--section X,Y,Z]...\n\
                        [--tls-ca PEM --tls-cert PEM --tls-key PEM --server-name DNS]\n\
       both modes accept [--memory-mib N; default 512] [--gpu-memory-mib N; default 64] and [--content <dir>] to load a data-driven pack (atlas.toml,\n\
       atlas.png, world.toml) instead of the built-in fixture content";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let mut capture: Option<String> = None;
    let mut window = false;
    let mut width = 960u32;
    let mut height = 540u32;
    let mut radius = 2i64;
    let mut radius_set = false;
    let mut view = String::from("overview");
    let mut frames: Option<u32> = None;
    let mut screenshot: Option<String> = None;
    let mut autoplay = false;
    let mut benchmark = false;
    let mut benchmark_motion = None;
    let mut walk = false;
    let mut connect: Option<String> = None;
    let mut sensitivity = None;
    let mut fps_limit = None;
    let mut probe_outside = false;
    let mut ui_preview = None;
    let mut settings_tab = settings::Tab::Graphics;
    let mut places: Vec<(String, [i64; 3])> = Vec::new();
    let mut content: Option<String> = None;
    let mut memory_mib = 512u64;
    let mut geometry_mib = 64u64;
    let (mut tls_ca, mut tls_cert, mut tls_key, mut server_name) = (None, None, None, None);
    let mut sections = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].clone();
        let mut value = || -> Option<String> {
            index += 1;
            arguments.get(index).cloned()
        };
        match argument.as_str() {
            "--capture" => capture = value(),
            "--window" => window = true,
            "--width" => {
                // Zero-size targets panic in wgpu texture creation; clamp.
                if let Some(parsed) = value().and_then(|v| v.parse::<u32>().ok()) {
                    width = parsed.max(1);
                }
            }
            "--height" => {
                if let Some(parsed) = value().and_then(|v| v.parse::<u32>().ok()) {
                    height = parsed.max(1);
                }
            }
            "--radius" => {
                // A negative radius yields an empty window; treat it as 0.
                if let Some(parsed) = value().and_then(|v| v.parse::<i64>().ok()) {
                    radius = parsed.max(0);
                    radius_set = true;
                }
            }
            "--view" => view = value().unwrap_or(view),
            "--frames" => {
                if let Some(parsed) = value().and_then(|v| v.parse().ok()) {
                    frames = Some(parsed);
                }
            }
            "--screenshot" => screenshot = value(),
            "--autoplay" => autoplay = true,
            "--benchmark" => benchmark = true,
            "--benchmark-motion" => {
                benchmark_motion = match value().as_deref() {
                    Some("stationary") => Some(camera::BenchmarkMotion::Stationary),
                    Some("local") => Some(camera::BenchmarkMotion::Local),
                    Some("travel") => Some(camera::BenchmarkMotion::Travel),
                    _ => {
                        eprintln!("--benchmark-motion requires stationary, local or travel");
                        return ExitCode::FAILURE;
                    }
                };
            }
            "--walk" => walk = true,
            "--connect" => connect = value(),
            "--content" => content = value(),
            "--tls-ca" => tls_ca = value(),
            "--tls-cert" => tls_cert = value(),
            "--tls-key" => tls_key = value(),
            "--server-name" => server_name = value(),
            "--section" => {
                let parsed = value().and_then(|value| {
                    value
                        .split(',')
                        .map(str::parse::<i64>)
                        .collect::<Result<Vec<_>, _>>()
                        .ok()
                });
                match parsed.as_deref() {
                    Some([x, y, z]) if sections.len() < 64 => {
                        sections.push(torse_types::SectionPos::new(*x, *y, *z))
                    }
                    _ => {
                        eprintln!("--section requires X,Y,Z and at most 64 sections");
                        return ExitCode::FAILURE;
                    }
                }
            }
            "--memory-mib" => match value().and_then(|value| value.parse::<u64>().ok()) {
                Some(value @ 80..=16384) => memory_mib = value,
                _ => {
                    eprintln!("--memory-mib requires 80..=16384");
                    return ExitCode::FAILURE;
                }
            },
            "--gpu-memory-mib" => match value().and_then(|value| value.parse::<u64>().ok()) {
                Some(value @ 1..=4096) => geometry_mib = value,
                _ => {
                    eprintln!("--gpu-memory-mib requires 1..=4096");
                    return ExitCode::FAILURE;
                }
            },
            "--sensitivity" => sensitivity = value().and_then(|v| v.parse::<f64>().ok()),
            "--fps-limit" => match value().and_then(|v| v.parse::<u16>().ok()) {
                Some(fps @ 30..=1000) => fps_limit = Some(fps),
                _ => {
                    eprintln!("--fps-limit requires 30..=1000");
                    return ExitCode::FAILURE;
                }
            },
            "--settings-tab" => {
                settings_tab = match value().as_deref() {
                    Some("debug") => settings::Tab::Debug,
                    Some("gameplay") => settings::Tab::Gameplay,
                    Some("graphics") => settings::Tab::Graphics,
                    Some("controls") => settings::Tab::Controls,
                    Some("ui") => settings::Tab::Interface,
                    _ => {
                        eprintln!(
                            "--settings-tab requires debug, gameplay, graphics, controls or ui"
                        );
                        return ExitCode::FAILURE;
                    }
                };
                ui_preview = Some(ui::Mode::Settings);
            }
            "--probe-outside" => probe_outside = true,
            "--place" => match value().as_deref().map(parse_place) {
                Some(Ok(place)) => places.push(place),
                Some(Err(error)) => {
                    eprintln!("{error}");
                    return ExitCode::FAILURE;
                }
                None => {
                    eprintln!("--place expects MATERIAL:X,Y,Z");
                    return ExitCode::FAILURE;
                }
            },
            "--ui-preview" => match value().as_deref().map(ui::Mode::parse) {
                Some(Ok(mode)) => ui_preview = Some(mode),
                _ => {
                    eprintln!("--ui-preview requires hud, pause, machine, settings or inventory");
                    return ExitCode::FAILURE;
                }
            },
            "--help" | "-h" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
        index += 1;
    }

    let tls = match (tls_ca, tls_cert, tls_key, server_name) {
        (None, None, None, None) => None,
        (Some(ca), Some(certificate), Some(key), Some(server_name))
            if window && connect.is_some() =>
        {
            Some(net::TlsClient {
                ca,
                certificate,
                key,
                server_name,
            })
        }
        _ => {
            eprintln!(
                "TLS requires --window --connect and all of --tls-ca, --tls-cert, --tls-key, --server-name"
            );
            return ExitCode::FAILURE;
        }
    };
    if !sections.is_empty() && (!window || connect.is_none()) {
        eprintln!("--section requires --window --connect");
        return ExitCode::FAILURE;
    }
    if window {
        if !places.is_empty() {
            eprintln!("--place is only available with --capture");
            return ExitCode::FAILURE;
        }
        if benchmark && frames.is_none_or(|count| count == 0) {
            eprintln!("--benchmark requires a positive --frames count");
            return ExitCode::FAILURE;
        }
        if benchmark_motion.is_some() && (!benchmark || walk || connect.is_some() || autoplay) {
            eprintln!("--benchmark-motion requires a local fly benchmark without autoplay");
            return ExitCode::FAILURE;
        }
        let mut preferences = match settings::Settings::load(&settings::Settings::path()) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("client settings failed: {error}");
                return ExitCode::FAILURE;
            }
        };
        if radius_set {
            preferences.radius = radius.min(16) as u16;
        }
        if let Some(value) = sensitivity {
            preferences.sensitivity = (value.clamp(0.2, 5.0) * 100.0).round() as u16;
        }
        if let Some(fps) = fps_limit {
            preferences.frame_limit = frame_pacing::FrameLimit { enabled: true, fps };
        }
        if std::env::var_os("TORSE_UI_SKIN").is_some() {
            preferences.skin = settings::SkinChoice::External;
        }
        if let Err(error) = preferences.validate() {
            eprintln!("client settings failed: {error}");
            return ExitCode::FAILURE;
        }
        let config = WindowConfig {
            preferences: preferences.clone(),
            settings_tab,
            sections: (!sections.is_empty()).then_some(sections),
            tls,
            memory_mib,
            geometry_mib,
            radius: i64::from(preferences.radius),
            width,
            height,
            frames,
            screenshot,
            autoplay,
            benchmark,
            benchmark_motion,
            walk,
            connect,
            probe_outside,
            sensitivity: f64::from(preferences.sensitivity) / 100.0,
            fps_limit,
            ui_mode: ui_preview.unwrap_or_default(),
            content: content.clone(),
        };
        match run_window(config) {
            Ok(summary) => {
                println!(
                    "memory_current_bytes={} memory_peak_bytes={} memory_class_peak_bytes={:?}",
                    summary.memory.total, summary.memory.total_peak, summary.memory.peak
                );
                println!(
                    "gpu_geometry_bytes={} gpu_geometry_limit={} gpu_sections={} gpu_evictions={} visible_sections={} culled_sections={}",
                    summary.geometry.bytes,
                    summary.geometry.limit,
                    summary.geometry.sections,
                    summary.geometry.evictions,
                    summary.draws.visible_sections,
                    summary.draws.culled_sections
                );
                println!(
                    "adapter={} backend={} present={} format={} frames={} streams={} rebuilds={} \
                     uploads={} screenshots={} actions_ok={} actions_rejected={} walk_ticks={} \
                     walker=({:.1},{:.1},{:.1}) session={} batches={} requests_sent={} requests_rejected={} baseline_sections={} \
                     sections={} meshes={} vertices={} faces={} elapsed_ms={} \n                     mean_frame_us={} max_frame_us={} stream_frame_us={} rebuild_frame_us={} \
                     cpu_us={} upload_us={} present_us={}",
                    summary.adapter,
                    summary.backend,
                    summary.present,
                    summary.format,
                    summary.frames,
                    summary.streams,
                    summary.rebuilds,
                    summary.uploads,
                    summary.screenshots,
                    summary.actions_ok,
                    summary.actions_rejected,
                    summary.walk_ticks,
                    summary.walker.map_or(0.0, |p| p[0]),
                    summary.walker.map_or(0.0, |p| p[1]),
                    summary.walker.map_or(0.0, |p| p[2]),
                    summary.session.map_or(0, |session| session),
                    summary.batches,
                    summary.requests_sent,
                    summary.requests_rejected,
                    summary.baseline_sections,
                    summary.sections,
                    summary.meshes,
                    summary.vertices,
                    summary.faces,
                    summary.elapsed_ms,
                    summary.mean_frame_us,
                    summary.max_frame_us,
                    summary.stream_frame_us,
                    summary.rebuild_frame_us,
                    summary.cpu_us,
                    summary.upload_us,
                    summary.present_us,
                );
                println!(
                    "cpu_admitted={} cpu_cancelled={} cpu_stale={} cpu_backpressure={} cpu_peak_jobs={} cpu_peak_input_bytes={} cpu_peak_output_bytes={}",
                    summary.cpu.admitted,
                    summary.cpu.cancelled,
                    summary.cpu.stale,
                    summary.cpu.backpressure,
                    summary.cpu.peak_jobs,
                    summary.cpu.peak_input_bytes,
                    summary.cpu.peak_output_bytes,
                );
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("client failed: {error}");
                ExitCode::FAILURE
            }
        }
    } else if let Some(path) = capture {
        match run(
            &path,
            width,
            height,
            radius,
            &view,
            ui_preview,
            &places,
            content.as_deref(),
            memory_mib,
            geometry_mib,
        ) {
            Ok(summary) => {
                println!("{summary}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("client failed: {error}");
                ExitCode::FAILURE
            }
        }
    } else {
        println!("{USAGE}");
        ExitCode::SUCCESS
    }
}

/// Render one offscreen frame and write it as a PNG.
///
/// Every parameter comes straight from the command line, so the list is long by
/// construction rather than by accident.
#[allow(clippy::too_many_arguments)]
fn run(
    path: &str,
    width: u32,
    height: u32,
    radius: i64,
    view: &str,
    ui_preview: Option<ui::Mode>,
    places: &[(String, [i64; 3])],
    content: Option<&str>,
    memory_mib: u64,
    geometry_mib: u64,
) -> Result<String, String> {
    let start = Instant::now();
    let pool =
        torse_types::MemoryPool::new(torse_types::MemoryBudget::new(memory_mib * 1024 * 1024));
    let startup = pool
        .reserve(
            torse_types::MemoryClass::Codec,
            64 * 1024 * 1024,
            torse_types::EvictionRank::Pinned,
        )
        .map_err(|error| error.to_string())?;
    let world_memory =
        Scene::startup_world_reservation(&pool, radius).map_err(|error| error.to_string())?;
    let mut scene = match content {
        Some(directory) => Scene::new_game(radius, std::path::Path::new(directory))?,
        None => Scene::new(radius)?,
    };
    if ui_preview.is_some() {
        scene.enable_items(None)?;
    }
    scene
        .attach_memory_prepared(pool.clone(), world_memory)
        .map_err(|error| error.to_string())?;
    drop(startup);
    for (material, [x, y, z]) in places {
        let key = torse_types::ContentKey::parse(material)
            .map_err(|error| format!("--place `{material}`: {error}"))?;
        scene
            .apply_edit(true, &key, torse_types::BlockPos::new(*x, *y, *z), 0)
            .map_err(|error| format!("--place `{material}`: {error}"))?;
    }
    scene.enable_lighting()?;
    let mut resolved = false;
    for _ in 0..4096 {
        if scene.refresh_lighting(16_384)? {
            resolved = true;
            break;
        }
    }
    if !resolved {
        return Err("lighting did not fit the capture budget".into());
    }
    let _frame = scene
        .reserve_frame_memory(u64::from(width) * u64::from(height) * 16 + 64 * 1024 * 1024)
        .map_err(|error| error.to_string())?;
    scene.refresh_meshes();
    let _geometry = pool
        .reserve(
            torse_types::MemoryClass::Codec,
            scene.geometry_work_bytes(),
            torse_types::EvictionRank::Pinned,
        )
        .map_err(|error| error.to_string())?;
    let (vertices, indices, faces) = scene.geometry();
    if indices.is_empty() {
        return Err("scene produced no geometry".to_owned());
    }
    let vertex_bytes = Scene::vertex_bytes(&vertices);

    let mut renderer = Renderer::new(width, height)?;
    renderer.set_geometry_limit(geometry_mib * 1024 * 1024);
    renderer.attach_memory(pool.clone());
    renderer.set_palette(scene.palette());
    if !renderer.upload_section(torse_types::SectionPos::ZERO, &vertex_bytes, &indices) {
        return Err("terrain geometry or upload metadata exceeds memory budget".into());
    }

    let (eye, target) = match view {
        "player" => {
            let eye = scene.eye();
            (eye, [eye[0], eye[1] - 1.5, eye[2] - 12.0])
        }
        _ => ([40.0, 18.0, 40.0], [0.0, 6.0, 0.0]),
    };
    let camera = Camera::new(eye, target);
    if !renderer.render(
        camera.view_projection(width as f32 / height as f32),
        camera.eye,
    ) {
        return Err("transparent sorting exceeds memory budget".into());
    }
    if let Some(mode) = ui_preview {
        renderer.render_ui(mode, &scene)?;
    }
    renderer.save_png(path)?;

    let elapsed = start.elapsed();
    let gpu = renderer.geometry_usage();
    let draws = renderer.draw_stats();
    Ok(format!(
        "sections={} meshes={} entities={} seq={} vertices={} indices={} faces={} eye=({:.1},{:.1},{:.1}) elapsed_ms={} gpu_geometry_bytes={} gpu_geometry_limit={} visible_sections={} culled_sections={}",
        scene.sections().len(),
        scene.mesh_count(),
        scene.authority().entities().len(),
        scene.authority().seq().0,
        vertices.len(),
        indices.len(),
        faces,
        eye[0],
        eye[1],
        eye[2],
        elapsed.as_millis(),
        gpu.bytes,
        gpu.limit,
        draws.visible_sections,
        draws.culled_sections
    ))
}

/// Parse one `MATERIAL:X,Y,Z` placement argument.
fn parse_place(spec: &str) -> Result<(String, [i64; 3]), String> {
    let (material, coordinates) = spec
        .rsplit_once(':')
        .ok_or_else(|| format!("--place expects MATERIAL:X,Y,Z, got `{spec}`"))?;
    let parts: Vec<&str> = coordinates.split(',').collect();
    if parts.len() != 3 {
        return Err(format!("--place expects three coordinates in `{spec}`"));
    }
    let mut values = [0i64; 3];
    for (slot, part) in values.iter_mut().zip(parts) {
        *slot = part
            .trim()
            .parse()
            .map_err(|_| format!("--place coordinate `{part}` is not an integer"))?;
    }
    Ok((material.to_string(), values))
}
