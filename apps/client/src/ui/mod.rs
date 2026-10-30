//! Client UI foundation: a flat dark theme drawn on the canvas, HUD, modal
//! focus, and a clearly labeled machine preview. No UI control has access to
//! authoritative mutation.
use torse_ui::{canvas, gpu, style};

use crate::settings::{Binding, OptionId, Settings, SkinChoice, Tab};
use canvas::{Canvas, Rect, Sprite, Text};
use gpu::Overlay;
use std::collections::BTreeMap;
use style::{MUTED, SEPARATOR, TEXT, TITLE_TEXT};
use torse_ui::{Part, Theme};
mod inventory;
mod models;
mod settings;
mod themes;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Game,
    Pause,
    MachinePreview,
    Settings,
    Inventory,
}
impl Mode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "hud" => Ok(Self::Game),
            "pause" => Ok(Self::Pause),
            "machine" => Ok(Self::MachinePreview),
            "settings" => Ok(Self::Settings),
            "inventory" => Ok(Self::Inventory),
            _ => Err("UI preview must be hud, pause, machine, settings or inventory".into()),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Resume,
    Preview,
    Quit,
    SensDown,
    SensUp,
    ToggleFrameLimit,
    FrameRate(u16),
    Settings,
    SettingsBack,
    SettingsTab(Tab),
    Toggle(OptionId),
    Value(OptionId, u16),
    Rebind(Binding),
    ResetSettings,
    Slot(usize),
    Skin(SkinChoice),
    CatalogPage(i32),
}

