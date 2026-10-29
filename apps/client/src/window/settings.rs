//! Apply local preferences without changing world authority.
use super::*;
use crate::settings::{OptionId, Settings};

impl WindowApp {
    pub(super) fn settings_action(&mut self, action: crate::ui::Action) -> bool {
        use crate::ui::Action;
        match action {
            Action::Settings => self.set_ui_mode(crate::ui::Mode::Settings),
            Action::SettingsBack => self.set_ui_mode(crate::ui::Mode::Pause),
            Action::SettingsTab(tab) => {
                self.ui.tab = tab;
                self.ui.rebinding = None;
            }
            Action::Toggle(option) => {
                self.ui.settings.toggle(option);
                if option == OptionId::InfiniteSupply {
                    self.configure_supply();
                }
            }
            Action::Value(option, value) => self.ui.settings.set(option, value),
            Action::Rebind(binding) => self.ui.rebinding = Some(binding),
            Action::Skin(choice) => {
                self.ui.settings.skin = choice;
                self.last_result.clear();
            }
            Action::ResetSettings => {
                self.ui.settings = Settings::default();
                self.ui.rebinding = None;
            }
            _ => return false,
        }
        true
    }
    pub(super) fn configure_supply(&mut self) {
        let Some(owner) = self.scene.as_ref().and_then(Scene::item_owner) else {
            return;
        };
        let desired = if self.ui.settings.infinite_supply {
            torse_items::supply::SupplyMode::Infinite
        } else {
            torse_items::supply::SupplyMode::Finite
        };
        if self
            .scene
            .as_ref()
            .is_some_and(|scene| scene.supply_mode() != desired)
        {
            self.submit_item_call(torse_items::supply::call(owner, desired));
        }
    }
    pub(super) fn apply_preferences(&mut self) {
        if self.net.is_none() {
            self.configure_supply();
        }
        self.look_sensitivity = f64::from(self.ui.settings.sensitivity) / 100.0;
        self.input.reset();
        self.input.bindings = self.ui.settings.bindings.clone();
        self.input.invert_y = self.ui.settings.invert_y;
        self.frame_pacer.reset();
        self.entity_sequence = None;
        if let Some(camera) = self.camera.as_mut() {
            camera.fov_y_deg = f32::from(self.ui.settings.fov);
            camera.speed = f32::from(self.ui.settings.fly_speed);
            camera.far = f32::from(self.ui.settings.radius + 2) * 32.0;
        }
        if let Some(scene) = self.scene.as_mut() {
            scene.set_view_radius(i64::from(self.ui.settings.radius));
        }
        if let Some(gpu) = self.gpu.as_mut() {
            if let Err(error) = gpu.ui.select_skin(self.ui.settings.skin) {
                self.ui.settings.skin = gpu.ui.skin_choice();
                self.last_result = format!("UI skin load failed: {error}");
            }
            let selected = if self.ui.settings.vsync {
                wgpu::PresentMode::Fifo
            } else if gpu.present_modes.contains(&wgpu::PresentMode::Immediate) {
                wgpu::PresentMode::Immediate
            } else if gpu.present_modes.contains(&wgpu::PresentMode::Mailbox) {
                wgpu::PresentMode::Mailbox
            } else {
                wgpu::PresentMode::Fifo
            };
            if selected != gpu.config.present_mode && gpu.present_modes.contains(&selected) {
                gpu.config.present_mode = selected;
                gpu.present_mode = format!("{selected:?}");
                let (device, _) = gpu.context.parts();
                gpu.surface.configure(device, &gpu.config);
            }
        }
    }
    pub(super) fn save_preferences(&mut self) {
        self.preferences_dirty = Some(Instant::now());
    }
    pub(super) fn flush_preferences(&mut self, force: bool) {
        let Some(changed) = self.preferences_dirty else {
            return;
        };
        if !force && changed.elapsed() < std::time::Duration::from_millis(250) {
            return;
        }
        self.preferences_dirty = None;
        if let Err(error) = self.ui.settings.save(&Settings::path()) {
            self.last_result = format!("settings save failed: {error}");
        }
    }
    pub(super) fn rebind_key(&mut self, key: winit::keyboard::KeyCode) -> bool {
        let Some(binding) = self.ui.rebinding.take() else {
            return false;
        };
        if key == winit::keyboard::KeyCode::Escape {
            return true;
        }
        match self.ui.settings.bindings.rebind(binding, key) {
            Ok(()) => {
                self.apply_preferences();
                self.save_preferences();
                self.last_result.clear();
            }
            Err(error) => self.last_result = error,
        }
        true
    }
    pub(super) fn toggle_debug(&mut self) {
        self.ui.settings.toggle(OptionId::Debug);
        self.save_preferences();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preferences_change_camera_and_input_without_settling_world_actions() {
        let mut app = super::super::tests::app();
        app.scene = Some(Scene::new(0).unwrap());
        let before = app.scene.as_ref().unwrap().authority().seq();
        app.ui.settings.fov = 90;
        app.ui.settings.radius = 1;
        app.ui.settings.sensitivity = 200;
        app.ui.settings.fly_speed = 30;
        app.ui.settings.invert_y = true;
        app.ui
            .settings
            .bindings
            .rebind(
                crate::settings::Binding::Forward,
                winit::keyboard::KeyCode::KeyZ,
            )
            .unwrap();
        app.apply_preferences();
        assert_eq!(app.camera.unwrap().fov_y_deg, 90.0);
        assert_eq!(app.camera.unwrap().speed, 30.0);
        assert_eq!(app.look_sensitivity, 2.0);
        assert!(app.input.invert_y);
        assert_eq!(app.input.bindings.forward, winit::keyboard::KeyCode::KeyZ);
        let scene = app.scene.as_mut().unwrap();
        scene.stream_around(SectionPos::ZERO, 9).unwrap();
        assert_eq!(scene.sections().len(), 9);
        assert_eq!(scene.authority().seq(), before);
    }

    #[test]
    fn skin_action_replaces_an_old_error_and_reset_restores_the_default() {
        let mut app = super::super::tests::app();
        app.last_result = "previous skin load failed".into();
        assert!(app.settings_action(crate::ui::Action::Skin(crate::settings::SkinChoice::Blue)));
        assert_eq!(app.ui.settings.skin, crate::settings::SkinChoice::Blue);
        assert!(app.last_result.is_empty());
        assert!(app.settings_action(crate::ui::Action::ResetSettings));
        assert_eq!(app.ui.settings.skin, crate::settings::SkinChoice::Dark);
    }
}
