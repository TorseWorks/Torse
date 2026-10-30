use super::*;
impl Layout {
    pub(super) fn catalog_button(&self, direction: i32) -> Rect {
        let p = self.panel(Mode::Inventory);
        Rect::new(
            p.x + 12 + if direction > 0 { 240 } else { 0 },
            p.y + 190,
            28,
            20,
        )
    }
    pub(super) fn inventory_slot(&self, slot: usize, infinite: bool) -> Rect {
        let p = self.panel(Mode::Inventory);
        let row = if infinite {
            slot / 9
        } else if slot < 9 {
            4
        } else {
            slot / 9 - 1
        };
        Rect::new(
            p.x + 12 + (slot % 9) as i32 * 30,
            p.y + 52 + row as i32 * 32,
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
            "base:vertical_glass_slab" => Some("item_vertical_glass"),
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
        show_count: bool,
    ) {
        let Some(stack) = stack else {
            return;
        };
        if let Some(sprite) = self.models.get(&stack.item) {
            c.image(
                sprite,
                Rect::new(
                    rect.x + 1,
                    rect.y + 1,
                    rect.w.saturating_sub(2),
                    rect.h.saturating_sub(2),
                ),
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
        if !show_count {
            return;
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
        let infinite = v.settings.infinite_supply;
        let side = Rect::new(p.x + 292, p.y + 30, p.w.saturating_sub(304), 202);
        self.theme.paint(c, Part::Group, side);
        self.label(
            c,
            if infinite {
                "inventory_supply"
            } else {
                "inventory_storage"
            },
            Rect::new(p.x + 12, p.y + 30, 268, 16),
            11,
            TITLE_TEXT,
        );
        for slot in 0..36 {
            let rect = l.inventory_slot(slot, infinite);
            self.theme.slot(c, rect, v.inventory_source == Some(slot));
            if v.hover == Some(Action::Slot(slot)) {
                style::hline(c, rect.x, rect.y, rect.w, TITLE_TEXT);
            }
            self.paint_item(
                c,
                rect,
                v.inventory.get(slot).and_then(Option::as_ref),
                !infinite,
            );
            if !infinite && slot < 9 {
                self.text.draw(
                    c,
                    &(slot + 1).to_string(),
                    Rect::new(rect.x + 2, rect.y + 1, 8, 9),
                    8,
                    MUTED,
                );
            }
        }
        if infinite {
            for direction in [-1, 1] {
                let rect = l.catalog_button(direction);
                let active = if direction < 0 {
                    v.catalog_page > 0
                } else {
                    v.catalog_page + 1 < v.catalog_pages
                };
                self.theme.button(
                    c,
                    rect,
                    active && v.hover == Some(Action::CatalogPage(direction)),
                    v.pressed == Some(Action::CatalogPage(direction)),
                );
                self.text.draw(
                    c,
                    if direction < 0 { "<" } else { ">" },
                    Rect::new(rect.x + 10, rect.y + 3, 12, 14),
                    12,
                    if active { TEXT } else { MUTED },
                );
            }
            self.text.draw(
                c,
                &format!("{} / {}", v.catalog_page + 1, v.catalog_pages.max(1)),
                Rect::new(p.x + 112, p.y + 193, 120, 16),
                11,
                TEXT,
            );
        } else {
            self.label(
                c,
                "inventory_bar",
                Rect::new(p.x + 12, p.y + 164, 268, 14),
                10,
                MUTED,
            );
        }
        let selected = match v.hover {
            Some(Action::Slot(slot)) => Some(slot),
            _ => v.inventory_source,
        };
        let stack = selected
            .and_then(|slot| v.inventory.get(slot).and_then(Option::as_ref))
            .or_else(|| v.bar.get(v.material).and_then(Option::as_ref));
        if let Some(stack) = stack {
            if let Some(sprite) = self.models.detail(&stack.item) {
                c.image(
                    sprite,
                    Rect::new(side.x + (side.w as i32 - 72) / 2, side.y + 12, 72, 72),
                );
            }
            let name = self.item_name(stack);
            self.text.draw(
                c,
                &name,
                Rect::new(side.x + 8, side.y + 94, side.w.saturating_sub(16), 18),
                11,
                TITLE_TEXT,
            );
            let quantity = if infinite {
                self.labels
                    .get("inventory_supply")
                    .copied()
                    .unwrap_or("")
                    .to_owned()
            } else {
                format!(
                    "{}: {}",
                    self.labels.get("inventory_quantity").copied().unwrap_or(""),
                    stack.count
                )
            };
            self.text.draw(
                c,
                &quantity,
                Rect::new(side.x + 8, side.y + 119, side.w.saturating_sub(16), 16),
                10,
                MUTED,
            );
            if infinite {
                let target = format!(
                    "{}: {}",
                    self.labels.get("inventory_target").copied().unwrap_or(""),
                    v.material + 1
                );
                self.text.draw(
                    c,
                    &target,
                    Rect::new(side.x + 8, side.y + 134, side.w.saturating_sub(16), 13),
                    9,
                    TEXT,
                );
            }
            let id = stack.item.to_string();
            for (row, chunk) in id.as_bytes().chunks(18).take(3).enumerate() {
                self.text.draw(
                    c,
                    &String::from_utf8_lossy(chunk),
                    Rect::new(
                        side.x + 8,
                        side.y + 148 + row as i32 * 12,
                        side.w.saturating_sub(16),
                        12,
                    ),
                    8,
                    MUTED,
                );
            }
        } else {
            self.label(
                c,
                "inventory_unavailable",
                Rect::new(side.x + 8, side.y + 96, side.w.saturating_sub(16), 32),
                9,
                MUTED,
            );
        }
        self.label(
            c,
            if infinite {
                "inventory_supply_help"
            } else {
                "inventory_help"
            },
            Rect::new(p.x + 12, p.y + 219, p.w.saturating_sub(24), 13),
            9,
            MUTED,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_inventory_slot_captures_matching_press_and_release_at_each_scale() {
        for (size, infinite) in [[480, 270], [960, 540], [1920, 1080]]
            .into_iter()
            .flat_map(|size| [false, true].map(|mode| (size, mode)))
        {
            let layout = Layout::new(size);
            let mut state = State::default();
            state.settings.infinite_supply = infinite;
            state.set_mode(Mode::Inventory);
            for slot in 0..36 {
                let rect = layout.inventory_slot(slot, infinite);
                let cursor = [
                    f64::from(rect.x + 10) * f64::from(layout.scale),
                    f64::from(rect.y + 10) * f64::from(layout.scale),
                ];
                state.motion(cursor, size);
                assert_eq!(state.pointer(true, size), None);
                assert_eq!(state.pointer(false, size), Some(Action::Slot(slot)));
            }
            if infinite {
                for direction in [-1, 1] {
                    let rect = layout.catalog_button(direction);
                    state.motion(
                        [
                            f64::from(rect.x + 8) * f64::from(layout.scale),
                            f64::from(rect.y + 8) * f64::from(layout.scale),
                        ],
                        size,
                    );
                    state.pointer(true, size);
                    assert_eq!(
                        state.pointer(false, size),
                        Some(Action::CatalogPage(direction))
                    );
                }
            }
            let slot = layout.inventory_slot(0, infinite);
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
