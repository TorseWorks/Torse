use super::*;
impl Layout {
    pub(super) fn inventory_slot(&self, slot: usize) -> Rect {
        let p = self.panel(Mode::Inventory);
        Rect::new(
            p.x + (p.w as i32 - 268) / 2 + (slot % 9) as i32 * 30,
            p.y + 54 + (slot / 9) as i32 * 32,
            28,
            28,
        )
    }
}

impl Layer {
    pub(super) fn item_name(&self, stack: &torse_inventory::ItemStack) -> String {
        let label = match stack.item.to_string().as_str() {
            "base:stone" => Some("item_stone"),
            "base:glass_slab" => Some("item_glass"),
            "base:plant" => Some("item_plant"),
            _ => None,
        };
        label
            .and_then(|label| self.labels.get(label).copied())
            .unwrap_or(stack.item.path())
            .to_owned()
    }
    pub(super) fn paint_item(
        &mut self,
        c: &mut Canvas,
        rect: Rect,
        stack: Option<&torse_inventory::ItemStack>,
    ) {
        let Some(stack) = stack else {
            return;
        };
        let icon = match stack.item.to_string().as_str() {
            "base:stone" => Some(0),
            "base:glass_slab" => Some(1),
            "base:plant" => Some(2),
            _ => None,
        };
        if let Some(icon) = icon {
            c.sprite(
                &self.skin.icons[icon],
                Rect::new(rect.x + 4, rect.y + 4, 16, 16),
                0,
            );
        } else {
            self.text.draw(
                c,
                stack.item.path(),
                Rect::new(rect.x + 2, rect.y + 4, rect.w.saturating_sub(4), 12),
                8,
                TEXT,
            );
        }
        let count = stack.count.to_string();
        let width = self.text.measure(&count, 9);
        self.text.draw(
            c,
            &count,
            Rect::new(
                rect.x + rect.w as i32 - width as i32 - 2,
                rect.y + rect.h as i32 - 11,
                width,
                11,
            ),
            9,
            TITLE_TEXT,
        );
    }
    pub(super) fn paint_inventory(&mut self, c: &mut Canvas, l: &Layout, v: &View) {
        let p = l.panel(Mode::Inventory);
        self.label(
            c,
            if v.inventory.is_empty() {
                "inventory_unavailable"
            } else {
                "inventory_help"
            },
            Rect::new(p.x + 14, p.y + 30, p.w.saturating_sub(28), 18),
            10,
            MUTED,
        );
        for slot in 0..36 {
            let rect = l.inventory_slot(slot);
            self.theme.slot(c, rect, v.inventory_source == Some(slot));
            if v.hover == Some(Action::Slot(slot)) {
                style::hline(c, rect.x, rect.y, rect.w, TITLE_TEXT);
            }
            self.paint_item(c, rect, v.inventory.get(slot).and_then(Option::as_ref));
        }
        self.label(
            c,
            "inventory_hotbar",
            Rect::new(p.x + 14, p.y + 188, p.w.saturating_sub(28), 18),
            10,
            MUTED,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_inventory_slot_captures_matching_press_and_release_at_each_scale() {
        for size in [[480, 270], [960, 540], [1920, 1080]] {
            let layout = Layout::new(size);
            let mut state = State::default();
            state.set_mode(Mode::Inventory);
            for slot in 0..36 {
                let rect = layout.inventory_slot(slot);
                let cursor = [
                    f64::from(rect.x + 10) * f64::from(layout.scale),
                    f64::from(rect.y + 10) * f64::from(layout.scale),
                ];
                state.motion(cursor, size);
                assert_eq!(state.pointer(true, size), None);
                assert_eq!(state.pointer(false, size), Some(Action::Slot(slot)));
            }
            let slot = layout.inventory_slot(0);
            state.motion(
                [
                    f64::from(slot.x + 10) * f64::from(layout.scale),
                    f64::from(slot.y + 10) * f64::from(layout.scale),
                ],
                size,
            );
            state.pointer(true, size);
            state.set_mode(Mode::Game);
            assert_eq!(state.pointer(false, size), None);
        }
    }
}