#[derive(Default)]
pub struct State {
    pub mode: Mode,
    pub cursor: [f64; 2],
    pub settings: Settings,
    pub tab: Tab,
    pub rebinding: Option<Binding>,
    pub inventory_source: Option<usize>,
    pub catalog_page: usize,
    capture: torse_ui::Pointer<Action>,
}
impl State {
    pub fn modal(&self) -> bool {
        self.mode != Mode::Game
    }
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.capture.cancel();
        self.rebinding = None;
        self.inventory_source = None;
    }
    fn event(event: Option<torse_ui::Event<Action>>) -> Option<Action> {
        match event? {
            torse_ui::Event::Activated(action) => Some(action),
            torse_ui::Event::Changed(Action::FrameRate(_), fps) => Some(Action::FrameRate(fps)),
            torse_ui::Event::Changed(Action::Value(option, _), value) => {
                Some(Action::Value(option, value))
            }
            _ => None,
        }
    }
    pub fn pointer(&mut self, down: bool, size: [u32; 2]) -> Option<Action> {
        let layout = Layout::new(size);
        let controls = layout.controls(self.mode, self.tab, self.settings.infinite_supply);
        self.capture.cursor = layout.point(self.cursor);
        Self::event(self.capture.button(down, &controls))
    }
    pub fn motion(&mut self, cursor: [f64; 2], size: [u32; 2]) -> Option<Action> {
        self.cursor = cursor;
        let layout = Layout::new(size);
        Self::event(self.capture.motion(
            layout.point(cursor),
            &layout.controls(self.mode, self.tab, self.settings.infinite_supply),
        ))
    }
}
struct Layout {
    scale: u32,
    width: u32,
    height: u32,
}
impl Layout {
    fn new(size: [u32; 2]) -> Self {
        let scale = (size[0] / 480).min(size[1] / 270).clamp(1, 4);
        Self {
            scale,
            width: size[0].max(1).div_ceil(scale),
            height: size[1].max(1).div_ceil(scale),
        }
    }
    fn panel(&self, mode: Mode) -> Rect {
        let (w, h) = if mode == Mode::Inventory {
            (452, 240)
        } else if mode == Mode::Settings {
            (400, 248)
        } else if mode == Mode::Pause {
            (284, 238)
        } else {
            (324, 140)
        };
        let w = w.min(self.width.saturating_sub(8));
        let h = h.min(self.height.saturating_sub(8));
        Rect::new(
            (self.width - w) as i32 / 2,
            (self.height - h) as i32 / 2,
            w,
            h,
        )
    }
    fn buttons(&self, mode: Mode, tab: Tab) -> Vec<(Action, Rect, &'static str)> {
        let p = self.panel(mode);
        match mode {
            Mode::Game | Mode::MachinePreview | Mode::Inventory => Vec::new(),
            Mode::Settings => self.settings_buttons(tab),
            Mode::Pause => vec![
                (
                    Action::Resume,
                    Rect::new(p.x + 14, p.y + 34, p.w.saturating_sub(28), 22),
                    "menu_resume",
                ),
                (
                    Action::Settings,
                    Rect::new(p.x + 14, p.y + 60, p.w.saturating_sub(28), 22),
                    "menu_settings",
                ),
                (
                    Action::Quit,
                    Rect::new(p.x + 14, p.y + 86, p.w.saturating_sub(28), 22),
                    "menu_quit",
                ),
                (
                    Action::SensDown,
                    Rect::new(p.x + p.w as i32 - 110, p.y + 130, 20, 20),
                    "sens_down",
                ),
                (
                    Action::SensUp,
                    Rect::new(p.x + p.w as i32 - 34, p.y + 130, 20, 20),
                    "sens_up",
                ),
                (
                    Action::ToggleFrameLimit,
                    Rect::new(p.x + 14, p.y + 156, p.w.saturating_sub(28), 20),
                    "menu_frame_limit",
                ),
            ],
        }
    }
    fn sensitivity_field(&self) -> Rect {
        let p = self.panel(Mode::Pause);
        Rect::new(p.x + p.w as i32 - 86, p.y + 130, 48, 20)
    }
    fn point(&self, cursor: [f64; 2]) -> [i32; 2] {
        [
            (cursor[0] / f64::from(self.scale)) as i32,
            (cursor[1] / f64::from(self.scale)) as i32,
        ]
    }
    fn fps_slider(&self) -> Rect {
        let p = self.panel(Mode::Pause);
        Rect::new(p.x + 14, p.y + 196, p.w.saturating_sub(28), 16)
    }
    fn close(&self, mode: Mode) -> Rect {
        let p = self.panel(mode);
        Rect::new(p.x + p.w as i32 - 22, p.y + 3, 18, 16)
    }
    fn controls(&self, mode: Mode, tab: Tab, infinite: bool) -> Vec<torse_ui::Control<Action>> {
        if mode == Mode::Game {
            return Vec::new();
        }
        let mut controls: Vec<_> = self
            .buttons(mode, tab)
            .into_iter()
            .map(|(key, rect, _)| torse_ui::Control::Button { key, rect })
            .collect();
        controls.push(torse_ui::Control::Button {
            key: Action::Resume,
            rect: self.close(mode),
        });
        if mode == Mode::Pause {
            controls.push(torse_ui::Control::Slider {
                key: Action::FrameRate(0),
                rect: self.fps_slider(),
                min: crate::frame_pacing::MIN_FPS,
                max: crate::frame_pacing::MAX_FPS,
            });
        }
        if mode == Mode::Settings {
            controls.extend(
                self.settings_sliders(tab)
                    .into_iter()
                    .map(|(key, rect, min, max)| torse_ui::Control::Slider {
                        key,
                        rect,
                        min,
                        max,
                    }),
            );
        }
        if mode == Mode::Inventory && infinite {
            controls.extend([-1, 1].map(|direction| torse_ui::Control::Button {
                key: Action::CatalogPage(direction),
                rect: self.catalog_button(direction),
            }));
        }
        if mode == Mode::Inventory {
            controls.extend((0..36).map(|slot| torse_ui::Control::Button {
                key: Action::Slot(slot),
                rect: self.inventory_slot(slot, infinite),
            }));
        }
        controls
    }
    fn hit(&self, mode: Mode, cursor: [f64; 2], tab: Tab, infinite: bool) -> Option<Action> {
        if mode == Mode::Game {
            return None;
        }
        let p = [
            (cursor[0] / f64::from(self.scale)) as i32,
            (cursor[1] / f64::from(self.scale)) as i32,
        ];
        if self.close(mode).contains(p) {
            return Some(Action::Resume);
        }
        if mode == Mode::Inventory {
            if infinite
                && let Some(direction) = [-1, 1]
                    .into_iter()
                    .find(|d| self.catalog_button(*d).contains(p))
            {
                return Some(Action::CatalogPage(direction));
            }
            return (0..36)
                .find(|slot| self.inventory_slot(*slot, infinite).contains(p))
                .map(Action::Slot);
        }
        self.buttons(mode, tab)
            .into_iter()
            .find(|(_, rect, _)| rect.contains(p))
            .map(|(a, _, _)| a)
    }
}

