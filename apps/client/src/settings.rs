//! Versioned local preferences, separate from world authority and networking.
use crate::frame_pacing::{FrameLimit, MAX_FPS, MIN_FPS};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use winit::keyboard::KeyCode;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Tab {
    Debug,
    Gameplay,
    #[default]
    Graphics,
    Controls,
    Interface,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkinChoice {
    #[default]
    Dark,
    Blue,
    External,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionId {
    Debug,
    Hud,
    Outline,
    SectionBorders,
    InvertY,
    AutoJump,
    AutoPickup,
    InfiniteSupply,
    Entities,
    Vsync,
    Fov,
    Radius,
    FlySpeed,
    Sensitivity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Binding {
    Forward,
    Back,
    Left,
    Right,
    Jump,
    Descend,
    Walk,
    Screenshot,
    Debug,
    Inventory,
    Pickup,
    Drop,
}
pub const BINDINGS: [Binding; 12] = [
    Binding::Forward,
    Binding::Back,
    Binding::Left,
    Binding::Right,
    Binding::Jump,
    Binding::Descend,
    Binding::Walk,
    Binding::Screenshot,
    Binding::Debug,
    Binding::Inventory,
    Binding::Pickup,
    Binding::Drop,
];
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bindings {
    pub forward: KeyCode,
    pub back: KeyCode,
    pub left: KeyCode,
    pub right: KeyCode,
    pub jump: KeyCode,
    pub descend: KeyCode,
    pub walk: KeyCode,
    pub screenshot: KeyCode,
    pub debug: KeyCode,
    pub inventory: KeyCode,
    pub pickup: KeyCode,
    pub drop: KeyCode,
}
impl Default for Bindings {
    fn default() -> Self {
        Self {
            forward: KeyCode::KeyW,
            back: KeyCode::KeyS,
            left: KeyCode::KeyA,
            right: KeyCode::KeyD,
            jump: KeyCode::Space,
            descend: KeyCode::ShiftLeft,
            walk: KeyCode::F5,
            screenshot: KeyCode::F12,
            debug: KeyCode::F8,
            inventory: KeyCode::Tab,
            pickup: KeyCode::KeyF,
            drop: KeyCode::KeyQ,
        }
    }
}
impl Bindings {
    pub fn key(&self, binding: Binding) -> KeyCode {
        match binding {
            Binding::Forward => self.forward,
            Binding::Back => self.back,
            Binding::Left => self.left,
            Binding::Right => self.right,
            Binding::Jump => self.jump,
            Binding::Descend => self.descend,
            Binding::Walk => self.walk,
            Binding::Screenshot => self.screenshot,
            Binding::Debug => self.debug,
            Binding::Inventory => self.inventory,
            Binding::Pickup => self.pickup,
            Binding::Drop => self.drop,
        }
    }
    pub fn rebind(&mut self, binding: Binding, key: KeyCode) -> Result<(), String> {
        if matches!(
            key,
            KeyCode::Escape
                | KeyCode::KeyE
                | KeyCode::Digit1
                | KeyCode::Digit2
                | KeyCode::Digit3
                | KeyCode::Digit4
                | KeyCode::Digit5
                | KeyCode::Digit6
                | KeyCode::Digit7
                | KeyCode::Digit8
                | KeyCode::Digit9
        ) || BINDINGS
            .iter()
            .any(|other| *other != binding && self.key(*other) == key)
        {
            return Err("key is reserved or already bound".into());
        }
        let target = match binding {
            Binding::Forward => &mut self.forward,
            Binding::Back => &mut self.back,
            Binding::Left => &mut self.left,
            Binding::Right => &mut self.right,
            Binding::Jump => &mut self.jump,
            Binding::Descend => &mut self.descend,
            Binding::Walk => &mut self.walk,
            Binding::Screenshot => &mut self.screenshot,
            Binding::Debug => &mut self.debug,
            Binding::Inventory => &mut self.inventory,
            Binding::Pickup => &mut self.pickup,
            Binding::Drop => &mut self.drop,
        };
        *target = key;
        Ok(())
    }
    fn validate(&self) -> Result<(), String> {
        for binding in BINDINGS {
            let mut copy = self.clone();
            copy.rebind(binding, self.key(binding))?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub version: u16,
    pub frame_limit: FrameLimit,
    pub debug: bool,
    pub hud: bool,
    pub outline: bool,
    pub section_borders: bool,
    pub invert_y: bool,
    pub auto_jump: bool,
    pub auto_pickup: bool,
    pub infinite_supply: bool,
    pub supply_slots: Vec<String>,
    pub entities: bool,
    pub vsync: bool,
    pub fov: u16,
    pub radius: u16,
    pub fly_speed: u16,
    pub sensitivity: u16,
    pub bindings: Bindings,
    pub skin: SkinChoice,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 2,
            frame_limit: FrameLimit::default(),
            debug: false,
            hud: true,
            outline: true,
            section_borders: false,
            invert_y: false,
            auto_jump: false,
            auto_pickup: false,
            infinite_supply: false,
            supply_slots: vec![],
            entities: true,
            vsync: false,
            fov: 55,
            radius: 4,
            fly_speed: 12,
            sensitivity: 100,
            bindings: Bindings::default(),
            skin: SkinChoice::default(),
        }
    }
}
impl Settings {
    pub fn path() -> PathBuf {
        std::env::var_os("TORSE_CLIENT_SETTINGS")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".work/client-settings.toml"))
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 2
            || !(MIN_FPS..=MAX_FPS).contains(&self.frame_limit.fps)
            || !(40..=110).contains(&self.fov)
            || self.supply_slots.len() > 9
            || self.supply_slots.iter().any(|id| id.len() > 256)
            || self.radius > 16
            || !(1..=300).contains(&self.fly_speed)
            || !(20..=500).contains(&self.sensitivity)
        {
            return Err("client settings version or value is invalid".into());
        }
        self.bindings.validate()
    }
    pub fn load(path: &Path) -> Result<Self, String> {
        use std::io::Read;
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.to_string()),
        };
        let mut text = String::new();
        file.take(64 * 1024 + 1)
            .read_to_string(&mut text)
            .map_err(|error| error.to_string())?;
        if text.len() > 64 * 1024 {
            return Err("client settings exceed the source budget".into());
        }
        let mut settings: Self = toml::from_str(&text).map_err(|error| error.to_string())?;
        if settings.version == 1 {
            let legacy = Bindings {
                debug: KeyCode::F3,
                ..Bindings::default()
            };
            // Only an entirely unchanged legacy layout is a known old default.
            // Preserve customized layouts rather than guessing user intent.
            if settings.bindings == legacy {
                settings.bindings = Bindings::default();
            }
            settings.version = 2;
        }
        let input: toml::Value = toml::from_str(&text).map_err(|error| error.to_string())?;
        for (name, binding) in [("pickup", Binding::Pickup), ("drop", Binding::Drop)] {
            if input
                .get("bindings")
                .and_then(|table| table.get(name))
                .is_none()
            {
                let current = settings.bindings.key(binding);
                if settings.bindings.rebind(binding, current).is_err() {
                    let free = [
                        KeyCode::KeyF,
                        KeyCode::KeyQ,
                        KeyCode::KeyG,
                        KeyCode::KeyR,
                        KeyCode::KeyT,
                        KeyCode::KeyY,
                        KeyCode::KeyU,
                        KeyCode::KeyI,
                        KeyCode::KeyO,
                        KeyCode::KeyP,
                        KeyCode::F6,
                        KeyCode::F7,
                        KeyCode::F9,
                        KeyCode::F10,
                    ]
                    .into_iter()
                    .find(|candidate| settings.bindings.rebind(binding, *candidate).is_ok());
                    if free.is_none() {
                        return Err("no free default item binding".into());
                    }
                }
            }
        }
        for slot in &mut settings.supply_slots {
            if slot == "base:vertical_glass_slab" {
                *slot = "base:glass_slab".to_owned();
            }
        }
        settings.validate()?;
        Ok(settings)
    }
    pub fn save(&self, path: &Path) -> Result<(), String> {
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        self.validate()?;
        let text = toml::to_string_pretty(self).map_err(|error| error.to_string())?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let temporary = path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        let result = (|| -> Result<(), std::io::Error> {
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.map_err(|error| error.to_string())
    }
    pub fn toggle(&mut self, option: OptionId) {
        let target = match option {
            OptionId::Debug => &mut self.debug,
            OptionId::Hud => &mut self.hud,
            OptionId::Outline => &mut self.outline,
            OptionId::SectionBorders => &mut self.section_borders,
            OptionId::InvertY => &mut self.invert_y,
            OptionId::AutoJump => &mut self.auto_jump,
            OptionId::AutoPickup => &mut self.auto_pickup,
            OptionId::InfiniteSupply => &mut self.infinite_supply,
            OptionId::Entities => &mut self.entities,
            OptionId::Vsync => &mut self.vsync,
            _ => return,
        };
        *target = !*target;
    }
    pub fn checked(&self, option: OptionId) -> bool {
        match option {
            OptionId::Debug => self.debug,
            OptionId::Hud => self.hud,
            OptionId::Outline => self.outline,
            OptionId::SectionBorders => self.section_borders,
            OptionId::InvertY => self.invert_y,
            OptionId::AutoJump => self.auto_jump,
            OptionId::AutoPickup => self.auto_pickup,
            OptionId::InfiniteSupply => self.infinite_supply,
            OptionId::Entities => self.entities,
            OptionId::Vsync => self.vsync,
            _ => false,
        }
    }
    pub fn value(&self, option: OptionId) -> u16 {
        match option {
            OptionId::Fov => self.fov,
            OptionId::Radius => self.radius,
            OptionId::FlySpeed => self.fly_speed,
            OptionId::Sensitivity => self.sensitivity,
            _ => 0,
        }
    }
    pub fn set(&mut self, option: OptionId, value: u16) {
        match option {
            OptionId::Fov => self.fov = value.clamp(40, 110),
            OptionId::Radius => self.radius = value.min(16),
            OptionId::FlySpeed => self.fly_speed = value.clamp(1, 300),
            OptionId::Sensitivity => self.sensitivity = value.clamp(20, 500),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn former_vertical_supply_slots_resolve_to_the_unified_slab_item() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../.work/settings-slab-{}.toml",
            std::process::id()
        ));
        let settings = Settings {
            supply_slots: vec!["base:vertical_glass_slab".into(), "base:stone".into()],
            ..Settings::default()
        };
        settings.save(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(loaded.supply_slots, vec!["base:glass_slab", "base:stone"]);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn preferences_round_trip_reject_conflicts_and_out_of_range_values() {
        let mut settings = Settings::default();
        settings
            .bindings
            .rebind(Binding::Forward, KeyCode::KeyZ)
            .unwrap();
        assert!(
            settings
                .bindings
                .rebind(Binding::Back, KeyCode::KeyZ)
                .is_err()
        );
        assert!(
            settings
                .bindings
                .rebind(Binding::Back, KeyCode::Escape)
                .is_err()
        );
        let decoded: Settings =
            toml::from_str(&toml::to_string_pretty(&settings).unwrap()).unwrap();
        assert_eq!(decoded, settings);
        assert!(decoded.validate().is_ok());
        settings.version = 3;
        assert!(settings.validate().is_err());
        settings.version = 2;
        settings.fov = 0;
        assert!(settings.validate().is_err());
    }

    #[test]
    fn preference_replacement_is_complete_and_rejected_values_preserve_previous_bytes() {
        assert_eq!(
            toml::from_str::<Settings>("version=2").unwrap().skin,
            SkinChoice::Dark
        );
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.work");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("settings-roundtrip-{}.toml", std::process::id()));
        let mut settings = Settings::default();
        settings.save(&path).unwrap();
        settings.fov = 90;
        settings.skin = SkinChoice::Blue;
        settings.section_borders = true;
        settings
            .bindings
            .rebind(Binding::Forward, KeyCode::KeyZ)
            .unwrap();
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), settings);
        let before = std::fs::read(&path).unwrap();
        settings.fov = 1;
        assert!(settings.save(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn legacy_default_binding_moves_to_f8_without_overwriting_custom_layouts() {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.work");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("settings-legacy-{}.toml", std::process::id()));
        let mut legacy = Settings {
            version: 1,
            ..Settings::default()
        };
        legacy.bindings.debug = KeyCode::F3;
        std::fs::write(&path, toml::to_string_pretty(&legacy).unwrap()).unwrap();
        let current = Settings::load(&path).unwrap();
        assert_eq!(current.version, 2);
        assert_eq!(current.bindings.debug, KeyCode::F8);
        legacy.bindings.forward = KeyCode::KeyZ;
        legacy.bindings.screenshot = KeyCode::F8;
        std::fs::write(&path, toml::to_string_pretty(&legacy).unwrap()).unwrap();
        let current = Settings::load(&path).unwrap();
        assert_eq!(current.bindings, legacy.bindings);
        current.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), current);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn adding_item_bindings_preserves_old_custom_keys() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../.work/settings-item-migration-{}.toml",
            std::process::id()
        ));
        let old = Settings {
            bindings: Bindings {
                forward: KeyCode::KeyF,
                back: KeyCode::KeyQ,
                ..Bindings::default()
            },
            ..Settings::default()
        };
        let mut source = toml::Value::try_from(&old).unwrap();
        let bindings = source.get_mut("bindings").unwrap().as_table_mut().unwrap();
        bindings.remove("pickup");
        bindings.remove("drop");
        std::fs::write(&path, toml::to_string(&source).unwrap()).unwrap();
        let migrated = Settings::load(&path).unwrap();
        assert_eq!(migrated.bindings.forward, KeyCode::KeyF);
        assert_eq!(migrated.bindings.back, KeyCode::KeyQ);
        assert_ne!(migrated.bindings.pickup, KeyCode::KeyF);
        assert_ne!(migrated.bindings.drop, KeyCode::KeyQ);
        assert_ne!(migrated.bindings.pickup, migrated.bindings.drop);
        migrated.validate().unwrap();
        std::fs::remove_file(path).unwrap();
    }
}
