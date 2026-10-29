//! Transactional skin replacement, independent of world authority.
use super::*;
use std::path::Path;
use torse_types::MemoryPool;

pub(super) fn load(
    choice: SkinChoice,
    external: Option<&Path>,
    memory: Option<&MemoryPool>,
) -> Result<Theme, String> {
    let mut theme = match choice {
        SkinChoice::Dark => Theme::load_default()?,
        SkinChoice::Blue => Theme::load_blue()?,
        SkinChoice::External => Theme::load_dir(external.ok_or("no external UI skin configured")?)?,
    };
    if let Some(pool) = memory {
        theme.attach_memory(pool.clone())?;
    }
    Ok(theme)
}

impl Layer {
    pub fn skin_choice(&self) -> SkinChoice {
        self.selected_skin
    }
    pub fn select_skin(&mut self, choice: SkinChoice) -> Result<(), String> {
        if choice == self.selected_skin {
            return Ok(());
        }
        let theme = load(choice, self.external_skin.as_deref(), self.memory.as_ref())?;
        self.theme = theme;
        self.selected_skin = choice;
        self.last = None;
        Ok(())
    }
}

#[cfg(all(test, feature = "gpu-tests"))]
mod tests {
    use super::*;
    use torse_types::{EvictionRank, MemoryBudget, MemoryClass};

    fn pixels(layer: &Layer) -> Vec<u8> {
        let mut canvas = Canvas::new(40, 24, 1);
        layer
            .theme
            .paint(&mut canvas, Part::Window, Rect::new(0, 0, 40, 24));
        layer
            .theme
            .paint(&mut canvas, Part::Title, Rect::new(1, 1, 38, 20));
        canvas.as_pixels().to_vec()
    }

    #[test]
    fn skin_pointer_actions_persist_and_failed_replacements_keep_the_current_theme() {
        let session = torse_render::open_headless().unwrap();
        let pool = MemoryPool::new(MemoryBudget::new(128 * 1024 * 1024));
        let mut layer = Layer::new(&session.device, wgpu::TextureFormat::Rgba8Unorm).unwrap();
        layer.external_skin = None;
        layer.select_skin(SkinChoice::Dark).unwrap();
        layer.attach_memory(pool.clone()).unwrap();
        let dark = pixels(&layer);
        for size in [[640, 360], [960, 540], [1920, 1080]] {
            let mut state = State {
                mode: Mode::Settings,
                tab: Tab::Interface,
                ..State::default()
            };
            let layout = Layout::new(size);
            let (_, rect, _) = layout
                .settings_buttons(Tab::Interface)
                .into_iter()
                .find(|(action, _, _)| *action == Action::Skin(SkinChoice::Blue))
                .unwrap();
            state.cursor = [
                f64::from((rect.x + 2) * layout.scale as i32),
                f64::from((rect.y + 2) * layout.scale as i32),
            ];
            assert_eq!(state.pointer(true, size), None);
            let Some(Action::Skin(choice)) = state.pointer(false, size) else {
                panic!("skin button did not activate")
            };
            layer.select_skin(choice).unwrap();
            state.settings.skin = choice;
            let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
                "../../.work/skin-restart-{}-{}.toml",
                std::process::id(),
                size[0]
            ));
            state.settings.save(&path).unwrap();
            let restored = Settings::load(&path).unwrap();
            layer.select_skin(SkinChoice::Dark).unwrap();
            layer.select_skin(restored.skin).unwrap();
            assert_eq!(layer.skin_choice(), SkinChoice::Blue);
            assert_ne!(pixels(&layer), dark);
            std::fs::remove_file(path).unwrap();
        }
        let blue = pixels(&layer);
        let used = pool.usage().total;
        assert!(layer.select_skin(SkinChoice::External).is_err());
        assert_eq!(layer.skin_choice(), SkinChoice::Blue);
        assert_eq!(pixels(&layer), blue);
        assert_eq!(pool.usage().total, used);
        let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../.work/skin-external-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("default.toml"), "invalid manifest").unwrap();
        std::fs::write(
            directory.join("default.png"),
            include_bytes!("../../../../modules/ui/assets/blue.png"),
        )
        .unwrap();
        layer.external_skin = Some(directory.clone());
        assert!(layer.select_skin(SkinChoice::External).is_err());
        assert_eq!(pixels(&layer), blue);
        std::fs::write(
            directory.join("default.toml"),
            include_str!("../../../../modules/ui/assets/blue.toml"),
        )
        .unwrap();
        layer.select_skin(SkinChoice::External).unwrap();
        assert_eq!(layer.skin_choice(), SkinChoice::External);
        assert_eq!(pixels(&layer), blue);
        layer.select_skin(SkinChoice::Blue).unwrap();
        let held = pool
            .reserve(
                MemoryClass::Resource,
                128 * 1024 * 1024 - used,
                EvictionRank::Pinned,
            )
            .unwrap();
        assert!(layer.select_skin(SkinChoice::Dark).is_err());
        assert_eq!(pixels(&layer), blue);
        drop(held);
        let state = State::default();
        layer.prepare(
            &session.device,
            &session.queue,
            [640, 360],
            &state,
            Hud {
                scene: None,
                bar: Vec::new(),
                material: 0,
                network: false,
                ready: true,
                result: "",
                names: Vec::new(),
                sensitivity: 1.0,
                debug_lines: &[],
                inventory: Vec::new(),
            },
        );
        assert!(layer.last.is_some());
        layer.select_skin(SkinChoice::Dark).unwrap();
        assert!(layer.last.is_none());
        assert_eq!(pixels(&layer), dark);
        drop(layer);
        assert_eq!(pool.usage().total, 0);
        std::fs::remove_file(directory.join("default.toml")).unwrap();
        std::fs::remove_file(directory.join("default.png")).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
