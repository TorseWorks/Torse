//! Interactive windowed client.
//!
//! One winit window driven by [`ApplicationHandler`]. Each frame pumps a fixed
//! logic step from measured time, streams and uploads on the presentation
//! frame, and draws through the shared [`GpuContext`]. The draw does not
//! advance logic. Sections outside the radius unload, so sustained flight
//! keeps the resident set bounded.

use std::sync::Arc;
use std::time::Instant;

use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, CurrentSurfaceTexture, Device,
    Extent3d, PollType, Queue, Surface, SurfaceConfiguration, SurfaceTexture, TexelCopyBufferInfo,
    TexelCopyBufferLayout, TexelCopyTextureInfo, Texture, TextureAspect, TextureDescriptor,
    TextureDimension, TextureFormat, TextureUsages, TextureView, TextureViewDescriptor,
};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::camera::{FlyCamera, MoveInput};
use crate::input::InputState;
use crate::pick::PickOutcome;
use crate::renderer::GpuContext;
use crate::scene::Scene;
use crate::texture::Palette;
use torse_render::{FrameBudget, FrameLimits, SectionUploadQueue};
use torse_sim::PresentationClock;
use torse_types::{BlockPos, ContentKey, SectionPos};
use torse_world::Position;
mod items;
mod settings;

/// How far the pick ray reaches, in blocks.
const PICK_RANGE: f32 = 64.0;
/// Sections generated per frame while streaming; spreading the work keeps
/// the frame time bounded at the cost of populating a window over a few
/// frames.
const FRAME_LIMITS: FrameLimits = FrameLimits {
    generation: 4,
    meshing: 4,
    uploads: 4,
    upload_bytes: 8 * 1024 * 1024,
};
/// Fixed logic step in seconds. A presentation frame may run several of these
/// and must not stretch one step to match the frame.
const LOGIC_STEP_SECONDS: f64 = 1.0 / 20.0;
/// Most logic steps one presentation frame may catch up.
const MAX_LOGIC_STEPS: u32 = 8;

/// Autoplay fires one demonstration action every this many frames.
const AUTOPLAY_PERIOD: u32 = 24;
/// Camera eye height above the walker's body centre.
const WALKER_EYE_OFFSET: f64 = 0.7;

/// Startup parameters for the windowed run.
#[derive(Clone, Debug)]
pub struct WindowConfig {
    pub preferences: crate::settings::Settings,
    pub settings_tab: crate::settings::Tab,
    /// Explicit section interest; absent keeps the local demonstration scope.
    pub sections: Option<Vec<torse_types::SectionPos>>,
    pub tls: Option<crate::net::TlsClient>,
    /// Shared CPU memory reservation ceiling in MiB.
    pub memory_mib: u64,
    /// Independent logical resident geometry payload ceiling in MiB.
    pub geometry_mib: u64,
    /// Section loading radius around the camera.
    pub radius: i64,
    /// Initial window size in logical pixels.
    pub width: u32,
    pub height: u32,
    /// Exit automatically after this many presented frames.
    pub frames: Option<u32>,
    /// Save the final presented frame to this path before exiting.
    pub screenshot: Option<String>,
    /// Fly forward automatically instead of waiting for keys.
    pub autoplay: bool,
    /// Ignore interactive input and leave the cursor free during finite measurement.
    pub benchmark: bool,
    pub benchmark_motion: Option<crate::camera::BenchmarkMotion>,
    /// Start in walk mode with a player entity on the ground.
    pub walk: bool,
    /// Connect to a standalone server and mirror its authorized projection.
    pub connect: Option<String>,
    /// Captured-look sensitivity multiplier; scroll steps multiply it in
    /// walk mode.
    pub sensitivity: f64,
    pub fps_limit: Option<u16>,
    /// Send one edit request far outside the approved scope, for acceptance
    /// evidence that the server rejects out-of-scope targets.
    pub probe_outside: bool,
    /// Initial UI screen; machine mode is explicitly a sample-data preview.
    pub ui_mode: crate::ui::Mode,
    /// Data-driven content pack directory; `None` keeps the fixture content.
    pub content: Option<String>,
}

/// Measurements reported when the window closes.
#[derive(Clone, Debug, Default)]
pub struct WindowSummary {
    pub geometry: crate::renderer::GeometryUsage,
    pub draws: crate::renderer::DrawStats,
    /// Logical shared reservation usage; excludes native allocator and GPU memory.
    pub memory: torse_types::MemoryUsage,
    /// Adapter name reported by the backend.
    pub adapter: String,
    /// Backend selected for the surface.
    pub backend: String,
    /// Surface colour format the pipeline was built for.
    pub format: String,
    /// Present mode selected for the surface.
    pub present: String,
    /// Presented frames.
    pub frames: u32,
    /// Times the section window moved with the camera.
    pub streams: usize,
    /// Section meshes rebuilt.
    pub rebuilds: usize,
    /// Geometry uploads to the GPU.
    pub uploads: usize,
    /// Screenshots written.
    pub screenshots: usize,
    /// Actions planned and committed through the action service.
    pub actions_ok: u32,
    /// Actions rejected by the action service or the commit.
    pub actions_rejected: u32,
    /// Walk ticks committed while in walk mode.
    pub walk_ticks: u32,
    /// Final walker position, when walk mode was entered.
    pub walker: Option<[f64; 3]>,
    /// Server session, when a connection was established.
    pub session: Option<u128>,
    /// Replica increments applied.
    pub batches: u32,
    /// Edit requests sent over the connection.
    pub requests_sent: u32,
    /// Requests the server rejected at admission.
    pub requests_rejected: u32,
    /// Sections received in the baseline.
    pub baseline_sections: usize,
    /// Resident sections at exit.
    pub sections: usize,
    /// Resident meshes at exit.
    pub meshes: usize,
    /// Final vertex and face counts.
    pub vertices: usize,
    pub faces: usize,
    /// Wall time from resume to exit.
    pub elapsed_ms: u128,
    /// Mean frame interval in microseconds.
    pub mean_frame_us: u64,
    /// Longest frame interval in microseconds.
    pub max_frame_us: u64,
    /// Mean frame time while streaming new sections.
    pub stream_frame_us: u64,
    /// Mean frame time containing mesh rebuilds.
    pub rebuild_frame_us: u64,
    /// Mean main-thread streaming and mesh admission time.
    pub cpu_us: u64,
    /// Mean terrain-buffer upload time.
    pub upload_us: u64,
    /// Mean render encoding and presentation time.
    pub present_us: u64,
    pub cpu: crate::scene::BackgroundStats,
}

struct Gpu {
    present_modes: Vec<wgpu::PresentMode>,
    ui: crate::ui::Layer,
    context: GpuContext,
    surface: Surface<'static>,
    config: SurfaceConfiguration,
    depth: Texture,
    depth_view: TextureView,
    window: Arc<Window>,
    backend_name: String,
    present_mode: String,
}

struct WindowApp {
    preferences_dirty: Option<Instant>,
    debug_lines: Vec<String>,
    current_fps: u32,
    work_budget: FrameBudget,
    pending_uploads: SectionUploadQueue,
    ui: crate::ui::State,
    config: WindowConfig,
    frame_pacer: crate::frame_pacing::FramePacer,
    scene: Option<Scene>,
    camera: Option<FlyCamera>,
    input: InputState,
    gpu: Option<Gpu>,
    materials: Vec<ContentKey>,
    material: usize,
    /// Whether the hotbar names come from a pack instead of the packaged icons.
    pack: bool,
    center: Option<SectionPos>,
    uploaded: bool,
    entity_sequence: Option<u64>,
    frames: u32,
    streams: usize,
    rebuilds: usize,
    uploads: usize,
    screenshots: usize,
    actions_ok: u32,
    actions_rejected: u32,
    autoplay_break: bool,
    net: Option<crate::net::Connection<torse_net::TcpTransport>>,
    session: Option<u128>,
    grabbed: bool,
    focused: bool,
    look_sensitivity: f64,
    probe_sent: bool,
    batches: u32,
    requests_sent: u32,
    requests_rejected: u32,
    walk: bool,
    presentation: PresentationClock,
    walk_ticks: u32,
    /// Eye at the last completed logic step.
    present_prev: [f32; 3],
    /// Eye at the logic step the frame interpolates toward.
    present_next: [f32; 3],
    /// Autoplay sets this when the walker stopped against a terrain step.
    walk_blocked: bool,
    last_result: String,
    adapter: String,
    started: Option<Instant>,
    last_frame: Option<Instant>,
    fps_origin: Instant,
    fps_frames: u32,
    frame_total_us: u128,
    benchmark_mark_us: u128,
    benchmark_stages: [u128; 4],
    frame_max_us: u128,
    frame_count: u32,
    streams_mark: usize,
    stream_total_us: u128,
    stream_count: u32,
    rebuilds_mark: usize,
    rebuild_total_us: u128,
    rebuild_count: u32,
    cpu_total_us: u128,
    cpu_samples: u32,
    upload_total_us: u128,
    upload_samples: u32,
    present_total_us: u128,
    present_samples: u32,
    pending_shot: Option<String>,
    /// Palette waiting for the next frame to be uploaded, set when a scene is
    /// installed before the GPU context exists or from a network baseline.
    pending_palette: Option<Palette>,
    summary: Option<WindowSummary>,
    failure: Option<String>,
    rebuild_network: bool,
    memory: torse_types::MemoryPool,
}

fn mean_us(total: u128, samples: u32) -> u64 {
    if samples == 0 {
        0
    } else {
        u64::try_from(total / u128::from(samples)).unwrap_or(0)
    }
}

impl WindowApp {
    fn new(config: WindowConfig) -> Self {
        let memory = torse_types::MemoryPool::new(torse_types::MemoryBudget::new(
            config.memory_mib * 1024 * 1024,
        ));
        let materials = ["base:stone", "base:glass_slab", "base:plant"]
            .into_iter()
            .map(|name| ContentKey::parse(name).expect("static material key"))
            .collect();
        Self {
            preferences_dirty: None,
            debug_lines: Vec::new(),
            current_fps: 0,
            work_budget: FrameBudget::new(FRAME_LIMITS, 16_666),
            pending_uploads: SectionUploadQueue::default(),
            ui: {
                let mut state = crate::ui::State::default();
                state.set_mode(config.ui_mode);
                state.settings = config.preferences.clone();
                state.tab = config.settings_tab;
                if let Some(fps) = config.fps_limit {
                    state.settings.frame_limit =
                        crate::frame_pacing::FrameLimit { enabled: true, fps };
                }
                state
            },
            config,
            frame_pacer: crate::frame_pacing::FramePacer::default(),
            scene: None,
            camera: None,
            input: InputState::new(),
            gpu: None,
            materials,
            material: 0,
            pack: false,
            center: None,
            uploaded: false,
            entity_sequence: None,
            frames: 0,
            streams: 0,
            rebuilds: 0,
            uploads: 0,
            screenshots: 0,
            actions_ok: 0,
            actions_rejected: 0,
            autoplay_break: false,
            net: None,
            session: None,
            grabbed: false,
            focused: true,
            look_sensitivity: 1.0,
            probe_sent: false,
            batches: 0,
            requests_sent: 0,
            requests_rejected: 0,
            walk: false,
            presentation: PresentationClock::new(LOGIC_STEP_SECONDS, MAX_LOGIC_STEPS)
                .expect("logic step and catch-up cap are positive"),
            walk_ticks: 0,
            present_prev: [0.0; 3],
            present_next: [0.0; 3],
            walk_blocked: false,
            last_result: String::new(),
            adapter: String::new(),
            started: None,
            last_frame: None,
            fps_origin: Instant::now(),
            fps_frames: 0,
            frame_total_us: 0,
            benchmark_mark_us: 0,
            benchmark_stages: [0; 4],
            frame_max_us: 0,
            frame_count: 0,
            streams_mark: 0,
            stream_total_us: 0,
            stream_count: 0,
            rebuilds_mark: 0,
            rebuild_total_us: 0,
            rebuild_count: 0,
            cpu_total_us: 0,
            cpu_samples: 0,
            upload_total_us: 0,
            upload_samples: 0,
            present_total_us: 0,
            present_samples: 0,
            pending_shot: None,
            pending_palette: None,
            summary: None,
            failure: None,
            rebuild_network: false,
            memory,
        }
    }

