//! Flat dark theme drawn procedurally on the UI canvas, in the vein of
//! Dear ImGui: translucent dark panels, one-pixel borders, compact controls
//! and a single accent colour that always means "active or selected".

use super::canvas::{Canvas, Rect};

pub const WINDOW_BG: [u8; 4] = [24, 26, 30, 244];
pub const GROUP_BG: [u8; 4] = [29, 31, 36, 255];
pub const BORDER: [u8; 4] = [86, 92, 102, 255];
pub const SEPARATOR: [u8; 4] = [58, 62, 70, 255];
pub const TITLE_TEXT: [u8; 4] = [242, 244, 246, 255];
pub const TEXT: [u8; 4] = [226, 229, 233, 255];
pub const MUTED: [u8; 4] = [148, 155, 163, 255];
pub const BUTTON_BG: [u8; 4] = [44, 47, 53, 255];
pub const BUTTON_BG_HOT: [u8; 4] = [56, 60, 68, 255];
pub const BUTTON_BG_ACTIVE: [u8; 4] = [38, 41, 46, 255];
pub const BUTTON_BORDER: [u8; 4] = [66, 71, 80, 255];
pub const BUTTON_BORDER_HOT: [u8; 4] = [104, 111, 122, 255];
pub const ACCENT: [u8; 4] = [96, 148, 210, 255];
pub const SLOT_BG: [u8; 4] = [30, 32, 36, 255];
pub const SLOT_BORDER: [u8; 4] = [58, 62, 70, 255];
pub const FIELD_BG: [u8; 4] = [20, 21, 24, 255];
pub const CHIP_BG: [u8; 4] = [22, 24, 28, 208];
pub const CHIP_BORDER: [u8; 4] = [70, 75, 84, 190];
pub const TRACK_BG: [u8; 4] = [36, 39, 44, 255];
pub const SHADOW: [u8; 4] = [12, 13, 16, 200];

/// Fill `rect` with `bg` and trace a one-pixel `border` around it.
pub fn frame(c: &mut Canvas, rect: Rect, bg: [u8; 4], border: [u8; 4]) {
    c.fill(rect, bg);
    hline(c, rect.x, rect.y, rect.w, border);
    hline(c, rect.x, rect.y + rect.h as i32 - 1, rect.w, border);
    vline(c, rect.x, rect.y, rect.h, border);
    vline(c, rect.x + rect.w as i32 - 1, rect.y, rect.h, border);
}

pub fn hline(c: &mut Canvas, x: i32, y: i32, w: u32, color: [u8; 4]) {
    c.fill(Rect::new(x, y, w, 1), color);
}

pub fn vline(c: &mut Canvas, x: i32, y: i32, h: u32, color: [u8; 4]) {
    c.fill(Rect::new(x, y, 1, h), color);
}

/// Menu button with normal, hover and pressed looks. The pressed state keeps
/// a darker fill and takes the accent border, matching the selection accent.
pub fn button(c: &mut Canvas, rect: Rect, hot: bool, active: bool) {
    let bg = if active {
        BUTTON_BG_ACTIVE
    } else if hot {
        BUTTON_BG_HOT
    } else {
        BUTTON_BG
    };
    let border = if active {
        ACCENT
    } else if hot {
        BUTTON_BORDER_HOT
    } else {
        BUTTON_BORDER
    };
    frame(c, rect, bg, border);
}

/// Inventory slot; the selected slot swaps to the accent border.
pub fn slot(c: &mut Canvas, rect: Rect, selected: bool) {
    frame(
        c,
        rect,
        SLOT_BG,
        if selected { ACCENT } else { SLOT_BORDER },
    );
}

/// Horizontal progress track with an inset accent fill.
pub fn progress(c: &mut Canvas, rect: Rect, amount: f32) {
    frame(c, rect, TRACK_BG, SLOT_BORDER);
    let inner = rect.w.saturating_sub(2);
    let fill = ((f64::from(inner)) * f64::from(amount.clamp(0.0, 1.0))).round() as u32;
    c.fill(
        Rect::new(rect.x + 1, rect.y + 1, fill, rect.h.saturating_sub(2)),
        ACCENT,
    );
}

/// Centred crosshair with a one-pixel drop shadow. The vertical stroke is
/// split around the centre so no pixel is blended twice.
pub fn crosshair(c: &mut Canvas, cx: i32, cy: i32) {
    let (x, y) = (cx - 3, cy - 3);
    for (x, y, color) in [(x + 1, y + 1, SHADOW), (x, y, [235, 237, 240, 220])] {
        hline(c, x, y + 3, 7, color);
        vline(c, x + 3, y, 3, color);
        vline(c, x + 3, y + 4, 3, color);
    }
}

/// Small close cross inside a title-bar hit rect.
pub fn close_glyph(c: &mut Canvas, rect: Rect, hot: bool) {
    let color = if hot { TITLE_TEXT } else { MUTED };
    let (x, y) = (
        rect.x + rect.w as i32 / 2 - 3,
        rect.y + rect.h as i32 / 2 - 3,
    );
    for i in 0..6 {
        c.fill(Rect::new(x + i, y + i, 1, 1), color);
        c.fill(Rect::new(x + 5 - i, y + i, 1, 1), color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_draws_background_with_one_pixel_border() {
        let mut c = Canvas::new(6, 4, 1);
        frame(
            &mut c,
            Rect::new(0, 0, 6, 4),
            [10, 10, 10, 255],
            [200, 0, 0, 255],
        );
        assert_eq!(c.as_pixels()[..3], [200, 0, 0]);
        assert_eq!(&c.as_pixels()[6 * 4 + 3 * 4..][..3], &[10, 10, 10]);
    }

    #[test]
    fn progress_fill_stays_inside_the_track() {
        let mut c = Canvas::new(10, 6, 1);
        progress(&mut c, Rect::new(0, 0, 10, 6), 0.5);
        // Half of the eight inner pixels are accent; the border row is not.
        let accent = c
            .as_pixels()
            .chunks_exact(4)
            .filter(|p| *p == ACCENT)
            .count();
        assert_eq!(accent, 4 * 4);
    }

    #[test]
    fn crosshair_marks_the_centre_with_a_shadow() {
        let mut c = Canvas::new(9, 9, 1);
        crosshair(&mut c, 4, 4);
        let at = |x: usize, y: usize| &c.as_pixels()[(y * 9 + x) * 4..(y * 9 + x) * 4 + 4];
        assert_eq!(at(4, 4), &[235, 237, 240, 220]);
        assert_eq!(at(5, 5), SHADOW);
    }
}