#[derive(PartialEq, Eq)]
struct View {
    size: [u32; 2],
    mode: Mode,
    material: usize,
    network: bool,
    ready: bool,
    result: String,
    /// Hotbar labels supplied by the scene; empty keeps the packaged icons.
    names: Vec<String>,
    sensitivity: u16,
    hover: Option<Action>,
    pressed: Option<Action>,
    close_hot: bool,
    frame_limit: crate::frame_pacing::FrameLimit,
    dragging_fps: bool,
    settings: Settings,
    tab: Tab,
    rebinding: Option<Binding>,
    debug_lines: Vec<String>,
    inventory: Vec<Option<torse_inventory::ItemStack>>,
    inventory_source: Option<usize>,
    catalog_page: usize,
    catalog_pages: usize,
    pickup: Option<torse_inventory::ItemStack>,
    bar: Vec<Option<torse_inventory::ItemStack>>,
}

pub struct Layer {
    overlay: Overlay,
    models: models::Models,
    text: Text,
    labels: BTreeMap<&'static str, &'static str>,
    last: Option<View>,
    canvas: Option<Canvas>,
    theme: Theme,
    selected_skin: SkinChoice,
    external_skin: Option<std::path::PathBuf>,
    memory: Option<torse_types::MemoryPool>,
}
pub struct Hud<'a> {
    pub scene: Option<&'a crate::scene::Scene>,
    pub pickup: Option<torse_inventory::ItemStack>,
    pub catalog_pages: usize,
    pub material: usize,
    pub network: bool,
    pub ready: bool,
    pub result: &'a str,
    /// Hotbar labels; empty uses the packaged icon names.
    pub names: Vec<String>,
    /// Captured-look sensitivity, shown and tuned in the pause menu.
    pub sensitivity: f64,
    pub debug_lines: &'a [String],
    pub inventory: Vec<Option<torse_inventory::ItemStack>>,
    pub bar: Vec<Option<torse_inventory::ItemStack>>,
}
impl Layer {
    pub fn attach_memory(&mut self, pool: torse_types::MemoryPool) -> Result<(), String> {
        self.canvas = None;
        self.last = None;
        self.theme.attach_memory(pool.clone())?;
        self.text.attach_memory(pool.clone())?;
        self.models.attach_memory(pool.clone())?;
        self.memory = Some(pool);
        Ok(())
    }
    pub fn clear_caches(&mut self) {
        self.text.clear_cache();
    }
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Result<Self, String> {
        let text = Text::load()?;
        let resource = if text.chinese {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../games/base/assets/locales/zh-Hans.txt"
            ))
        } else {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../games/base/assets/locales/en.txt"
            ))
        };
        let external_skin = std::env::var_os("TORSE_UI_SKIN").map(std::path::PathBuf::from);
        let mut selected_skin = if external_skin.is_some() {
            SkinChoice::External
        } else {
            SkinChoice::Dark
        };
        let theme =
            themes::load(selected_skin, external_skin.as_deref(), None).or_else(|error| {
                eprintln!("UI skin load failed: {error}; using the built-in dark skin");
                selected_skin = SkinChoice::Dark;
                Theme::load_default()
            })?;
        Ok(Self {
            overlay: Overlay::new(device, format),
            models: models::Models::default(),
            text,
            labels: resource
                .lines()
                .filter_map(|line| line.split_once('='))
                .collect(),
            last: None,
            canvas: None,
            memory: None,
            theme,
            selected_skin,
            external_skin,
        })
    }
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        size: [u32; 2],
        state: &State,
        hud: Hud<'_>,
    ) {
        let layout = Layout::new(size);
        if let Some(scene) = hud.scene {
            let keys = hud
                .inventory
                .iter()
                .chain(hud.bar.iter())
                .filter_map(|s| s.as_ref().map(|s| s.item.clone()))
                .chain(["base:stone", "base:glass_slab", "base:plant"].map(torse_items::key));
            if self
                .models
                .prepare(scene, keys, self.memory.as_ref(), 32 * layout.scale)
            {
                self.last = None;
            }
        }
        if let Some(scene) = hud.scene {
            let selected = match layout.hit(
                state.mode,
                state.cursor,
                state.tab,
                state.settings.infinite_supply,
            ) {
                Some(Action::Slot(slot)) => Some(slot),
                _ => state.inventory_source,
            };
            let detail = if state.mode == Mode::Inventory {
                selected
                    .and_then(|slot| hud.inventory.get(slot).and_then(Option::as_ref))
                    .or_else(|| hud.bar.get(hud.material).and_then(Option::as_ref))
                    .map(|stack| stack.item.clone())
            } else {
                None
            };
            if self
                .models
                .prepare_detail(scene, detail, 72 * layout.scale, self.memory.as_ref())
            {
                self.last = None;
            }
        }
        let names = if hud.bar.is_empty() {
            hud.names.into_iter().take(3).collect()
        } else {
            hud.bar
                .iter()
                .take(9)
                .map(|stack| {
                    stack
                        .as_ref()
                        .map_or_else(String::new, |stack| self.item_name(stack))
                })
                .collect()
        };
        let view = View {
            size,
            mode: state.mode,
            material: hud.material.min(if hud.bar.is_empty() { 2 } else { 8 }),
            network: hud.network,
            ready: hud.ready,
            result: hud.result.chars().take(120).collect(),
            names,
            sensitivity: (hud.sensitivity * 100.0).round() as u16,
            hover: layout.hit(
                state.mode,
                state.cursor,
                state.tab,
                state.settings.infinite_supply,
            ),
            pressed: state.capture.active(),
            frame_limit: state.settings.frame_limit,
            dragging_fps: state.capture.dragging(),
            settings: state.settings.clone(),
            tab: state.tab,
            rebinding: state.rebinding,
            inventory: hud.inventory,
            bar: hud.bar,
            inventory_source: state.inventory_source,
            catalog_page: state.catalog_page,
            catalog_pages: hud.catalog_pages,
            pickup: hud.pickup,
            debug_lines: if state.settings.debug {
                hud.debug_lines.to_vec()
            } else {
                Vec::new()
            },
            close_hot: {
                let p = [
                    (state.cursor[0] / f64::from(layout.scale)) as i32,
                    (state.cursor[1] / f64::from(layout.scale)) as i32,
                ];
                state.mode != Mode::Game && layout.close(state.mode).contains(p)
            },
        };

        if self.last.as_ref() == Some(&view) {
            return;
        }
        let mut canvas = match self.canvas.take() {
            Some(mut canvas)
                if canvas.width == layout.width
                    && canvas.height == layout.height
                    && canvas.scale == layout.scale =>
            {
                canvas.clear();
                canvas
            }
            old => {
                drop(old);
                let Ok(canvas) = Canvas::try_new(
                    layout.width,
                    layout.height,
                    layout.scale,
                    self.memory.as_ref(),
                ) else {
                    self.text.clear_cache();
                    return;
                };
                canvas
            }
        };
        self.paint(&mut canvas, &layout, &view);
        // The canvas already matches the surface one to one; the shader
        // must not divide again.
        let uploaded = self.overlay.upload(device, queue, &canvas, 1);
        self.canvas = Some(canvas);
        if uploaded.is_err() {
            return;
        }
        self.last = Some(view);
    }
    pub fn draw(&self, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView) {
        self.overlay.draw(encoder, target);
    }
    fn label(&mut self, canvas: &mut Canvas, key: &str, rect: Rect, size: u16, color: [u8; 4]) {
        let value = self.labels.get(key).copied().unwrap_or("?");
        self.text.draw(canvas, value, rect, size, color);
    }
    /// One locale line centred horizontally inside a region.
    fn centre(&mut self, canvas: &mut Canvas, key: &str, region: Rect, size: u16, color: [u8; 4]) {
        let value = self.labels.get(key).copied().unwrap_or("?");
        let tw = self.text.measure(value, size).min(region.w);
        self.text.draw(
            canvas,
            value,
            Rect::new(
                region.x + (region.w - tw) as i32 / 2,
                region.y,
                tw,
                u32::from(size) + 2,
            ),
            size,
            color,
        );
    }
    fn shadow_label(&mut self, canvas: &mut Canvas, key: &str, rect: Rect, size: u16) {
        let value = self.labels.get(key).copied().unwrap_or("?");
        self.text.draw(
            canvas,
            value,
            Rect::new(rect.x + 1, rect.y + 1, rect.w, rect.h),
            size,
            style::SHADOW,
        );
        self.text.draw(canvas, value, rect, size, TEXT);
    }
    fn paint(&mut self, c: &mut Canvas, l: &Layout, v: &View) {
        self.text.set_scale(u16::try_from(c.scale).unwrap_or(1));
        let w = l.width as i32;
        let h = l.height as i32;
        if v.settings.hud {
            self.paint_hud(c, l, v, w, h);
        }
        self.paint_debug(c, l, v);
        if v.mode == Mode::Game {
            if v.settings.hud {
                self.theme
                    .paint(c, Part::Crosshair, Rect::new(w / 2 - 7, h / 2 - 7, 16, 16));
            }
            return;
        }
        c.fill(Rect::new(0, 0, l.width, l.height), [8, 9, 11, 130]);
        let p = l.panel(v.mode);
        self.theme.paint(c, Part::Window, p);
        self.theme.paint(
            c,
            Part::Title,
            Rect::new(p.x + 1, p.y + 1, p.w.saturating_sub(2), 20),
        );
        let title = if v.mode == Mode::Inventory {
            "inventory_title"
        } else if v.mode == Mode::Settings {
            "menu_settings"
        } else if v.mode == Mode::Pause {
            "menu_title"
        } else {
            "machine_title"
        };
        self.label(
            c,
            title,
            Rect::new(p.x + 12, p.y + 4, p.w.saturating_sub(40), 16),
            12,
            TITLE_TEXT,
        );
        self.theme.paint(
            c,
            if v.close_hot {
                Part::CloseHover
            } else {
                Part::Close
            },
            l.close(v.mode),
        );
        style::hline(c, p.x, p.y + 21, p.w, SEPARATOR);
        if v.mode == Mode::Inventory {
            self.paint_inventory(c, l, v);
        } else if v.mode == Mode::Settings {
            self.paint_settings(c, l, v);
        } else if v.mode == Mode::Pause {
            self.paint_pause(c, l, v, p);
        } else {
            self.paint_machine(c, p);
        }
        for (action, rect, key) in l.buttons(v.mode, v.tab) {
            self.theme.button(
                c,
                rect,
                v.hover == Some(action),
                (v.pressed == Some(action) && v.hover == Some(action))
                    || action == Action::SettingsTab(v.tab)
                    || action == Action::Skin(v.settings.skin),
            );
            if action == Action::ToggleFrameLimit {
                self.theme.checkbox(
                    c,
                    Rect::new(rect.x + 4, rect.y + 3, 14, 14),
                    v.frame_limit.enabled,
                );
            }
            if let Action::Toggle(option) = action {
                self.theme.checkbox(
                    c,
                    Rect::new(rect.x + 4, rect.y + 3, 14, 14),
                    v.settings.checked(option),
                );
            }
            let text = if let Action::Rebind(binding) = action {
                format!(
                    "{}: {}",
                    self.labels.get(key).copied().unwrap_or("?"),
                    if v.rebinding == Some(binding) {
                        "...".to_owned()
                    } else {
                        format!("{:?}", v.settings.bindings.key(binding))
                    }
                )
            } else {
                self.labels.get(key).copied().unwrap_or("?").to_owned()
            };
            let label = text.as_str();
            let tw = self.text.measure(label, 12).min(rect.w.saturating_sub(12));
            let color = if v.hover == Some(action) {
                TITLE_TEXT
            } else {
                TEXT
            };
            self.text.draw(
                c,
                label,
                Rect::new(
                    rect.x + (rect.w - tw) as i32 / 2,
                    rect.y + (rect.h as i32 - 12) / 2,
                    tw,
                    14,
                ),
                12,
                color,
            );
        }
    }
    /// HUD is shared by all modes: hotbar, status chip, result toast, help.
    fn paint_hud(&mut self, c: &mut Canvas, l: &Layout, v: &View, w: i32, h: i32) {
        if v.mode == Mode::Game {
            let count = if v.bar.is_empty() { 3 } else { 9 };
            let slot_x = w / 2 - (count * 34 - 2) / 2;
            let slot_y = h - 46;
            let packaged = v.names.is_empty();
            for i in 0..count as usize {
                let x = slot_x + i as i32 * 34;
                self.theme
                    .slot(c, Rect::new(x, slot_y, 32, 32), i == v.material);
                if !v.bar.is_empty() {
                    self.paint_item(
                        c,
                        Rect::new(x, slot_y, 32, 32),
                        v.bar.get(i).and_then(Option::as_ref),
                        !v.settings.infinite_supply,
                    );
                } else if packaged {
                    let item = torse_items::key(["base:stone", "base:glass_slab", "base:plant"][i]);
                    if let Some(sprite) = self.models.get(&item) {
                        c.image(sprite, Rect::new(x + 1, slot_y + 1, 30, 30));
                    }
                }
                self.text.draw(
                    c,
                    &(i + 1).to_string(),
                    Rect::new(x + 3, slot_y + 2, 8, 8),
                    8,
                    MUTED,
                );
            }
            // The selected item name would collide with the modal panels.
            if v.mode == Mode::Game {
                match v.names.get(v.material) {
                    Some(name) => {
                        let name = name.clone();
                        let width = self.text.measure(&name, 12);
                        self.text.draw(
                            c,
                            &name,
                            Rect::new(w / 2 - width as i32 / 2, slot_y - 18, width, 16),
                            12,
                            TEXT,
                        );
                    }
                    None => {
                        let selected =
                            ["item_stone", "item_glass", "item_plant"][v.material.min(2)];
                        let width = self.text.measure(self.labels[selected], 12);
                        self.shadow_label(
                            c,
                            selected,
                            Rect::new(w / 2 - width as i32 / 2, slot_y - 18, width, 16),
                            12,
                        );
                    }
                }
            }
        }
        if v.mode == Mode::Game {
            let mode = if v.settings.infinite_supply {
                "inventory_supply"
            } else {
                "inventory_finite"
            };
            let width = self
                .text
                .measure(self.labels.get(mode).copied().unwrap_or(""), 10)
                + 16;
            self.theme
                .paint(c, Part::Chip, Rect::new(w - width as i32 - 8, 8, width, 20));
            self.label(
                c,
                mode,
                Rect::new(w - width as i32, 12, width.saturating_sub(8), 14),
                10,
                TEXT,
            );
            if let Some(stack) = &v.pickup {
                let key = format!("{:?}", v.settings.bindings.pickup);
                let key = key.strip_prefix("Key").unwrap_or(&key);
                let prompt = format!(
                    "[{}] {} {} x{}",
                    key,
                    self.labels.get("hud_pickup").copied().unwrap_or(""),
                    self.item_name(stack),
                    stack.count
                );
                let width = (self.text.measure(&prompt, 10) + 16).min(l.width.saturating_sub(24));
                let rect = Rect::new(w / 2 - width as i32 / 2, h / 2 + 20, width, 21);
                self.theme.paint(c, Part::Chip, rect);
                self.text.draw(
                    c,
                    &prompt,
                    Rect::new(rect.x + 8, rect.y + 4, rect.w.saturating_sub(16), 14),
                    10,
                    TITLE_TEXT,
                );
            }
        }
        let info = if !v.network {
            "hud_local"
        } else if v.ready {
            "hud_online"
        } else {
            "hud_connecting"
        };
        let info_w = self.text.measure(self.labels[info], 11) + 12;
        self.theme.paint(c, Part::Chip, Rect::new(8, 8, info_w, 20));
        self.label(c, info, Rect::new(14, 11, info_w, 14), 11, TEXT);
        self.shadow_label(
            c,
            "hud_help",
            Rect::new(10, h - 14, l.width.saturating_sub(20), 13),
            9,
        );
        if !v.result.is_empty() {
            let rw = (self.text.measure(&v.result, 10) + 12).min(l.width.saturating_sub(16));
            self.theme.paint(c, Part::Chip, Rect::new(8, 32, rw, 20));
            self.text.draw(
                c,
                &v.result,
                Rect::new(14, 35, rw.saturating_sub(12), 15),
                10,
                TEXT,
            );
        }
    }
    fn paint_pause(&mut self, c: &mut Canvas, l: &Layout, v: &View, p: Rect) {
        style::hline(c, p.x, p.y + 118, p.w, SEPARATOR);
        self.label(
            c,
            "menu_sensitivity",
            Rect::new(p.x + 14, p.y + 134, 120, 14),
            11,
            TEXT,
        );
        let field = l.sensitivity_field();
        self.theme.paint(c, Part::Field, field);
        let value = format!("{:.2}", f64::from(v.sensitivity) / 100.0);
        let tw = self.text.measure(&value, 11);
        self.text.draw(
            c,
            &value,
            Rect::new(field.x + (field.w - tw) as i32 / 2, field.y + 4, tw, 14),
            11,
            TEXT,
        );
        self.label(
            c,
            "menu_note",
            Rect::new(p.x + 14, p.y + 220, p.w.saturating_sub(28), 12),
            9,
            MUTED,
        );
        let value = format!("{} FPS", v.frame_limit.fps);
        self.text.draw(
            c,
            &value,
            Rect::new(p.x + 14, p.y + 180, p.w.saturating_sub(28), 14),
            11,
            TEXT,
        );
        let track = l.fps_slider();
        self.theme.paint(c, Part::Track, track);
        let fraction = f32::from(v.frame_limit.fps - crate::frame_pacing::MIN_FPS)
            / f32::from(crate::frame_pacing::MAX_FPS - crate::frame_pacing::MIN_FPS);
        let x = track.x + (fraction * track.w.saturating_sub(8) as f32).round() as i32;
        self.theme.button(
            c,
            Rect::new(x, track.y + 1, 8, track.h.saturating_sub(2)),
            v.dragging_fps,
            v.dragging_fps,
        );
    }
    fn paint_machine(&mut self, c: &mut Canvas, p: Rect) {
        self.label(
            c,
            "machine_notice",
            Rect::new(p.x + 14, p.y + 24, p.w.saturating_sub(28), 12),
            9,
            MUTED,
        );
        // Input group with the sample item; output group stays empty. The
        // middle column keeps an eight-pixel gutter to both group frames.
        let groups: [(i32, &str, Option<usize>, &str); 2] = [
            (p.x + 14, "machine_input", Some(0), "item_stone"),
            (p.x + p.w as i32 - 110, "machine_output", None, "empty"),
        ];
        for (gx, label, icon, caption) in groups {
            self.theme
                .paint(c, Part::Group, Rect::new(gx, p.y + 42, 96, 58));
            self.centre(c, label, Rect::new(gx, p.y + 46, 96, 12), 10, TEXT);
            self.theme
                .slot(c, Rect::new(gx + 36, p.y + 58, 24, 24), false);
            if let Some(index) = icon {
                let item = torse_items::key(["base:stone", "base:glass_slab", "base:plant"][index]);
                if let Some(sprite) = self.models.get(&item) {
                    c.image(sprite, Rect::new(gx + 37, p.y + 59, 22, 22));
                }
                self.text
                    .draw(c, "1", Rect::new(gx + 37, p.y + 60, 8, 9), 8, MUTED);
            }
            self.centre(c, caption, Rect::new(gx, p.y + 86, 96, 12), 9, MUTED);
        }
        let mid = Rect::new(p.x + 118, p.y + 44, 88, 44);
        self.centre(
            c,
            "machine_progress",
            Rect::new(mid.x, p.y + 46, mid.w, 12),
            10,
            TEXT,
        );
        self.theme
            .progress(c, Rect::new(mid.x, p.y + 62, mid.w, 8), 0.75);
        let ratio = "3 / 4";
        let tw = self.text.measure(ratio, 10) + 4;
        self.text.draw(
            c,
            ratio,
            Rect::new(mid.x + (mid.w - tw) as i32 / 2, p.y + 74, tw, 12),
            10,
            TEXT,
        );
        self.label(
            c,
            "machine_status",
            Rect::new(p.x + 14, p.y + 118, p.w.saturating_sub(28), 13),
            11,
            TEXT,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "gpu-tests")]
    #[test]
    fn saturated_world_budget_cannot_freeze_a_prepared_menu_canvas() {
        use torse_types::{EvictionRank, MemoryBudget, MemoryClass, MemoryPool};
        let session = torse_render::open_headless().unwrap();
        let pool = MemoryPool::new(MemoryBudget::new(128 * 1024 * 1024));
        let mut layer = Layer::new(&session.device, wgpu::TextureFormat::Rgba8Unorm).unwrap();
        layer.attach_memory(pool.clone()).unwrap();
        let mut state = State::default();
        let hud = || Hud {
            scene: None,
            pickup: None,
            catalog_pages: 1,
            bar: Vec::new(),
            material: 0,
            network: false,
            ready: true,
            result: "",
            names: Vec::new(),
            sensitivity: 1.0,
            debug_lines: &[],
            inventory: Vec::new(),
        };
        state.set_mode(Mode::Pause);
        layer.prepare(&session.device, &session.queue, [640, 360], &state, hud());
        state.set_mode(Mode::Game);
        layer.prepare(&session.device, &session.queue, [640, 360], &state, hud());
        let game = layer.canvas.as_ref().unwrap().as_pixels().to_vec();
        let held = pool
            .reserve(
                MemoryClass::World,
                128 * 1024 * 1024 - pool.usage().total,
                EvictionRank::Pinned,
            )
            .unwrap();
        state.set_mode(Mode::Pause);
        layer.prepare(&session.device, &session.queue, [640, 360], &state, hud());
        assert_eq!(layer.last.as_ref().unwrap().mode, Mode::Pause);
        assert_ne!(layer.canvas.as_ref().unwrap().as_pixels(), game);
        state.set_mode(Mode::Game);
        layer.prepare(&session.device, &session.queue, [640, 360], &state, hud());
        assert_eq!(layer.canvas.as_ref().unwrap().as_pixels(), game);
        drop(held);
        drop(layer);
        assert_eq!(pool.usage().total, 0);
    }
    #[test]
    fn modal_buttons_require_press_and_release_on_the_same_target() {
        let mut state = State::default();
        state.set_mode(Mode::Pause);
        let l = Layout::new([960, 540]);
        let r = l.buttons(Mode::Pause, Tab::Graphics)[0].1;
        state.cursor = [f64::from((r.x + 5) * 2), f64::from((r.y + 5) * 2)];
        assert_eq!(state.pointer(true, [960, 540]), None);
        state.cursor = [0.0, 0.0];
        assert_eq!(state.pointer(false, [960, 540]), None);
        state.cursor = [f64::from((r.x + 5) * 2), f64::from((r.y + 5) * 2)];
        state.pointer(true, [960, 540]);
        assert_eq!(state.pointer(false, [960, 540]), Some(Action::Resume));
    }
    #[test]
    fn scaled_hit_testing_agrees_with_small_and_large_layouts() {
        for size in [[640, 360], [960, 540], [1920, 1080]] {
            let l = Layout::new(size);
            let r = l.close(Mode::MachinePreview);
            let cursor = [
                f64::from((r.x + 10) * l.scale as i32),
                f64::from((r.y + 10) * l.scale as i32),
            ];
            assert_eq!(
                l.hit(Mode::MachinePreview, cursor, Tab::Graphics, false),
                Some(Action::Resume)
            );
            assert_eq!(l.hit(Mode::Game, cursor, Tab::Graphics, false), None);
        }
    }

    #[test]
    fn fps_slider_captures_drag_clamps_endpoints_and_cancels_on_mode_change() {
        for size in [[640, 360], [960, 540], [1920, 1080]] {
            let mut state = State::default();
            state.set_mode(Mode::Pause);
            let layout = Layout::new(size);
            let rect = layout.fps_slider();
            let pointer = |x| {
                [
                    f64::from(x * layout.scale as i32),
                    f64::from((rect.y + 5) * layout.scale as i32),
                ]
            };
            state.cursor = pointer(rect.x);
            assert_eq!(
                state.pointer(false, size),
                None,
                "release alone cannot edit a slider"
            );
            assert_eq!(
                state.pointer(true, size),
                Some(Action::FrameRate(crate::frame_pacing::MIN_FPS))
            );
            assert_eq!(
                state.motion(pointer(rect.x + rect.w as i32 + 100), size),
                Some(Action::FrameRate(crate::frame_pacing::MAX_FPS))
            );
            assert_eq!(
                state.pointer(false, size),
                Some(Action::FrameRate(crate::frame_pacing::MAX_FPS))
            );
            assert_eq!(state.motion(pointer(rect.x), size), None);
            state.pointer(true, size);
            state.set_mode(Mode::Game);
            assert_eq!(state.motion(pointer(rect.x + 30), size), None);
            assert_eq!(state.pointer(true, size), None);
        }
    }

    #[test]
    fn settings_buttons_are_reachable_on_every_tab_and_scale() {
        for size in [[640, 360], [960, 540], [1920, 1080]] {
            let layout = Layout::new(size);
            for tab in [
                Tab::Debug,
                Tab::Gameplay,
                Tab::Graphics,
                Tab::Controls,
                Tab::Interface,
            ] {
                let mut state = State {
                    mode: Mode::Settings,
                    tab,
                    ..State::default()
                };
                for (action, rect, _) in layout.buttons(Mode::Settings, tab) {
                    assert!(layout.panel(Mode::Settings).contains([rect.x, rect.y]));
                    state.cursor = [
                        f64::from((rect.x + 2) * layout.scale as i32),
                        f64::from((rect.y + 2) * layout.scale as i32),
                    ];
                    assert_eq!(state.pointer(true, size), None);
                    assert_eq!(state.pointer(false, size), Some(action));
                }
            }
        }
    }
}
