//! Input capture over controls declared anew by the host each frame.
use crate::Rect;

#[derive(Clone, Copy)]
pub enum Control<K> {
    Button {
        key: K,
        rect: Rect,
    },
    Slider {
        key: K,
        rect: Rect,
        min: u16,
        max: u16,
    },
}
impl<K: Copy> Control<K> {
    fn key(self) -> K {
        match self {
            Self::Button { key, .. } | Self::Slider { key, .. } => key,
        }
    }
    fn rect(self) -> Rect {
        match self {
            Self::Button { rect, .. } | Self::Slider { rect, .. } => rect,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event<K> {
    Activated(K),
    Changed(K, u16),
}

pub struct Pointer<K> {
    pub cursor: [i32; 2],
    active: Option<K>,
    dragging: bool,
}
impl<K> Default for Pointer<K> {
    fn default() -> Self {
        Self {
            cursor: [0; 2],
            active: None,
            dragging: false,
        }
    }
}
impl<K: Copy + Eq> Pointer<K> {
    pub fn cancel(&mut self) {
        self.active = None;
        self.dragging = false;
    }
    pub fn active(&self) -> Option<K> {
        self.active
    }
    pub fn dragging(&self) -> bool {
        self.dragging
    }
    pub fn hover(&self, controls: &[Control<K>]) -> Option<K> {
        controls
            .iter()
            .copied()
            .rev()
            .find(|control| control.rect().contains(self.cursor))
            .map(Control::key)
    }
    fn changed(&mut self, controls: &[Control<K>]) -> Option<Event<K>> {
        let key = self.active?;
        let control = controls.iter().copied().find(|c| c.key() == key);
        let Some(Control::Slider { rect, min, max, .. }) = control else {
            self.cancel();
            return None;
        };
        let width = i64::from(rect.w.saturating_sub(1).max(1));
        let x = (i64::from(self.cursor[0]) - i64::from(rect.x)).clamp(0, width);
        let range = max.saturating_sub(min);
        let value = u16::try_from((x * i64::from(range) + width / 2) / width).ok()?;
        Some(Event::Changed(key, min.saturating_add(value)))
    }
    pub fn motion(&mut self, cursor: [i32; 2], controls: &[Control<K>]) -> Option<Event<K>> {
        self.cursor = cursor;
        if self.dragging {
            self.changed(controls)
        } else {
            None
        }
    }
    pub fn button(&mut self, down: bool, controls: &[Control<K>]) -> Option<Event<K>> {
        if down {
            self.cancel();
            let control = controls
                .iter()
                .copied()
                .rev()
                .find(|c| c.rect().contains(self.cursor))?;
            self.active = Some(control.key());
            self.dragging = matches!(control, Control::Slider { .. });
            if self.dragging {
                self.changed(controls)
            } else {
                None
            }
        } else if self.dragging {
            let event = self.changed(controls);
            self.cancel();
            event
        } else {
            let key = self.active.take()?;
            (self.hover(controls) == Some(key)).then_some(Event::Activated(key))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn button_requires_matching_release_and_slider_captures_until_release() {
        let controls = [
            Control::Button {
                key: 1,
                rect: Rect::new(0, 0, 20, 10),
            },
            Control::Slider {
                key: 2,
                rect: Rect::new(0, 20, 101, 10),
                min: 30,
                max: 1000,
            },
        ];
        let mut pointer = Pointer::default();
        pointer.motion([5, 5], &controls);
        assert_eq!(pointer.button(true, &controls), None);
        pointer.motion([200, 5], &controls);
        assert_eq!(pointer.button(false, &controls), None);
        pointer.motion([0, 25], &controls);
        assert_eq!(pointer.button(true, &controls), Some(Event::Changed(2, 30)));
        assert_eq!(
            pointer.motion([500, -200], &controls),
            Some(Event::Changed(2, 1000))
        );
        assert_eq!(
            pointer.button(false, &controls),
            Some(Event::Changed(2, 1000))
        );
        assert!(!pointer.dragging());
        assert_eq!(pointer.button(false, &controls), None);
    }
    #[test]
    fn removing_a_dragged_control_cancels_it() {
        let control = [Control::Slider {
            key: 2,
            rect: Rect::new(0, 0, 10, 10),
            min: 0,
            max: 100,
        }];
        let mut pointer = Pointer::default();
        pointer.button(true, &control);
        assert_eq!(pointer.motion([5, 5], &[]), None);
        assert!(pointer.active().is_none());
    }
}
