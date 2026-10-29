//! Settings composition using the shared UI module's controls and skin.
use super::*;

impl Layout {
    pub(super) fn settings_buttons(&self, tab: Tab) -> Vec<(Action, Rect, &'static str)> {
        let p = self.panel(Mode::Settings);
        let width = p.w.saturating_sub(24) / 5;
        let mut buttons: Vec<_> = [
            (Tab::Debug, "settings_debug"),
            (Tab::Gameplay, "settings_gameplay"),
            (Tab::Graphics, "settings_graphics"),
            (Tab::Controls, "settings_controls"),
            (Tab::Interface, "settings_ui"),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (tab, label))| {
            (
                Action::SettingsTab(tab),
                Rect::new(
                    p.x + 12 + index as i32 * width as i32,
                    p.y + 30,
                    width.saturating_sub(3),
                    22,
                ),
                label,
            )
        })
        .collect();
        let mut row = |index: i32, action, label| {
            buttons.push((
                action,
                Rect::new(p.x + 12, p.y + 62 + index * 24, p.w.saturating_sub(24), 20),
                label,
            ))
        };
        match tab {
            Tab::Debug => {
                row(0, Action::Toggle(OptionId::Debug), "settings_debug_overlay");
                row(1, Action::Toggle(OptionId::Hud), "settings_hud");
                row(2, Action::Toggle(OptionId::Outline), "settings_outline");
                row(3, Action::Toggle(OptionId::Entities), "settings_entities");
                row(
                    4,
                    Action::Toggle(OptionId::SectionBorders),
                    "settings_section_borders",
                );
                row(5, Action::Preview, "menu_preview");
            }
            Tab::Gameplay => {
                row(0, Action::Toggle(OptionId::InvertY), "settings_invert_y");
                row(1, Action::Toggle(OptionId::AutoJump), "settings_auto_jump");
            }
            Tab::Graphics => {
                row(0, Action::Toggle(OptionId::Vsync), "settings_vsync");
                row(1, Action::ToggleFrameLimit, "menu_frame_limit");
            }
            Tab::Interface => {
                row(0, Action::Skin(SkinChoice::Dark), "skin_dark");
                row(1, Action::Skin(SkinChoice::Blue), "skin_blue");
                row(2, Action::Skin(SkinChoice::External), "skin_external");
            }
            Tab::Controls => {
                let column = p.w.saturating_sub(28) / 2;
                for (index, binding) in crate::settings::BINDINGS.into_iter().enumerate() {
                    let label = match binding {
                        Binding::Forward => "bind_forward",
                        Binding::Back => "bind_back",
                        Binding::Left => "bind_left",
                        Binding::Right => "bind_right",
                        Binding::Jump => "bind_jump",
                        Binding::Descend => "bind_descend",
                        Binding::Walk => "bind_walk",
                        Binding::Screenshot => "bind_screenshot",
                        Binding::Debug => "bind_debug",
                        Binding::Inventory => "bind_inventory",
                    };
                    buttons.push((
                        Action::Rebind(binding),
                        Rect::new(
                            p.x + 12 + (index % 2) as i32 * (column + 4) as i32,
                            p.y + 62 + (index / 2) as i32 * 26,
                            column,
                            22,
                        ),
                        label,
                    ));
                }
            }
        }
        buttons.push((
            Action::SettingsBack,
            Rect::new(p.x + 12, p.y + p.h as i32 - 28, 88, 20),
            "settings_back",
        ));
        buttons.push((
            Action::ResetSettings,
            Rect::new(p.x + p.w as i32 - 132, p.y + p.h as i32 - 28, 120, 20),
            "settings_reset",
        ));
        buttons
    }
    pub(super) fn settings_sliders(&self, tab: Tab) -> Vec<(Action, Rect, u16, u16)> {
        let p = self.panel(Mode::Settings);
        let rect = |y| Rect::new(p.x + 12, p.y + y, p.w.saturating_sub(24), 12);
        match tab {
            Tab::Graphics => vec![
                (Action::FrameRate(0), rect(124), 30, 1000),
                (Action::Value(OptionId::Fov, 0), rect(158), 40, 110),
                (Action::Value(OptionId::Radius, 0), rect(192), 0, 16),
            ],
            Tab::Gameplay => vec![
                (Action::Value(OptionId::FlySpeed, 0), rect(134), 1, 300),
                (Action::Value(OptionId::Sensitivity, 0), rect(174), 20, 500),
            ],
            _ => Vec::new(),
        }
    }
}