    fn poll_network(&mut self, event_loop: &ActiveEventLoop) -> bool {
        // Drain the connection first: the baseline creates the scene and
        // increments refresh it before this frame's meshes are captured.
        let events = match self.net.as_mut().map(crate::net::Connection::poll) {
            Some(Ok(events)) => events,
            Some(Err(error)) => {
                self.failure = Some(format!("connection failed: {error}"));
                event_loop.exit();
                return false;
            }
            None => Vec::new(),
        };
        for event in events {
            if let Err(error) = self.apply_net_event(event.event) {
                self.failure = Some(error);
                event_loop.exit();
                return false;
            }
        }
        if self.rebuild_network
            && let Err(error) = self.rebuild_network_scene()
        {
            self.failure = Some(error);
            event_loop.exit();
            return false;
        }
        true
    }

    fn frame(&mut self, event_loop: &ActiveEventLoop) {
        // The HUD needs the pack labels before the GPU borrow starts.
        self.sync_inventory();
        let names = self.hotbar_names();
        let frame_started = Instant::now();
        self.frame_pacer
            .started(frame_started, self.ui.settings.frame_limit);
        if !self.poll_network(event_loop) {
            return;
        }

        if self.camera.is_none() {
            return;
        }
        // Protect presentation headroom before world jobs can fill the pool.
        // Release it only at draw time, so menus and diagnostics still repaint.
        let presentation_memory = match self.scene.as_mut() {
            Some(scene) => scene.reserve_frame_memory(4 * 1024 * 1024),
            None => self
                .memory
                .reserve(
                    torse_types::MemoryClass::Codec,
                    4 * 1024 * 1024,
                    torse_types::EvictionRank::Pinned,
                )
                .map(Some),
        }
        .ok()
        .flatten();
        let frame_bytes = self.gpu.as_ref().map_or(0, |gpu| {
            u64::from(gpu.config.width) * u64::from(gpu.config.height) * 16 + 64 * 1024 * 1024
        });
        let frame_memory = match self.scene.as_mut() {
            Some(scene) => scene.reserve_frame_memory(frame_bytes),
            None => self
                .memory
                .reserve(
                    torse_types::MemoryClass::Codec,
                    frame_bytes,
                    torse_types::EvictionRank::Pinned,
                )
                .map(Some),
        };
        let _frame_memory = match frame_memory {
            Ok(memory) => memory,
            Err(_) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.ui.clear_caches();
                }
                self.last_result = "memory pressure: world work deferred".to_owned();
                self.draw(event_loop, &names, frame_started, presentation_memory);
                return;
            }
        };
        if self.scene.is_none() {
            // Connected but no baseline yet: keep presenting cleared frames
            // so polling continues and the window does not sit on an
            // uninitialized surface.
            if let Some(gpu) = self.gpu.as_mut() {
                match gpu.surface.get_current_texture() {
                    CurrentSurfaceTexture::Success(frame)
                    | CurrentSurfaceTexture::Suboptimal(frame) => {
                        let view = frame.texture.create_view(&TextureViewDescriptor::default());
                        let (device, queue) = gpu.context.parts();
                        let mut encoder =
                            device.create_command_encoder(&CommandEncoderDescriptor {
                                label: Some("client idle frame"),
                            });
                        gpu.context
                            .encode_draw(&mut encoder, &view, &gpu.depth_view);
                        gpu.ui.prepare(
                            device,
                            queue,
                            [gpu.config.width, gpu.config.height],
                            &self.ui,
                            crate::ui::Hud {
                                material: self.material,
                                network: self.net.is_some(),
                                ready: false,
                                result: &self.last_result,
                                names: names.clone(),
                                sensitivity: self.look_sensitivity,
                                debug_lines: &self.debug_lines,
                                inventory: self
                                    .scene
                                    .as_ref()
                                    .map_or_else(Vec::new, Scene::inventory_slots),
                            },
                        );
                        gpu.ui.draw(&mut encoder, &view);
                        queue.submit([encoder.finish()]);
                        queue.present(frame);
                    }
                    _ => {}
                }
                gpu.window.request_redraw();
            }
            return;
        }
        let dt = self.frame_interval(frame_started);
        let speed_now = self.camera.as_ref().expect("camera").speed;
        let (mut motion, speed) = self.input.take_input(speed_now);
        self.camera.as_mut().expect("camera").speed = speed;
        if (speed - speed_now).abs() > f32::EPSILON {
            self.ui.settings.fly_speed = speed.round().clamp(1.0, 300.0) as u16;
            self.save_preferences();
        }
        if self.config.autoplay && !self.ui.modal() {
            motion.forward = 1.0;
        }
        if let Err(reason) = self.pump_logic(dt, motion) {
            self.failure = Some(reason);
            event_loop.exit();
            return;
        }

        if let Some(motion) = self.config.benchmark_motion {
            let eye = motion.eye(self.frames);
            if let Some(camera) = self.camera.as_mut() {
                camera.eye = eye;
            }
            self.present_prev = eye;
            self.present_next = eye;
        }
        let center = self
            .scene
            .as_ref()
            .expect("camera implies scene")
            .center_section(self.camera.as_ref().expect("camera").eye);
        // Local scenes stream continuously with a per-frame generation
        // budget: a crossing no longer stalls one frame with a whole ring of
        // fresh sections. Network scenes keep the subscribed sections.
        let mut cpu_us = 0u128;
        if self.scene.as_ref().is_some_and(|scene| !scene.is_network()) {
            let Some(scene) = self.scene.as_mut() else {
                return;
            };
            let started = Instant::now();
            if let Err(error) = scene.stream_around(center, self.work_budget.limits().generation) {
                self.failure = Some(format!("streaming failed: {error}"));
                event_loop.exit();
                return;
            }
            cpu_us += started.elapsed().as_micros();
            if self.center != Some(center) {
                self.center = Some(center);
                self.streams += 1;
            }
        } else {
            self.center = None;
        }

        let streamed_at = Instant::now();
        // Resolve the aim from the fresh camera, translate clicks and the
        // autoplay demo into place or break intents, and commit them before
        // this frame's meshes are captured.
        let aim = {
            let Some(scene) = self.scene.as_ref() else {
                return;
            };
            let Some(camera) = self.camera.as_ref() else {
                return;
            };
            scene.pick(camera.eye, camera.forward(), PICK_RANGE)
        };
        let mut intent = self.input.take_click().and_then(|button| {
            action_for(
                self.scene.as_ref(),
                &aim,
                button,
                &self.materials[self.material],
            )
        });
        // The walk demo is about walking; firing place or break at the aim
        // while walking would block the walker's own path.
        let network = self.scene.as_ref().is_some_and(Scene::is_network);
        if network
            && self.config.probe_outside
            && !self.probe_sent
            && self.frames > 12
            && let Some(net) = self.net.as_mut()
        {
            // One deliberate out-of-scope target; the server should reject
            // it and neither world nor mirror changes.
            self.probe_sent = true;
            let call = crate::net::edit_call(
                false,
                &self.materials[self.material],
                BlockPos::new(200, 1, 1),
                0,
            );
            match net.request(call) {
                Ok(seq) => {
                    self.requests_sent += 1;
                    self.last_result = format!("outside probe seq {} sent", seq.0);
                }
                Err(error) => self.last_result = format!("probe failed: {error}"),
            }
        }
        if network
            && self.config.autoplay
            && !self.ui.modal()
            && self.frames.is_multiple_of(AUTOPLAY_PERIOD)
        {
            // Over a connection the demo exercises the request path: the
            // guest session should see its edit rejected by admission.
            let aim = {
                let Some(scene) = self.scene.as_ref() else {
                    return;
                };
                let Some(camera) = self.camera.as_ref() else {
                    return;
                };
                scene.pick(camera.eye, camera.forward(), PICK_RANGE)
            };
            if let Some((place, target, half)) = action_for(
                self.scene.as_ref(),
                &aim,
                MouseButton::Left,
                &self.materials[self.material],
            ) {
                let material = &self.materials[self.material];
                let call = self.edit_call(place, material, target, half);
                if let Some(Ok(seq)) = self.net.as_mut().map(|net| net.request(call)) {
                    self.requests_sent += 1;
                    self.last_result = format!("seq {} sent", seq.0);
                }
            }
        }
        if intent.is_none()
            && self.config.autoplay
            && !self.ui.modal()
            && !self.walk
            && !network
            && self.frames.is_multiple_of(AUTOPLAY_PERIOD)
        {
            let button = if self.autoplay_break {
                MouseButton::Left
            } else {
                MouseButton::Right
            };
            intent = action_for(
                self.scene.as_ref(),
                &aim,
                button,
                &self.materials[self.material],
            );
            self.autoplay_break = !self.autoplay_break;
        }
        if let Some((place, target, half)) = intent {
            if self.scene.as_ref().is_some_and(Scene::is_network) {
                // Edits go to the server, not the local mirror. The guest
                // session has no edit grants, so the admission reply carries
                // the rejection; the mirror never forks.
                let material = &self.materials[self.material];
                let call = self.edit_call(place, material, target, half);
                match self.net.as_mut().map(|net| net.request(call)) {
                    Some(Ok(seq)) => self.last_result = format!("seq {} sent", seq.0),
                    Some(Err(error)) => self.last_result = format!("send failed: {error}"),
                    None => self.last_result = "offline".to_owned(),
                }
            } else {
                let material = &self.materials[self.material];
                let call = self.edit_call(place, material, target, half);
                let result = match self.scene.as_mut() {
                    Some(scene) if scene.item_owner().is_some() => scene.apply_item_call(call),
                    Some(scene) => scene.apply_edit(place, material, target, half),
                    None => return,
                };
                match result {
                    Ok(()) => {
                        self.actions_ok += 1;
                        self.last_result.clear();
                    }
                    Err(reason) => {
                        self.actions_rejected += 1;
                        self.last_result = reason;
                    }
                }
            }
        }

        // Stage the selection outline from the post-edit aim so it matches
        // what the next frame will show.
        let outline = if !self.ui.settings.outline {
            Vec::new()
        } else {
            let Some(scene) = self.scene.as_ref() else {
                return;
            };
            let Some(camera) = self.camera.as_ref() else {
                return;
            };
            match scene.pick(camera.eye, camera.forward(), PICK_RANGE) {
                PickOutcome::Hit(hit) => scene.outline_bytes(&hit),
                PickOutcome::Miss => Vec::new(),
            }
        };
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.context.write_lines(&outline);
            let borders = if self.ui.settings.section_borders {
                self.scene
                    .as_ref()
                    .zip(self.camera.as_ref())
                    .map_or_else(Vec::new, |(scene, camera)| {
                        scene.section_border_bytes(camera.eye)
                    })
            } else {
                Vec::new()
            };
            gpu.context.write_borders(&borders);
        }

        let meshing_at = Instant::now();
        let rebuilt = match self
            .scene
            .as_mut()
            .expect("camera implies scene")
            .refresh_visuals(self.work_budget.limits().meshing)
        {
            Ok(rebuilt) => rebuilt,
            Err(error) => {
                self.failure = Some(error);
                event_loop.exit();
                return;
            }
        };
        cpu_us += meshing_at.elapsed().as_micros();
        self.rebuilds += rebuilt.len();
        let meshed_at = Instant::now();
        self.pending_uploads.extend(rebuilt);
        let scene = self.scene.as_ref().expect("camera implies scene");
        if let Some(gpu) = self.gpu.as_mut() {
            let camera = self.camera.as_ref().expect("active scene camera");
            let aspect = gpu.config.width as f32 / gpu.config.height.max(1) as f32;
            gpu.context
                .write_camera(camera.view_projection(aspect), camera.eye);
            gpu.context.set_section_focus(center);
            gpu.context.retain_sections(scene.sections());
            let side = scene.authority().store().side();
            self.pending_uploads
                .extend(scene.sections().iter().copied().filter(|pos| {
                    !gpu.context.has_section(*pos)
                        && scene.section_geometry_bytes(*pos).is_some_and(|bytes| {
                            bytes > 0 && gpu.context.should_upload(*pos, side, bytes)
                        })
                }));
            if !gpu.context.has_section(crate::renderer::ENTITY_SLOT)
                && !scene.authority().entities().is_empty()
            {
                self.entity_sequence = None;
            }
        }
        let surfaces_ready = if let Some(scene) = self.scene.as_mut() {
            scene.prune_surfaces();
            if scene.surfaces_dirty() {
                let (vertices, indices) = scene.surface_geometry();
                let bytes = Scene::vertex_bytes(&vertices);
                let ready = self.gpu.as_mut().is_some_and(|gpu| {
                    gpu.context
                        .upload_section(crate::renderer::SURFACE_SLOT, &bytes, &indices)
                });
                if ready {
                    scene.surfaces_uploaded();
                }
                ready
            } else {
                true
            }
        } else {
            false
        };
        let scene = self.scene.as_ref().expect("active scene");
        let interest: std::collections::BTreeSet<_> = scene.sections().iter().copied().collect();
        self.pending_uploads.retain(|p| interest.contains(&p));
        let upload_started = Instant::now();
        let uploads = match self.pending_uploads.select_where(
            center,
            self.work_budget.limits(),
            |p| {
                surfaces_ready
                    && self.gpu.as_ref().is_some_and(|gpu| {
                        scene.section_geometry_bytes(p).is_none_or(|bytes| {
                            gpu.context
                                .should_upload(p, scene.authority().store().side(), bytes)
                        })
                    })
            },
            |p| scene.section_geometry_bytes(p),
        ) {
            Ok(uploads) => uploads,
            Err(error) => {
                self.failure = Some(error.to_owned());
                event_loop.exit();
                return;
            }
        };
        let sequence = self
            .scene
            .as_ref()
            .expect("camera implies scene")
            .authority()
            .seq()
            .0;
        let mut presented = Vec::new();
        if !uploads.is_empty() || !self.uploaded || self.entity_sequence != Some(sequence) {
            let scene = self.scene.as_ref().expect("camera implies scene");
            if let Some(gpu) = self.gpu.as_mut() {
                // Re-upload only the changed sections; untouched terrain keeps
                // its buffers, so an edit costs one small upload, not the
                // whole scene.
                for pos in &uploads {
                    if let Some((vertices, indices)) = scene.section_geometry(*pos) {
                        let bytes = Scene::vertex_bytes(&vertices);
                        if !gpu.context.upload_section(*pos, &bytes, &indices) {
                            self.pending_uploads.extend([*pos]);
                        } else {
                            self.uploads += 1;
                            presented.push(*pos);
                        }
                    }
                }
                // Entity bodies change without mesh rebuilds; refresh them
                // alongside any terrain upload.
                if self.entity_sequence != Some(sequence) {
                    let (entity_vertices, entity_indices) = if self.ui.settings.entities {
                        scene.entity_geometry()
                    } else {
                        (Vec::new(), Vec::new())
                    };
                    let entity_bytes = Scene::vertex_bytes(&entity_vertices);
                    if gpu.context.upload_section(
                        crate::renderer::ENTITY_SLOT,
                        &entity_bytes,
                        &entity_indices,
                    ) {
                        self.entity_sequence = Some(sequence);
                    }
                }
            }
            self.uploaded = true;
        }
        if let Some(scene) = self.scene.as_mut() {
            for pos in presented {
                scene.surface_section_uploaded(pos);
            }
        }
        self.cpu_total_us += cpu_us;
        self.cpu_samples += 1;
        self.upload_total_us += upload_started.elapsed().as_micros();
        self.upload_samples += 1;
        let uploaded_at = Instant::now();
        // Belt and braces: some focus transitions skip the Focused event, and
        // a captured look with no focus moves the camera from outside.
        if self.grabbed
            && let Some(gpu) = self.gpu.as_ref()
            && !gpu.window.has_focus()
        {
            self.focused = false;
            self.input.reset();
            self.set_grab(false);
        }

        if self.input.take_screenshot() {
            self.pending_shot = Some(format!("torse-shot-{}.png", self.screenshots + 1));
        }

        if self.config.benchmark {
            for (sum, elapsed) in self.benchmark_stages.iter_mut().zip([
                streamed_at.duration_since(frame_started),
                meshed_at.duration_since(meshing_at),
                uploaded_at.duration_since(meshed_at),
            ]) {
                *sum += elapsed.as_micros();
            }
        }
        self.draw(event_loop, &names, frame_started, presentation_memory);
    }

    /// Present one frame. This does not pump logic steps.
    fn draw(
        &mut self,
        event_loop: &ActiveEventLoop,
        names: &[String],
        frame_started: Instant,
        presentation_memory: Option<torse_types::MemoryLease>,
    ) {
        drop(presentation_memory);
        let draw_started = Instant::now();
        let frame = match self.gpu.as_mut() {
            Some(gpu) => match gpu.surface.get_current_texture() {
                CurrentSurfaceTexture::Success(frame)
                | CurrentSurfaceTexture::Suboptimal(frame) => frame,
                CurrentSurfaceTexture::Timeout | CurrentSurfaceTexture::Occluded => {
                    gpu.window.request_redraw();
                    return;
                }
                CurrentSurfaceTexture::Outdated => {
                    let (device, _) = gpu.context.parts();
                    gpu.surface.configure(device, &gpu.config);
                    gpu.window.request_redraw();
                    return;
                }
                CurrentSurfaceTexture::Lost | CurrentSurfaceTexture::Validation => {
                    self.failure = Some("surface lost or failed validation".to_owned());
                    event_loop.exit();
                    return;
                }
            },
            None => return,
        };

        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };
        if let Some(palette) = self.pending_palette.take() {
            gpu.context.set_palette(&palette);
        }
        let view = frame.texture.create_view(&TextureViewDescriptor::default());
        let aspect = gpu.config.width as f32 / gpu.config.height.max(1) as f32;
        let (device, queue) = gpu.context.parts();
        let view_projection = self
            .camera
            .as_ref()
            .expect("gpu implies camera")
            .view_projection(aspect);
        let eye = self.camera.as_ref().expect("gpu implies camera").eye;
        gpu.context.write_camera(view_projection, eye);
        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
            label: Some("client window frame"),
        });
        if !gpu
            .context
            .encode_draw(&mut encoder, &view, &gpu.depth_view)
        {
            // A rejected terrain sort must not hide the interactive UI.
            let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("client pressure fallback"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.1,
                            g: 0.12,
                            b: 0.15,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        gpu.ui.prepare(
            device,
            queue,
            [gpu.config.width, gpu.config.height],
            &self.ui,
            crate::ui::Hud {
                material: self.material,
                network: self.net.is_some(),
                ready: self.scene.is_some(),
                result: &self.last_result,
                names: names.to_vec(),
                sensitivity: self.look_sensitivity,
                debug_lines: &self.debug_lines,
                inventory: self
                    .scene
                    .as_ref()
                    .map_or_else(Vec::new, Scene::inventory_slots),
            },
        );
        gpu.ui.draw(&mut encoder, &view);
        queue.submit([encoder.finish()]);
        if let Some(path) = self.pending_shot.take() {
            // The copy must be queued after the draw pass, otherwise it reads
            // the presented texture before this frame has been drawn into it.
            match capture_frame(
                device,
                queue,
                &frame,
                gpu.config.width,
                gpu.config.height,
                gpu.config.format,
                &path,
            ) {
                Ok(()) => self.screenshots += 1,
                Err(error) => {
                    self.failure = Some(error);
                }
            }
        }
        queue.present(frame);

        self.present_total_us += draw_started.elapsed().as_micros();
        self.present_samples += 1;

        let frame_us = frame_started.elapsed().as_micros();
        if self.config.benchmark {
            self.benchmark_stages[3] += draw_started.elapsed().as_micros();
        }
        self.work_budget
            .observe(frame_us.min(u128::from(u64::MAX)) as u64);
        self.frame_total_us += frame_us;
        self.frame_max_us = self.frame_max_us.max(frame_us);
        self.frame_count += 1;
        if self.streams_mark != self.streams {
            self.streams_mark = self.streams;
            self.stream_total_us += frame_us;
            self.stream_count += 1;
        }
        if self.rebuilds_mark != self.rebuilds {
            self.rebuilds_mark = self.rebuilds;
            self.rebuild_total_us += frame_us;
            self.rebuild_count += 1;
        }

        self.frames += 1;
        if self.config.benchmark && self.frames.is_multiple_of(300) {
            eprintln!(
                "benchmark_frame={} sample_frames=300 mean_frame_us={}",
                self.frames,
                (self.frame_total_us - self.benchmark_mark_us) / 300
            );
            self.benchmark_mark_us = self.frame_total_us;
            let usage = self.memory.usage();
            eprintln!(
                "benchmark_memory total_bytes={} world_bytes={}",
                usage.total,
                usage.current[torse_types::MemoryClass::World.index()]
            );
            eprintln!(
                "benchmark_stages_us stream={} mesh={} upload={} draw={}",
                self.benchmark_stages[0] / 300,
                self.benchmark_stages[1] / 300,
                self.benchmark_stages[2] / 300,
                self.benchmark_stages[3] / 300
            );
            self.benchmark_stages.fill(0);
        }
        self.fps_frames += 1;
        let title_span = self.fps_origin.elapsed().as_secs_f32();
        if title_span >= 0.5 {
            let fps = self.fps_frames as f32 / title_span;
            if let Some(scene) = self.scene.as_ref() {
                let material = self.materials[self.material].path();
                let status = if self.last_result.is_empty() {
                    "ok".to_owned()
                } else {
                    self.last_result.clone()
                };
                self.current_fps = fps.round() as u32;
                self.debug_lines = vec![
                    format!(
                        "FPS: {} | frame: {:.2} ms",
                        self.current_fps,
                        f64::from(title_span) * 1000.0 / f64::from(self.fps_frames.max(1))
                    ),
                    format!(
                        "sections: {} | meshes: {}",
                        scene.sections().len(),
                        scene.mesh_count()
                    ),
                    format!(
                        "CPU memory: {:.1} MiB",
                        self.memory.usage().total as f64 / (1024.0 * 1024.0)
                    ),
                    format!(
                        "GPU geometry: {:.1} MiB",
                        gpu.context.geometry_usage().bytes as f64 / (1024.0 * 1024.0)
                    ),
                    format!(
                        "GPU sections: {} | visible: {} | evictions: {}",
                        gpu.context.geometry_usage().sections,
                        gpu.context.draw_stats().visible_sections,
                        gpu.context.geometry_usage().evictions
                    ),
                    format!("position: {:.1}, {:.1}, {:.1}", eye[0], eye[1], eye[2]),
                    scene
                        .section_location(eye)
                        .map_or_else(String::new, |(section, local)| {
                            format!(
                                "section: {}, {}, {} | local: {}, {}, {}",
                                section.x, section.y, section.z, local.x, local.y, local.z
                            )
                        }),
                    format!(
                        "mode: {} | material: {material}",
                        if self.walk { "walk" } else { "fly" }
                    ),
                    format!(
                        "edits: {} ok / {} rejected",
                        self.actions_ok, self.actions_rejected
                    ),
                    format!(
                        "network: {} | requests: {}",
                        self.net.is_some(),
                        self.requests_sent
                    ),
                    format!("status: {status}"),
                ];
            }
            self.fps_origin = Instant::now();
            self.fps_frames = 0;
        }

        if let Some(limit) = self.config.frames
            && self.frames >= limit
        {
            if let Some(path) = self.config.screenshot.clone() {
                self.pending_shot = Some(path);
                // Present once more so the screenshot captures a real frame,
                // then stop on the following one.
                if self.frames == limit {
                    gpu.window.request_redraw();
                    return;
                }
            }
            self.finish(event_loop);
            return;
        }
        gpu.window.request_redraw();
    }

    /// Run the fixed logic steps owed to `dt` seconds.
    ///
    /// Streaming and drawing stay outside this call. Modal frame intervals
    /// are discarded while the existing interpolation phase stays frozen.
    fn pump_logic(&mut self, dt: f64, motion: MoveInput) -> Result<(), String> {
        if self.ui.modal() {
            return Ok(());
        }
        let steps = self.presentation.pump_logic(dt);
        if self.walk {
            self.pump_walk(steps, motion)?;
        } else {
            self.pump_fly(steps, motion);
        }
        self.apply_presented_eye();
        Ok(())
    }

    /// Measure real elapsed time; the presentation clock alone caps catch-up.
    fn frame_interval(&mut self, now: Instant) -> f64 {
        self.last_frame
            .replace(now)
            .map_or(0.0, |last| now.duration_since(last).as_secs_f64())
    }

    fn set_ui_mode(&mut self, mode: crate::ui::Mode) {
        self.ui.set_mode(mode);
        // A transition can happen with no redraws during a long pause.
        self.last_frame = None;
    }

    /// Fraction of the current logic step, for read-only pose interpolation.
    fn presentation_alpha(&self) -> f32 {
        self.presentation.presentation_alpha() as f32
    }

    /// Snap the presented eye and drop the interpolation phase.
    ///
    /// Movement correction from the authority is not settled here. Callers use
    /// this when an external pose replaces the one the logic steps produced.
    fn reset_presentation_pose(&mut self, eye: [f32; 3]) {
        self.presentation.reset();
        self.last_frame = None;
        self.walk_blocked = false;
        self.present_prev = eye;
        self.present_next = eye;
        if let Some(camera) = self.camera.as_mut() {
            camera.eye = eye;
        }
    }

    fn apply_presented_eye(&mut self) {
        let alpha = self.presentation_alpha();
        let Some(camera) = self.camera.as_mut() else {
            return;
        };
        for axis in 0..3 {
            camera.eye[axis] = self.present_prev[axis]
                + (self.present_next[axis] - self.present_prev[axis]) * alpha;
        }
    }

    /// Frequent authority confirmations replace the pose without consuming
    /// elapsed logic time; resetting the clock here can starve every walk tick.
    fn correct_network_pose(&mut self, eye: [f32; 3]) {
        self.present_prev = eye;
        self.present_next = eye;
        if let Some(camera) = self.camera.as_mut() {
            camera.eye = eye;
        }
    }

    fn pump_walk(&mut self, steps: u32, motion: MoveInput) -> Result<(), String> {
        if let Some(camera) = self.camera.as_mut() {
            camera.look(motion.yaw_delta, motion.pitch_delta);
        }
        let yaw = f64::from(self.camera.as_ref().map(|camera| camera.yaw).unwrap_or(0.0));
        let forward = [-yaw.sin(), 0.0, -yaw.cos()];
        let right = [yaw.cos(), 0.0, -yaw.sin()];
        let mut wish = [0.0f64; 3];
        for axis in 0..3 {
            wish[axis] =
                forward[axis] * f64::from(motion.forward) + right[axis] * f64::from(motion.strafe);
        }
        let length = (wish[0] * wish[0] + wish[1] * wish[1] + wish[2] * wish[2]).sqrt();
        if length > 0.0 {
            for value in &mut wish {
                *value /= length;
            }
        }
        let step = LOGIC_STEP_SECONDS as f32;
        for _ in 0..steps {
            let jump = self.input.jump_held()
                || ((self.config.autoplay || self.ui.settings.auto_jump) && self.walk_blocked);
            let before = (
                f64::from(self.present_next[0]),
                f64::from(self.present_next[2]),
            );
            let scene = self.scene.as_mut().ok_or("walk mode has no scene")?;
            let result = if let Some(connection) = self.net.as_mut() {
                let quantized = [
                    (wish[0] * 127.0).round() as i8,
                    (wish[2] * 127.0).round() as i8,
                ];
                match connection
                    .move_player(scene.authority(), quantized, jump)
                    .map_err(|e| e.to_string())?
                {
                    Some(position) => Ok((position, false)),
                    None => {
                        self.last_result =
                            "Player position is unavailable in this subscription".to_owned();
                        continue;
                    }
                }
            } else {
                scene.step_walker(wish, jump, step)
            };
            match result {
                Ok((position, _)) => {
                    let wanted = wish[0].abs() + wish[2].abs() > 1e-9;
                    let drifted =
                        (position.x - before.0).abs() + (position.z - before.1).abs() > 0.02;
                    self.walk_blocked = wanted && !drifted;
                    self.walk_ticks += 1;
                    self.present_prev = self.present_next;
                    self.present_next = [
                        position.x as f32,
                        (position.y + WALKER_EYE_OFFSET) as f32,
                        position.z as f32,
                    ];
                }
                Err(reason) => return Err(reason),
            }
        }
        Ok(())
    }

    fn pump_fly(&mut self, steps: u32, mut motion: MoveInput) {
        let yaw_delta = motion.yaw_delta;
        let pitch_delta = motion.pitch_delta;
        motion.yaw_delta = 0.0;
        motion.pitch_delta = 0.0;
        let Some(camera) = self.camera.as_mut() else {
            return;
        };
        camera.look(yaw_delta, pitch_delta);
        let step = LOGIC_STEP_SECONDS as f32;
        for _ in 0..steps {
            camera.eye = self.present_next;
            camera.advance(motion, step);
            self.present_prev = self.present_next;
            self.present_next = camera.eye;
        }
    }

    /// Apply one decoded server event; errors stop the run.
    fn apply_net_event(&mut self, event: crate::net::NetEvent) -> Result<(), String> {
        use crate::net::NetEvent;
        match event {
            NetEvent::Welcomed { session } => {
                self.session = Some(session.0);
            }
            NetEvent::Baseline(_snapshot) => {
                self.invalidate_network_scene();
            }
            NetEvent::Increment(changes) => {
                self.batches += 1;
                if self.rebuild_network {
                    return Ok(());
                }
                let Some(scene) = self.scene.as_mut() else {
                    return Err("increment before baseline".to_owned());
                };
                if !scene.try_apply_changes(&changes)? {
                    self.invalidate_network_scene();
                    return Ok(());
                }
                if let Some(connection) = self.net.as_mut() {
                    connection
                        .refresh_player(scene.authority())
                        .map_err(|error| error.to_string())?;
                    if self.walk
                        && let Some(id) = connection.player_entity()
                    {
                        scene.view_player(id);
                    }
                }
            }
            NetEvent::Player(_update) => {
                if let Some((connection, scene)) = self.net.as_mut().zip(self.scene.as_mut()) {
                    connection
                        .refresh_player(scene.authority())
                        .map_err(|error| error.to_string())?;
                    if self.walk {
                        if let Some(id) = connection.player_entity() {
                            scene.view_player(id);
                        }
                        if let Some(position) = connection.player_position(scene.authority()) {
                            self.correct_network_pose([
                                position.x as f32,
                                (position.y + WALKER_EYE_OFFSET) as f32,
                                position.z as f32,
                            ]);
                        }
                    }
                }
            }
            NetEvent::Admitted { seq, rejected } => {
                if rejected.is_some() {
                    self.requests_rejected += 1;
                }
                self.last_result = match rejected {
                    Some(code) => format!("seq {} rejected {code:?}", seq.0),
                    None => format!("seq {} admitted", seq.0),
                };
            }
            NetEvent::Finished { seq, committed } => {
                if !committed {
                    self.requests_rejected += 1;
                }
                self.last_result = format!(
                    "seq {} {}",
                    seq.0,
                    if committed { "committed" } else { "denied" }
                );
            }
            NetEvent::Closed(reason) => return Err(reason),
        }
        self.resume_requested_walk();
        Ok(())
    }
    fn invalidate_network_scene(&mut self) {
        self.scene = None;
        self.pending_palette = None;
        self.center = None;
        self.uploaded = false;
        self.entity_sequence = None;
        self.pending_uploads.clear();
        self.rebuild_network = true;
    }
    fn rebuild_network_scene(&mut self) -> Result<(), String> {
        let Some(snapshot) = self.net.as_ref().and_then(crate::net::Connection::snapshot) else {
            return Ok(());
        };
        let loaded_top = (snapshot.side as i64).min(snapshot.bounds.max_y);
        let mut scene = match Scene::from_snapshot_charged(snapshot, self.memory.clone()) {
            Ok(scene) => scene,
            Err(error) if error.code == torse_types::FaultCode::ResourceLimit => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        scene.enable_background(crate::scene::background_config())?;
        scene.enable_lighting()?;
        if let Some(connection) = self.net.as_mut() {
            connection
                .refresh_player(scene.authority())
                .map_err(|error| error.to_string())?;
            if self.walk
                && let Some(id) = connection.player_entity()
            {
                scene.view_player(id);
            }
        }
        let position = self
            .net
            .as_ref()
            .filter(|_| self.walk)
            .and_then(|connection| connection.player_position(scene.authority()));
        let eye = position.map_or([4.0, (loaded_top as f32 - 2.0).max(4.0), 4.0], |position| {
            [
                position.x as f32,
                (position.y + WALKER_EYE_OFFSET) as f32,
                position.z as f32,
            ]
        });
        if let Some(camera) = self.camera.as_mut() {
            camera.eye = eye;
            camera.pitch = -1.5;
            camera.yaw = 0.6;
        }
        self.reset_presentation_pose(eye);
        self.pending_palette = Some(scene.palette().clone());
        self.scene = Some(scene);
        self.rebuild_network = false;
        self.resume_requested_walk();
        Ok(())
    }
    fn resume_requested_walk(&mut self) {
        if self.config.walk
            && !self.walk
            && self.net.is_some()
            && self.scene.is_some()
            && let Some(camera) = self.camera.as_ref()
        {
            self.enter_walk(camera.eye);
        }
    }
    /// Capture or release the mouse.
    ///
    /// Hotbar labels for a pack scene; an empty list keeps the packaged icons.
    fn hotbar_names(&self) -> Vec<String> {
        if let Some(scene) = self
            .scene
            .as_ref()
            .filter(|scene| scene.item_owner().is_some())
        {
            return scene
                .inventory_slots()
                .iter()
                .take(9)
                .map(|stack| {
                    stack
                        .as_ref()
                        .map_or_else(String::new, |stack| stack.item.path().to_owned())
                })
                .collect();
        }
        if !self.pack {
            return Vec::new();
        }
        self.materials
            .iter()
            .take(3)
            .map(|key| key.path().to_owned())
            .collect()
    }

    fn set_grab(&mut self, grab: bool) {
        // Never capture while the window is unfocused; the next focus event
        // or the per-frame check re-arms capture.
        let grab = grab && self.focused && !self.config.benchmark;
        if let Some(gpu) = self.gpu.as_ref() {
            self.grabbed = if grab && gpu.window.has_focus() && !self.ui.modal() {
                // On Windows Locked pins the current desktop position, even
                // outside this window. Hidden Confined instead clips to the
                // client center and keeps both button routing and raw motion.
                gpu.window.set_cursor_visible(false);
                let result = gpu.window.set_cursor_grab(CursorGrabMode::Confined);
                #[cfg(not(target_os = "windows"))]
                let result = result.or_else(|_| {
                    gpu.window.set_cursor_grab(CursorGrabMode::Locked)?;
                    gpu.window
                        .set_cursor_position(winit::dpi::PhysicalPosition::new(
                            gpu.config.width / 2,
                            gpu.config.height / 2,
                        ))
                });
                if let Err(error) = &result {
                    let _ = gpu.window.set_cursor_grab(CursorGrabMode::None);
                    self.last_result = format!("Mouse capture failed: {error}");
                }
                result.is_ok()
            } else {
                let _ = gpu.window.set_cursor_grab(CursorGrabMode::None);
                false
            };
            gpu.window.set_cursor_visible(!self.grabbed);
        }
    }

    fn captured_motion(&mut self, delta: (f64, f64)) {
        if self.grabbed && self.focused && !self.ui.modal() {
            self.input.mouse_look(
                delta.0 * self.look_sensitivity,
                delta.1 * self.look_sensitivity,
            );
        }
    }

    /// Switch to walk mode by spawning the player entity under the camera.
    ///
    /// Preserve eye height when clear. The scene can raise a near-floor
    /// candidate within a bounded search before creating any entity.
    fn enter_walk(&mut self, eye: [f32; 3]) {
        if let Some((connection, scene)) = self.net.as_ref().zip(self.scene.as_mut()) {
            if let Some(position) = connection.player_position(scene.authority()) {
                if let Some(id) = connection.player_entity() {
                    scene.view_player(id);
                }
                self.walk = true;
                self.config.walk = false;
                self.reset_presentation_pose([
                    position.x as f32,
                    (position.y + WALKER_EYE_OFFSET) as f32,
                    position.z as f32,
                ]);
            } else {
                self.last_result = "Player position is unavailable in this subscription".to_owned();
            }
            return;
        }
        let under = Position::new(
            f64::from(eye[0]),
            f64::from(eye[1]) - WALKER_EYE_OFFSET,
            f64::from(eye[2]),
        );
        let under = match self.scene.as_mut() {
            Some(scene) => match scene.spawn_walker(under) {
                Ok(position) => position,
                Err(reason) => {
                    self.last_result = reason;
                    return;
                }
            },
            None => return,
        };
        self.walk = true;
        self.config.walk = false;
        // Seed the interpolation endpoints with the spawn position so the
        // first frames do not glide in from the origin.
        let start = [
            under.x as f32,
            (under.y + WALKER_EYE_OFFSET) as f32,
            under.z as f32,
        ];
        self.reset_presentation_pose(start);
    }

    /// F5 flips modes; only the server retains a body while flying.
    fn toggle_walk(&mut self) {
        if self.walk {
            if let Some(scene) = self.scene.as_mut()
                && let Err(error) = scene.stop_walker()
            {
                self.last_result = error;
            }
            self.walk = false;
            if let Some(eye) = self.camera.as_ref().map(|camera| camera.eye) {
                self.reset_presentation_pose(eye);
            }
            return;
        }
        if let Some(camera) = self.camera.as_ref() {
            self.enter_walk(camera.eye);
        }
    }

    fn ui_action(&mut self, action: crate::ui::Action, event_loop: &ActiveEventLoop) {
        if self.settings_action(action) {
            self.apply_preferences();
            self.save_preferences();
            return;
        }
        match action {
            crate::ui::Action::Slot(slot) => {
                self.inventory_slot_clicked(slot);
                return;
            }
            crate::ui::Action::ToggleFrameLimit => {
                self.ui.settings.frame_limit.enabled = !self.ui.settings.frame_limit.enabled;
                self.frame_pacer.reset();
            }
            crate::ui::Action::FrameRate(fps) => {
                self.ui.settings.frame_limit.fps = fps;
                self.frame_pacer.reset();
            }
            crate::ui::Action::Quit => self.finish(event_loop),
            crate::ui::Action::Resume => self.set_ui_mode(crate::ui::Mode::Game),
            crate::ui::Action::Preview => self.set_ui_mode(crate::ui::Mode::MachinePreview),
            crate::ui::Action::SensDown => {
                self.look_sensitivity = (self.look_sensitivity - 0.1).clamp(0.2, 5.0);
                self.last_result = format!("sens {:.2}", self.look_sensitivity);
                self.ui.settings.sensitivity = (self.look_sensitivity * 100.0).round() as u16;
            }
            crate::ui::Action::SensUp => {
                self.look_sensitivity = (self.look_sensitivity + 0.1).clamp(0.2, 5.0);
                self.last_result = format!("sens {:.2}", self.look_sensitivity);
                self.ui.settings.sensitivity = (self.look_sensitivity * 100.0).round() as u16;
            }
            _ => {}
        }
        self.save_preferences();
    }

    fn finish(&mut self, event_loop: &ActiveEventLoop) {
        self.flush_preferences(true);
        if let Some((scene, gpu)) = self.scene.as_ref().zip(self.gpu.as_ref()) {
            let (vertices, faces) = scene.geometry_counts();
            self.summary = Some(WindowSummary {
                geometry: gpu.context.geometry_usage(),
                draws: gpu.context.draw_stats(),
                memory: self.memory.usage(),
                cpu: scene.background_stats(),
                adapter: self.adapter.clone(),
                backend: gpu.backend_name.clone(),
                format: format!("{:?}", gpu.config.format),
                present: gpu.present_mode.clone(),
                frames: self.frames,
                streams: self.streams,
                rebuilds: self.rebuilds,
                uploads: self.uploads,
                screenshots: self.screenshots,
                actions_ok: self.actions_ok,
                actions_rejected: self.actions_rejected,
                session: self.session,
                batches: self.batches,
                requests_sent: self.requests_sent,
                requests_rejected: self.requests_rejected,
                baseline_sections: self
                    .scene
                    .as_ref()
                    .map_or(0, |scene| scene.sections().len()),
                walk_ticks: self.walk_ticks,
                walker: self
                    .scene
                    .as_ref()
                    .and_then(Scene::walker_position)
                    .map(|position| [position.x, position.y, position.z]),
                sections: scene.sections().len(),
                meshes: scene.mesh_count(),
                vertices,
                faces,
                elapsed_ms: self
                    .started
                    .map(|start| start.elapsed().as_millis())
                    .unwrap_or_default(),
                mean_frame_us: if self.frame_count == 0 {
                    0
                } else {
                    u64::try_from(self.frame_total_us / u128::from(self.frame_count)).unwrap_or(0)
                },
                max_frame_us: u64::try_from(self.frame_max_us).unwrap_or(0),
                stream_frame_us: if self.stream_count == 0 {
                    0
                } else {
                    u64::try_from(self.stream_total_us / u128::from(self.stream_count)).unwrap_or(0)
                },
                rebuild_frame_us: if self.rebuild_count == 0 {
                    0
                } else {
                    u64::try_from(self.rebuild_total_us / u128::from(self.rebuild_count))
                        .unwrap_or(0)
                },
                cpu_us: mean_us(self.cpu_total_us, self.cpu_samples),
                upload_us: mean_us(self.upload_total_us, self.upload_samples),
                present_us: mean_us(self.present_total_us, self.present_samples),
            });
        }
        event_loop.exit();
    }
}

impl ApplicationHandler for WindowApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let _startup = match self.memory.reserve(
            torse_types::MemoryClass::Codec,
            64 * 1024 * 1024,
            torse_types::EvictionRank::Pinned,
        ) {
            Ok(lease) => lease,
            Err(error) => {
                self.failure = Some(error.to_string());
                event_loop.exit();
                return;
            }
        };
        let attributes = Window::default_attributes()
            .with_title("Torse client")
            .with_min_inner_size(LogicalSize::new(640, 360))
            .with_inner_size(LogicalSize::new(self.config.width, self.config.height));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.failure = Some(format!("window creation failed: {error}"));
                event_loop.exit();
                return;
            }
        };

        let (session, surface) = match torse_render::open_for_window(|instance| {
            instance
                .create_surface(window.clone())
                .map_err(|error| format!("{error}"))
        }) {
            Ok(opened) => opened,
            Err(error) => {
                self.failure = Some(error);
                event_loop.exit();
                return;
            }
        };
        let info = session.info();
        let size = window.inner_size();
        let mut config = match surface.get_default_config(
            &session.adapter,
            size.width.max(1),
            size.height.max(1),
        ) {
            Some(config) => config,
            None => {
                self.failure = Some("surface has no supported configuration".to_owned());
                event_loop.exit();
                return;
            }
        };
        // Screenshots read back from the presented texture.
        config.usage |= TextureUsages::COPY_SRC;
        let capabilities = surface.get_capabilities(&session.adapter);
        let present =
            match torse_render::apply_present_mode(&mut config, &capabilities.present_modes) {
                Ok(mode) => mode,
                Err(error) => {
                    self.failure = Some(error);
                    event_loop.exit();
                    return;
                }
            };
        let present = if self.ui.settings.vsync
            && capabilities
                .present_modes
                .contains(&wgpu::PresentMode::Fifo)
        {
            config.present_mode = wgpu::PresentMode::Fifo;
            wgpu::PresentMode::Fifo
        } else if capabilities
            .present_modes
            .contains(&wgpu::PresentMode::Immediate)
        {
            config.present_mode = wgpu::PresentMode::Immediate;
            wgpu::PresentMode::Immediate
        } else {
            present
        };
        surface.configure(&session.device, &config);
        let depth = create_depth(&session.device, config.width, config.height);
        let depth_view = depth.create_view(&TextureViewDescriptor::default());
        let mut context = GpuContext::new(session.device, session.queue, config.format);
        context.set_geometry_limit(self.config.geometry_mib * 1024 * 1024);
        context.attach_memory(self.memory.clone());
        eprintln!("{}", torse_render::startup_line(&info, Some(present)));

        let scene = if self.config.connect.is_some() {
            // Network scenes arrive with the first baseline; the window stays
            // empty until then.
            None
        } else {
            // A pack scene generates on the foreground thread; the built-in
            // fixture scene keeps the bounded background pool.
            let world_memory =
                match Scene::startup_world_reservation(&self.memory, self.config.radius) {
                    Ok(memory) => memory,
                    Err(error) => {
                        self.failure = Some(error.to_string());
                        event_loop.exit();
                        return;
                    }
                };
            let built = match self.config.content.as_deref() {
                Some(directory) => {
                    Scene::new_game(self.config.radius, std::path::Path::new(directory))
                }
                None => Scene::new_background(self.config.radius),
            };
            match built {
                Ok(mut scene) => {
                    if let Err(error) =
                        scene.attach_memory_prepared(self.memory.clone(), world_memory)
                    {
                        self.failure = Some(error.to_string());
                        event_loop.exit();
                        return;
                    }
                    if let Err(error) = scene.enable_lighting() {
                        self.failure = Some(error);
                        event_loop.exit();
                        return;
                    }
                    Some(scene)
                }
                Err(error) => {
                    self.failure = Some(error);
                    event_loop.exit();
                    return;
                }
            }
        };
        let mut camera = FlyCamera::new([8.0, 26.0, 8.0]);
        if let Some(scene) = scene.as_ref() {
            camera = FlyCamera::new(scene.eye());
            if self.config.content.is_some() {
                // A pack scene is a landscape overview: stand above the spawn
                // column and look across the terrain instead of into it.
                camera.eye[1] = (camera.eye[1] + 12.0).min(56.0);
                camera.eye[2] = (camera.eye[2] - 26.0).max(2.0);
                camera.pitch = -0.28;
                camera.yaw = 0.0;
            } else {
                // Only the y = 0 section layer is loaded, and picking terminates
                // at the first unloaded cell. Keep the starting eye inside that
                // layer so the aim works no matter how high the spawn surface is.
                camera.eye[1] = camera.eye[1].min(14.0);
            }
        } else {
            // Overlook the subscribed scope until the baseline lands.
            camera.pitch = -0.7;
            camera.yaw = 0.6;
        }
        if let Some(address) = self.config.connect.clone() {
            let compatibility = match Scene::compatibility() {
                Ok(value) => value,
                Err(error) => {
                    self.failure = Some(error);
                    event_loop.exit();
                    return;
                }
            };
            let mut connection: crate::net::Connection<torse_net::TcpTransport> =
                match crate::net::Connection::connect(
                    &address,
                    self.config.tls.as_ref(),
                    &self.memory,
                ) {
                    Ok(connection) => connection,
                    Err(error) => {
                        self.failure = Some(format!("connect failed: {error}"));
                        event_loop.exit();
                        return;
                    }
                };
            let handshake = connection
                .attach_memory(self.memory.clone())
                .and_then(|()| connection.hello(compatibility))
                .and_then(|()| {
                    connection.subscribe(
                        self.config
                            .sections
                            .clone()
                            .unwrap_or_else(|| demo_scope().to_vec()),
                    )
                });
            if let Err(error) = handshake {
                self.failure = Some(format!("handshake failed: {error}"));
                event_loop.exit();
                return;
            }
            self.net = Some(connection);
        }
        self.adapter = info.name;
        let ui = {
            let (device, _) = context.parts();
            match crate::ui::Layer::new(device, config.format) {
                Ok(mut ui) => {
                    if let Err(error) = ui.attach_memory(self.memory.clone()) {
                        self.failure = Some(error);
                        event_loop.exit();
                        return;
                    }
                    ui
                }
                Err(error) => {
                    self.failure = Some(error);
                    event_loop.exit();
                    return;
                }
            }
        };
        self.gpu = Some(Gpu {
            present_modes: capabilities.present_modes.clone(),
            ui,
            backend_name: format!("{:?}", info.backend),
            present_mode: format!("{present:?}"),
            context,
            surface,
            config,
            depth,
            depth_view,
            window,
        });
        self.scene = scene;
        if let Some(scene) = self.scene.as_mut().filter(|scene| !scene.is_network())
            && let Err(error) = scene.enable_items(None)
        {
            self.failure = Some(error);
            event_loop.exit();
            return;
        }
        if let Some(notice) = self.scene.as_ref().and_then(Scene::startup_notice) {
            self.last_result = notice.to_owned();
        }
        if let Some(keys) = self.scene.as_ref().and_then(Scene::placeable_blocks) {
            self.materials = keys.iter().take(3).cloned().collect();
            self.material = 0;
            self.pack = true;
        }
        self.pending_palette = self.scene.as_ref().map(|scene| scene.palette().clone());
        self.present_prev = camera.eye;
        self.present_next = camera.eye;
        self.camera = Some(camera);
        // Enter walk mode only after the scene exists; enter_walk spawns the
        // walker through it.
        if self.config.walk {
            let eye = self.camera.as_ref().expect("camera was just installed").eye;
            self.enter_walk(eye);
        }
        self.look_sensitivity = self.config.sensitivity;
        self.apply_preferences();
        self.started = Some(Instant::now());
        self.fps_origin = Instant::now();
        // Capture the mouse right away; Esc opens the pause menu with the
        // real cursor back.
        self.set_grab(true);
        // Prime one frame immediately instead of waiting for the first event.
        if let Some(gpu) = self.gpu.as_ref() {
            gpu.window.request_redraw();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        if self.config.benchmark
            && matches!(
                event,
                WindowEvent::KeyboardInput { .. }
                    | WindowEvent::MouseInput { .. }
                    | WindowEvent::CursorMoved { .. }
                    | WindowEvent::MouseWheel { .. }
            )
        {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                self.finish(event_loop);
            }
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut()
                    && size.width > 0
                    && size.height > 0
                {
                    gpu.config.width = size.width;
                    gpu.config.height = size.height;
                    let (device, _) = gpu.context.parts();
                    gpu.surface.configure(device, &gpu.config);
                    gpu.depth = create_depth(device, size.width, size.height);
                    gpu.depth_view = gpu.depth.create_view(&TextureViewDescriptor::default());
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                use winit::keyboard::KeyCode;
                if event.state == ElementState::Pressed
                    && let winit::keyboard::PhysicalKey::Code(code) = event.physical_key
                {
                    if !event.repeat && self.rebind_key(code) {
                        return;
                    }
                    if code == self.ui.settings.bindings.inventory && !event.repeat {
                        let mode = if self.ui.mode == crate::ui::Mode::Inventory {
                            crate::ui::Mode::Game
                        } else {
                            crate::ui::Mode::Inventory
                        };
                        self.set_ui_mode(mode);
                        self.input.reset();
                        self.set_grab(!self.ui.modal());
                        return;
                    }
                    if !self.ui.modal() && !event.repeat {
                        if code == self.ui.settings.bindings.debug {
                            self.toggle_debug();
                            return;
                        }
                        if code == self.ui.settings.bindings.walk {
                            self.toggle_walk();
                            return;
                        }
                    }
                    if code == self.ui.settings.bindings.screenshot && !event.repeat {
                        self.input.key(event.physical_key, event.state);
                        return;
                    }
                    match code {
                        KeyCode::Escape if !event.repeat => {
                            let mode = if self.ui.modal() {
                                crate::ui::Mode::Game
                            } else {
                                crate::ui::Mode::Pause
                            };
                            self.set_ui_mode(mode);
                            self.input.reset();
                            self.set_grab(!self.ui.modal());
                            return;
                        }
                        KeyCode::KeyE if !event.repeat => {
                            let mode = if self.ui.modal() {
                                crate::ui::Mode::Game
                            } else {
                                crate::ui::Mode::MachinePreview
                            };
                            self.set_ui_mode(mode);
                            self.input.reset();
                            self.set_grab(!self.ui.modal());
                            return;
                        }
                        _ if self.ui.modal() => return,
                        KeyCode::Digit1 => self.material = 0,
                        KeyCode::Digit2 => self.material = 1,
                        KeyCode::Digit3 => self.material = 2,
                        KeyCode::Digit4 if self.materials.len() > 3 => self.material = 3,
                        KeyCode::Digit5 if self.materials.len() > 4 => self.material = 4,
                        KeyCode::Digit6 if self.materials.len() > 5 => self.material = 5,
                        KeyCode::Digit7 if self.materials.len() > 6 => self.material = 6,
                        KeyCode::Digit8 if self.materials.len() > 7 => self.material = 7,
                        KeyCode::Digit9 if self.materials.len() > 8 => self.material = 8,
                        _ => {}
                    }
                }
                if !self.ui.modal() {
                    self.input.key(event.physical_key, event.state);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if self.ui.modal() {
                    if button == MouseButton::Left {
                        let size = self
                            .gpu
                            .as_ref()
                            .map_or([960, 540], |g| [g.config.width, g.config.height]);
                        if let Some(action) = self.ui.pointer(state == ElementState::Pressed, size)
                        {
                            self.ui_action(action, event_loop);
                            self.input.reset();
                            self.set_grab(!self.ui.modal());
                        }
                    }
                } else if self.grabbed {
                    // Captured mouse: a press is a click, motion is look.
                    if state == ElementState::Pressed {
                        self.input.queue_click(button);
                    }
                } else {
                    self.input.mouse(button, state);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let size = self
                    .gpu
                    .as_ref()
                    .map_or([960, 540], |gpu| [gpu.config.width, gpu.config.height]);
                if let Some(action) = self.ui.motion([position.x, position.y], size) {
                    self.ui_action(action, event_loop);
                }
                if !self.grabbed && !self.ui.modal() {
                    self.input.cursor(position);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if !self.ui.modal() && self.walk {
                    // Fly speed is meaningless on foot; the wheel tunes the
                    // look sensitivity instead.
                    let notches = match delta {
                        winit::event::MouseScrollDelta::LineDelta(_, lines) => f64::from(lines),
                        winit::event::MouseScrollDelta::PixelDelta(p) => p.y / 48.0,
                    };
                    if notches.is_finite() && notches != 0.0 {
                        self.look_sensitivity = (self.look_sensitivity
                            * 1.08f64.powf(notches.clamp(-8.0, 8.0)))
                        .clamp(0.2, 5.0);
                        self.last_result = format!("sens {:.2}", self.look_sensitivity);
                        self.ui.settings.sensitivity =
                            (self.look_sensitivity * 100.0).round() as u16;
                        self.save_preferences();
                    }
                } else if !self.ui.modal() {
                    self.input.wheel(delta);
                }
            }
            WindowEvent::Focused(false) => {
                self.focused = false;
                self.ui.set_mode(self.ui.mode);
                self.input.reset();
                self.set_grab(false);
            }
            WindowEvent::Focused(true) => {
                self.focused = true;
                if !self.ui.modal() {
                    self.set_grab(true);
                }
            }
            WindowEvent::RedrawRequested => {
                if self.gpu.is_some()
                    && self.camera.is_some()
                    && self.frame_pacer.ready(Instant::now())
                {
                    self.frame(event_loop);
                }
            }
            _ => {}
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) {
        if self.config.benchmark {
            return;
        }
        if let DeviceEvent::MouseMotion { delta } = event {
            self.captured_motion(delta);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.flush_preferences(false);
        if !self.poll_network(event_loop) {
            return;
        }
        if let Some(gpu) = self.gpu.as_ref() {
            let now = Instant::now();
            if self.ui.settings.frame_limit.enabled {
                let deadline = self.frame_pacer.deadline(now);
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                if now >= deadline {
                    gpu.window.request_redraw();
                }
            } else {
                event_loop.set_control_flow(ControlFlow::Poll);
                gpu.window.request_redraw();
            }
        }
    }
}

/// The demo server's host-approved scope; subscriptions must be a subset.
const fn demo_scope() -> [SectionPos; 2] {
    [SectionPos::new(0, 0, 0), SectionPos::new(1, 0, 0)]
}

/// Translate one hit into the same intent for local and network actions.
fn action_for(
    scene: Option<&Scene>,
    outcome: &PickOutcome,
    button: MouseButton,
    material: &ContentKey,
) -> Option<(bool, BlockPos, u32)> {
    let PickOutcome::Hit(hit) = outcome else {
        return None;
    };
    match button {
        MouseButton::Left => Some((false, hit.block, 0)),
        MouseButton::Right => {
            let placement = scene?.placement_at(hit, material)?;
            Some((true, placement.target, placement.half))
        }
        _ => None,
    }
}

fn create_depth(device: &Device, width: u32, height: u32) -> Texture {
    device.create_texture(&TextureDescriptor {
        label: Some("client window depth"),
        size: Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Depth32Float,
        usage: TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    })
}

/// Read one presented frame back and write it as a PNG.
fn capture_frame(
    device: &Device,
    queue: &Queue,
    frame: &SurfaceTexture,
    width: u32,
    height: u32,
    format: TextureFormat,
    path: &str,
) -> Result<(), String> {
    let bytes_per_pixel = 4;
    let padded_row = (width * bytes_per_pixel).next_multiple_of(256);
    let readback = device.create_buffer(&BufferDescriptor {
        label: Some("client window readback"),
        size: u64::from(padded_row) * u64::from(height),
        usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
        label: Some("client window readback"),
    });
    encoder.copy_texture_to_buffer(
        TexelCopyTextureInfo {
            texture: &frame.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: TextureAspect::All,
        },
        TexelCopyBufferInfo {
            buffer: &readback,
            layout: TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(height),
            },
        },
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    device
        .poll(PollType::wait_indefinitely())
        .map_err(|error| format!("device poll failed: {error}"))?;
    let view = slice
        .get_mapped_range()
        .map_err(|error| format!("map failed: {error}"))?;
    let mut pixels = Vec::with_capacity((width * height * bytes_per_pixel) as usize);
    for row in 0..height {
        let start = (row * padded_row) as usize;
        let end = start + (width * bytes_per_pixel) as usize;
        pixels.extend_from_slice(&view[start..end]);
    }
    drop(view);
    readback.unmap();

    if matches!(
        format,
        TextureFormat::Bgra8UnormSrgb | TextureFormat::Bgra8Unorm
    ) {
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    } else if !matches!(
        format,
        TextureFormat::Rgba8UnormSrgb | TextureFormat::Rgba8Unorm
    ) {
        return Err(format!("unsupported screenshot format {format:?}"));
    }
    crate::renderer::write_png(path, width, height, &pixels)
}

/// Open the interactive window and block until it closes.
///
/// Returns the run summary, or the first failure that stopped the loop.
pub fn run_window(config: WindowConfig) -> Result<WindowSummary, String> {
    let event_loop = EventLoop::new().map_err(|error| format!("event loop failed: {error}"))?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = WindowApp::new(config);
    event_loop
        .run_app(&mut app)
        .map_err(|error| format!("event loop failed: {error}"))?;
    if let Some(failure) = app.failure {
        return Err(failure);
    }
    app.summary
        .ok_or_else(|| "window closed before the first frame".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn key(text: &str) -> ContentKey {
        ContentKey::parse(text).unwrap()
    }

    #[test]
    fn slab_clicks_resolve_and_commit_the_same_local_and_network_arguments() {
        let slab = key("base:glass_slab");
        let pos = BlockPos::new(8, 14, 8);
        for (state_half, origin, direction, held, target, half) in [
            (0, [8.5, 15.9, 8.5], [0.0, -1.0, 0.0], &slab, pos, 1),
            (1, [8.5, 13.8, 8.5], [0.0, 1.0, 0.0], &slab, pos, 0),
            (
                0,
                [8.5, 13.8, 8.5],
                [0.0, 1.0, 0.0],
                &slab,
                BlockPos::new(8, 13, 8),
                1,
            ),
            (
                1,
                [8.5, 15.9, 8.5],
                [0.0, -1.0, 0.0],
                &slab,
                BlockPos::new(8, 15, 8),
                0,
            ),
            (
                0,
                [6.5, 14.25, 8.5],
                [1.0, 0.0, 0.0],
                &slab,
                BlockPos::new(7, 14, 8),
                0,
            ),
            (
                1,
                [6.5, 14.75, 8.5],
                [1.0, 0.0, 0.0],
                &slab,
                BlockPos::new(7, 14, 8),
                1,
            ),
            (
                0,
                [8.5, 15.9, 8.5],
                [0.0, -1.0, 0.0],
                &key("base:stone"),
                BlockPos::new(8, 15, 8),
                0,
            ),
            (
                1,
                [8.5, 13.8, 8.5],
                [0.0, 1.0, 0.0],
                &key("base:stone"),
                BlockPos::new(8, 13, 8),
                1,
            ),
        ] {
            let mut scene = Scene::new(0).unwrap();
            scene.apply_edit(true, &slab, pos, state_half).unwrap();
            let aim = scene.pick(origin, direction, PICK_RANGE);
            let PickOutcome::Hit(hit) = aim else {
                panic!(
                    "slab not hit: half={state_half}, origin={origin:?}, direction={direction:?}, outcome={aim:?}"
                );
            };
            assert_eq!(hit.block, pos);
            let intent = action_for(Some(&scene), &aim, MouseButton::Right, held).unwrap();
            assert_eq!(intent, (true, target, half));
            let call = crate::net::edit_call(intent.0, held, intent.1, intent.2);
            assert_eq!(call.target, torse_sim::action::ActionTarget::Block(target));
            assert_eq!(
                call.parameters.get(&key("world:field/half")),
                Some(&torse_world::ComponentValue::U32(half))
            );
            assert_eq!(
                call.parameters.get(&key("world:field/block")),
                Some(&torse_world::ComponentValue::Content(held.clone()))
            );
            scene
                .apply_edit(intent.0, held, intent.1, intent.2)
                .unwrap();
            let state = scene.authority().store().query(target).ready().unwrap();
            let registry = scene.authority().registry();
            let expected = if target == pos {
                &key("base:glass")
            } else {
                held
            };
            assert_eq!(registry.definition_key(state), Some(expected));
            if target != pos && held == &slab {
                assert_eq!(registry.state_values(state), Some([half as u16].as_slice()));
            }
        }
    }

    #[test]
    fn side_placement_completes_an_adjacent_matching_slab() {
        let mut scene = Scene::new(0).unwrap();
        let stone = key("base:stone");
        let slab = key("base:glass_slab");
        let wall = BlockPos::new(8, 14, 8);
        let target = BlockPos::new(7, 14, 8);
        scene.apply_edit(true, &stone, wall, 0).unwrap();
        scene.apply_edit(true, &slab, target, 0).unwrap();
        let aim = scene.pick([6.5, 14.75, 8.5], [1.0, 0.0, 0.0], PICK_RANGE);
        let intent = action_for(Some(&scene), &aim, MouseButton::Right, &slab).unwrap();
        assert_eq!(intent, (true, target, 1));
        scene
            .apply_edit(intent.0, &slab, intent.1, intent.2)
            .unwrap();
        let state = scene.authority().store().query(target).ready().unwrap();
        assert_eq!(
            scene.authority().registry().definition_key(state),
            Some(&key("base:glass"))
        );
        assert!(action_for(Some(&scene), &PickOutcome::Miss, MouseButton::Right, &slab).is_none());
    }

    #[test]
    fn camera_above_height_limit_can_edit_legal_targets_and_complete_top_slabs() {
        let mut scene = Scene::new(0).unwrap();
        scene.enable_lighting().unwrap();
        let stone = key("base:stone");
        let slab = key("base:glass_slab");
        let max_y = scene.authority().store().bounds().max_y;
        let support = BlockPos::new(8, max_y - 2, 8);
        let top = BlockPos::new(8, max_y - 1, 8);
        let eye = [8.5, max_y as f32 + 1.5, 8.5];
        scene.apply_edit(true, &stone, support, 0).unwrap();
        let aim = scene.pick(eye, [0.0, -1.0, 0.0], PICK_RANGE);
        let intent = action_for(Some(&scene), &aim, MouseButton::Right, &stone).unwrap();
        assert_eq!(intent, (true, top, 0));
        scene
            .apply_edit(intent.0, &stone, intent.1, intent.2)
            .unwrap();

        let aim = scene.pick(eye, [0.0, -1.0, 0.0], PICK_RANGE);
        let intent = action_for(Some(&scene), &aim, MouseButton::Right, &stone).unwrap();
        assert_eq!(intent.1.y, max_y);
        let seq = scene.authority().seq();
        assert!(
            scene
                .apply_edit(intent.0, &stone, intent.1, intent.2)
                .is_err()
        );
        assert_eq!(scene.authority().seq(), seq);
        let intent = action_for(Some(&scene), &aim, MouseButton::Left, &stone).unwrap();
        scene
            .apply_edit(intent.0, &stone, intent.1, intent.2)
            .unwrap();

        scene.apply_edit(true, &slab, top, 0).unwrap();
        let aim = scene.pick(eye, [0.0, -1.0, 0.0], PICK_RANGE);
        let intent = action_for(Some(&scene), &aim, MouseButton::Right, &slab).unwrap();
        assert_eq!(intent, (true, top, 1));
        scene
            .apply_edit(intent.0, &slab, intent.1, intent.2)
            .unwrap();
        let state = scene.authority().store().query(top).ready().unwrap();
        assert_eq!(
            scene.authority().registry().definition_key(state),
            Some(&key("base:glass"))
        );

        assert_eq!(scene.pick(eye, [0.0, -1.0, 0.0], 1.0), PickOutcome::Miss);
        assert_eq!(
            scene.pick(eye, [0.0, 1.0, 0.0], PICK_RANGE),
            PickOutcome::Miss
        );
    }

    pub(super) fn app() -> WindowApp {
        let mut app = WindowApp::new(WindowConfig {
            preferences: crate::settings::Settings::default(),
            settings_tab: crate::settings::Tab::Graphics,
            sections: None,
            tls: None,
            memory_mib: 512,
            geometry_mib: 64,
            radius: 1,
            width: 960,
            height: 540,
            frames: None,
            screenshot: None,
            autoplay: false,
            benchmark: false,
            benchmark_motion: None,
            walk: false,
            connect: None,
            sensitivity: 1.0,
            fps_limit: None,
            probe_outside: false,
            ui_mode: crate::ui::Mode::Game,
            content: None,
        });
        app.camera = Some(FlyCamera::new([8.5, 12.7, 8.5]));
        app.reset_presentation_pose([8.5, 12.7, 8.5]);
        app
    }

    fn forward() -> MoveInput {
        MoveInput {
            forward: 1.0,
            ..MoveInput::default()
        }
    }

    #[test]
    fn captured_raw_motion_accumulates_small_and_large_steps_without_warps() {
        let mut app = app();
        app.grabbed = true;
        app.focused = true;
        app.look_sensitivity = 2.0;
        for _ in 0..100 {
            app.captured_motion((0.1, -0.1));
        }
        app.captured_motion((100.0, -50.0));
        app.captured_motion((f64::NAN, 0.0));
        let motion = app.input.take_input(12.0).0;
        assert!((motion.yaw_delta + 1.1).abs() < 1e-6);
        assert!((motion.pitch_delta - 0.6).abs() < 1e-6);
        assert_eq!(app.input.take_input(12.0).0.yaw_delta, 0.0);
    }

    #[test]
    fn captured_raw_motion_respects_focus_capture_and_modal_state() {
        let mut app = app();
        app.grabbed = true;
        app.focused = false;
        app.captured_motion((10.0, 10.0));
        app.focused = true;
        app.ui.mode = crate::ui::Mode::Pause;
        app.captured_motion((10.0, 10.0));
        app.ui.mode = crate::ui::Mode::Game;
        app.grabbed = false;
        app.captured_motion((10.0, 10.0));
        let motion = app.input.take_input(12.0).0;
        assert_eq!(motion.yaw_delta, 0.0);
        assert_eq!(motion.pitch_delta, 0.0);
    }

    #[test]
    fn frame_intervals_use_wall_time_without_fabrication_or_clamping() {
        let mut app = app();
        let now = Instant::now();
        assert_eq!(app.frame_interval(now), 0.0);
        assert_eq!(app.frame_interval(now), 0.0);
        assert_eq!(app.frame_interval(now + Duration::from_micros(10)), 0.00001);
        let dt = app.frame_interval(now + Duration::from_secs(1) + Duration::from_micros(10));
        assert_eq!(dt, 1.0);
        app.pump_logic(dt, forward()).expect("fly");
        // The single catch-up cap allows eight 50 ms steps after a hitch.
        assert!((app.present_next[2] - (8.5 - 8.0 * 12.0 * 0.05)).abs() < 1e-5);
        assert!(app.presentation_alpha() < 1e-6);
    }

    #[test]
    fn fly_pose_and_look_are_independent_of_frame_rate() {
        for fps in [30, 60, 144, 1000] {
            let mut app = app();
            for frame in 0..fps {
                let motion = MoveInput {
                    yaw_delta: if frame == 0 { 0.5 } else { 0.0 },
                    ..forward()
                };
                app.pump_logic(1.0 / f64::from(fps), motion).expect("fly");
            }
            let camera = app.camera.expect("camera");
            assert_eq!(camera.yaw, 0.5);
            assert!((app.present_next[0] - (8.5 - 12.0 * 0.5f32.sin())).abs() < 1e-4);
            assert!((app.present_next[2] - (8.5 - 12.0 * 0.5f32.cos())).abs() < 1e-4);
            assert!(app.presentation_alpha() < 1e-6);
        }
    }

    #[test]
    fn pause_freezes_the_pose_and_resumes_without_replaying_wall_time() {
        let mut app = app();
        app.pump_logic(0.075, forward()).expect("fly");
        let eye = app.camera.expect("camera").eye;
        let target = app.present_next;
        let alpha = app.presentation_alpha();
        app.set_ui_mode(crate::ui::Mode::Pause);
        app.pump_logic(10.0, forward()).expect("pause");
        assert_eq!(app.camera.expect("camera").eye, eye);
        assert_eq!(app.present_next, target);
        assert_eq!(app.presentation_alpha(), alpha);

        let now = Instant::now();
        app.frame_interval(now);
        app.set_ui_mode(crate::ui::Mode::Game);
        let dt = app.frame_interval(now + Duration::from_secs(30));
        assert_eq!(dt, 0.0);
        app.pump_logic(dt, forward()).expect("resume");
        assert_eq!(app.camera.expect("camera").eye, eye);
        app.pump_logic(0.025, forward()).expect("next tick");
        assert!(app.camera.expect("camera").eye[2] < eye[2]);
    }

    #[test]
    fn correction_replaces_both_endpoints_and_discards_old_time() {
        let mut app = app();
        app.pump_logic(0.075, forward()).expect("fly");
        app.last_frame = Some(Instant::now());
        app.walk_blocked = true;
        let corrected = [100.0, 20.0, 100.0];
        app.reset_presentation_pose(corrected);
        assert_eq!(app.presentation_alpha(), 0.0);
        assert!(app.last_frame.is_none());
        assert!(!app.walk_blocked);
        app.pump_logic(0.025, forward()).expect("partial frame");
        assert_eq!(app.camera.expect("camera").eye, corrected);
        assert_eq!(app.present_prev, corrected);
        assert_eq!(app.present_next, corrected);
        app.pump_logic(0.025, forward()).expect("step");
        assert_eq!(app.present_prev, corrected);
        assert!((app.present_next[2] - 99.4).abs() < 1e-5);
    }

    #[test]
    fn walking_uses_fixed_steps_and_presentation_does_not_move_the_entity() {
        let mut expected = None;
        for fps in [30, 60, 144] {
            let mut app = app();
            app.scene = Some(Scene::new(1).expect("scene"));
            app.enter_walk(app.camera.expect("camera").eye);
            assert!(app.walk);
            for _ in 0..fps {
                app.pump_logic(1.0 / f64::from(fps), forward())
                    .expect("walk");
            }
            assert_eq!(app.walk_ticks, 20);
            let position = app
                .scene
                .as_ref()
                .expect("scene")
                .walker_position()
                .expect("walker");
            if let Some(expected) = expected {
                assert_eq!(position, expected);
            } else {
                expected = Some(position);
            }
            app.apply_presented_eye();
            app.apply_presented_eye();
            app.set_ui_mode(crate::ui::Mode::Pause);
            app.pump_logic(30.0, forward()).expect("pause");
            assert_eq!(app.walk_ticks, 20);
            assert_eq!(
                app.scene.as_ref().expect("scene").walker_position(),
                Some(position)
            );
            // Switching modes drops stale interpolation and autoplay state.
            let eye = app.camera.expect("camera").eye;
            app.toggle_walk();
            assert!(!app.walk);
            assert_eq!(app.present_prev, eye);
            assert_eq!(app.present_next, eye);
            assert_eq!(app.presentation_alpha(), 0.0);
        }
    }

    #[test]
    fn local_mode_changes_release_bodies_and_keep_clear_viewpoints_stable() {
        let mut app = app();
        let mut scene = Scene::new(0).unwrap();
        scene.enable_lighting().unwrap();
        app.scene = Some(scene);
        for _ in 0..12 {
            let eye = [8.5, 66.7, 8.5];
            app.reset_presentation_pose(eye);
            app.toggle_walk();
            assert!(app.walk);
            assert_eq!(app.scene.as_ref().unwrap().authority().entities().len(), 1);
            assert_eq!(app.present_next, eye);
            app.pump_logic(0.05, MoveInput::default()).unwrap();
            assert!(app.scene.as_ref().unwrap().walker_position().unwrap().y < 66.0);
            app.toggle_walk();
            assert!(!app.walk);
            let scene = app.scene.as_ref().unwrap();
            assert_eq!(scene.authority().entities().len(), 0);
            assert!(scene.walker_position().is_none());
            assert!(scene.entity_geometry().0.is_empty());
        }
        app.reset_presentation_pose([8.5, 14.7, 8.5]);
        app.toggle_walk();
        app.toggle_walk();
        // The abandoned body must no longer veto placement at its old cell.
        app.scene
            .as_mut()
            .unwrap()
            .apply_edit(true, &key("base:stone"), BlockPos::new(8, 14, 8), 0)
            .unwrap();
        app.toggle_walk();
        assert!(app.walk);
        assert!(
            app.present_next[1] > 15.5,
            "near-floor entry raises the body clear"
        );
    }

    #[test]
    fn network_walk_does_not_create_a_local_authority() {
        let mut app = app();
        let snapshot = torse_sim::replica::ReplicaSnapshot {
            side: 16,
            bounds: torse_world::WorldBounds::new(0, 32).expect("bounds"),
            states: Vec::new(),
            sections: Vec::new(),
            entities: Vec::new(),
        };
        app.scene = Some(Scene::from_snapshot(&snapshot).expect("replica"));
        app.enter_walk(app.camera.expect("camera").eye);
        assert!(!app.walk);
        app.pump_logic(1.0, forward()).expect("network fly");
        assert_eq!(app.walk_ticks, 0);
        assert!(
            app.scene
                .as_ref()
                .expect("scene")
                .walker_position()
                .is_none()
        );
        assert_eq!(
            app.last_result,
            "network mirrors cannot settle local movement"
        );
    }

    #[test]
    fn frequent_network_confirmations_do_not_reset_elapsed_walk_time() {
        let mut app = app();
        app.scene = Some(Scene::new_background(1).unwrap());
        app.enter_walk(app.camera.unwrap().eye);
        assert!(app.walk);
        let start = Instant::now();
        assert_eq!(app.frame_interval(start), 0.0);
        for frame in 1..=100 {
            app.correct_network_pose(app.present_next);
            let dt = app.frame_interval(start + Duration::from_millis(frame * 10));
            assert_eq!(dt, 0.01);
            app.pump_logic(dt, forward()).unwrap();
        }
        assert_eq!(app.walk_ticks, 20);
    }
}