impl Layer {
    pub(super) fn paint_settings(&mut self, canvas: &mut Canvas, layout: &Layout, view: &View) {
        for (action, rect, min, max) in layout.settings_sliders(view.tab) {
            let (key, value) = match action {
                Action::FrameRate(_) => ("settings_frame_rate", view.settings.frame_limit.fps),
                Action::Value(option, _) => (
                    match option {
                        OptionId::Fov => "settings_fov",
                        OptionId::Radius => "settings_radius",
                        OptionId::FlySpeed => "settings_fly_speed",
                        _ => "menu_sensitivity",
                    },
                    view.settings.value(option),
                ),
                _ => continue,
            };
            let text = format!(
                "{}: {}",
                self.labels.get(key).copied().unwrap_or("?"),
                value
            );
            self.text.draw(
                canvas,
                &text,
                Rect::new(rect.x, rect.y - 15, rect.w, 14),
                11,
                TEXT,
            );
            self.theme.paint(canvas, Part::Track, rect);
            let fraction =
                f32::from(value.saturating_sub(min)) / f32::from(max.saturating_sub(min).max(1));
            let x = rect.x + (fraction * rect.w.saturating_sub(8) as f32).round() as i32;
            self.theme.paint(
                canvas,
                if view.pressed == Some(action) {
                    Part::ThumbHover
                } else {
                    Part::Thumb
                },
                Rect::new(x, rect.y + 1, 8, rect.h.saturating_sub(2)),
            );
        }
        if matches!(view.tab, Tab::Controls | Tab::Interface) {
            let p = layout.panel(Mode::Settings);
            let hint = if !view.result.is_empty() {
                view.result.as_str()
            } else {
                self.labels
                    .get(if view.tab == Tab::Controls {
                        "bind_hint"
                    } else {
                        "skin_hint"
                    })
                    .copied()
                    .unwrap_or("?")
            };
            self.text.draw(
                canvas,
                hint,
                Rect::new(p.x + 12, p.y + 194, p.w.saturating_sub(24), 18),
                10,
                MUTED,
            );
        }
    }
    pub(super) fn paint_debug(&mut self, canvas: &mut Canvas, layout: &Layout, view: &View) {
        if view.debug_lines.is_empty() {
            return;
        }
        let widths: Vec<_> = view
            .debug_lines
            .iter()
            .take(12)
            .map(|line| self.text.measure(line, 10))
            .collect();
        let Some((rect, count)) = layout.debug_panel(&widths) else {
            return;
        };
        self.theme.paint(canvas, Part::Chip, rect);
        for (index, line) in view.debug_lines.iter().take(count).enumerate() {
            self.text.draw(
                canvas,
                line,
                Rect::new(
                    rect.x + 6,
                    rect.y + 5 + index as i32 * 13,
                    rect.w.saturating_sub(12),
                    13,
                ),
                10,
                TEXT,
            );
        }
    }
}

impl Layout {
    fn debug_panel(&self, widths: &[u32]) -> Option<(Rect, usize)> {
        let count = widths
            .len()
            .min(12)
            .min(self.height.saturating_sub(106) as usize / 13);
        let width = widths
            .iter()
            .take(count)
            .max()?
            .saturating_add(12)
            .min(self.width.saturating_sub(16));
        (count != 0 && width > 12)
            .then_some((Rect::new(8, 56, width, count as u32 * 13 + 10), count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn debug_background_tracks_visible_text_and_stays_on_the_window_edge() {
        let layout = Layout::new([960, 540]);
        let (short, count) = layout.debug_panel(&[30, 60, 42]).unwrap();
        assert_eq!(
            (short.x, short.y, short.w, short.h, count),
            (8, 56, 72, 49, 3)
        );
        let (long, _) = layout.debug_panel(&[30, 160, 42]).unwrap();
        assert_eq!(long.w, 172);
        let (clamped, count) = layout.debug_panel(&[10000; 20]).unwrap();
        assert!(clamped.x + clamped.w as i32 <= layout.width as i32 - 8);
        assert!(clamped.y + clamped.h as i32 <= layout.height as i32 - 40);
        assert!(count <= 12);
        assert!(Layout::new([80, 80]).debug_panel(&[20; 3]).is_none());
    }
}
