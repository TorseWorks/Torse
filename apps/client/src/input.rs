//! Keyboard and mouse state for the windowed client.
//!
//! The window feeds raw winit events in; the frame loop reads one
//! [`MoveInput`] snapshot per frame. Look deltas accumulate between frames and
//! are consumed by [`InputState::take_input`], so event bursts within a frame
//! still rotate the camera once by their sum.

use std::collections::BTreeSet;

use crate::camera::MoveInput;
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton, MouseScrollDelta};
use winit::keyboard::{KeyCode, PhysicalKey};

/// Radians of yaw per pixel of horizontal drag.
const LOOK_PER_PIXEL: f32 = 0.005;
/// Multiplier applied to the fly speed per scroll step.
const SPEED_STEP: f32 = 1.25;
/// Fastest and slowest fly speed in blocks per second.
const SPEED_RANGE: (f32, f32) = (1.0, 300.0);
/// A press whose cursor wanders less than this many pixels is a click.
const CLICK_SLOP: f64 = 4.0;

/// Aggregated input state between frames.
#[derive(Debug, Default)]
pub struct InputState {
    pub bindings: crate::settings::Bindings,
    pub invert_y: bool,
    held: BTreeSet<KeyCode>,
    look_delta: PhysicalPosition<f64>,
    dragging: bool,
    last_cursor: Option<PhysicalPosition<f64>>,
    drag_distance: f64,
    pending_click: Option<MouseButton>,
    speed_factor: f32,
    screenshot: bool,
}

impl InputState {
    /// Start with the default speed factor of one.
    pub fn new() -> Self {
        Self {
            speed_factor: 1.0,
            ..Self::default()
        }
    }

    /// Track key presses; screenshot is an edge, movement is level-triggered.
    pub fn key(&mut self, key: PhysicalKey, state: ElementState) {
        let PhysicalKey::Code(code) = key else {
            return;
        };
        match state {
            ElementState::Pressed => {
                let fresh = self.held.insert(code);
                if fresh && code == self.bindings.screenshot {
                    self.screenshot = true;
                }
            }
            ElementState::Released => {
                self.held.remove(&code);
            }
        }
    }

    /// Left and right presses arm a drag; a release that barely moved is a
    /// click and is reported through [`InputState::take_click`].
    pub fn mouse(&mut self, button: MouseButton, state: ElementState) {
        if !matches!(button, MouseButton::Left | MouseButton::Right) {
            return;
        }
        match state {
            ElementState::Pressed => {
                self.dragging = true;
                self.last_cursor = None;
                self.drag_distance = 0.0;
            }
            ElementState::Released => {
                if self.dragging && self.drag_distance < CLICK_SLOP {
                    self.pending_click = Some(button);
                }
                self.dragging = false;
                self.last_cursor = None;
            }
        }
    }

    /// Accumulate cursor motion since the previous event. The first move after
    /// a press only establishes the baseline, so a click never rotates the view.
    pub fn cursor(&mut self, position: PhysicalPosition<f64>) {
        if let Some(last) = self.last_cursor.replace(position) {
            let delta = PhysicalPosition::new(position.x - last.x, position.y - last.y);
            self.drag_distance += (delta.x * delta.x + delta.y * delta.y).sqrt();
            if self.dragging {
                self.look_delta.x += delta.x;
                self.look_delta.y += delta.y;
            }
        }
    }

    /// Accumulate a bounded multiplier to consume once on the next frame.
    pub fn wheel(&mut self, delta: MouseScrollDelta) {
        let steps = match delta {
            MouseScrollDelta::LineDelta(_, lines) => lines,
            MouseScrollDelta::PixelDelta(position) => position.y as f32 / 48.0,
        };
        if steps.is_finite() {
            self.speed_factor = (self.speed_factor * SPEED_STEP.powf(steps.clamp(-64.0, 64.0)))
                .clamp(0.0001, 10_000.0);
        }
    }

    /// Consume one frame of movement intent and the pending look delta.
    ///
    /// `base_speed` is scaled by the wheel factor; no keys held yields a
    /// default `MoveInput` that leaves the camera in place.
    pub fn take_input(&mut self, base_speed: f32) -> (MoveInput, f32) {
        let axis = |positive: KeyCode, negative: KeyCode| {
            (self.held.contains(&positive) as i32 - self.held.contains(&negative) as i32) as f32
        };
        let yaw_delta = -(self.look_delta.x as f32) * LOOK_PER_PIXEL;
        let pitch_delta =
            -(self.look_delta.y as f32) * LOOK_PER_PIXEL * if self.invert_y { -1.0 } else { 1.0 };
        self.look_delta = PhysicalPosition::new(0.0, 0.0);
        let input = MoveInput {
            forward: axis(self.bindings.forward, self.bindings.back),
            strafe: axis(self.bindings.right, self.bindings.left),
            vertical: axis(self.bindings.jump, self.bindings.descend),
            yaw_delta,
            pitch_delta,
        };
        let speed = (base_speed * self.speed_factor).clamp(SPEED_RANGE.0, SPEED_RANGE.1);
        self.speed_factor = 1.0;
        (input, speed)
    }

    /// Whether the jump key is currently held.
    pub fn jump_held(&self) -> bool {
        self.held.contains(&self.bindings.jump)
    }

    /// Feed one captured-mouse motion step.
    ///
    /// Accumulates the same way as drag look so the shared `take_input`
    /// sign stays correct: mouse right turns right and mouse up looks up.
    pub fn mouse_look(&mut self, dx: f64, dy: f64) {
        if dx.is_finite() && dy.is_finite() {
            self.look_delta.x += dx;
            self.look_delta.y += dy;
        }
    }

    /// Queue a click that fired while the cursor was captured; there is no
    /// drag distance to distinguish it from a look motion.
    pub fn queue_click(&mut self, button: MouseButton) {
        self.pending_click = Some(button);
    }

    /// Consume the pending click, if the last press qualified as one.
    pub fn take_click(&mut self) -> Option<MouseButton> {
        self.pending_click.take()
    }

    /// Consume the screenshot edge.
    pub fn take_screenshot(&mut self) -> bool {
        std::mem::take(&mut self.screenshot)
    }
    /// Drop all held input when focus moves into a modal or leaves the window.
    pub fn reset(&mut self) {
        self.held.clear();
        self.look_delta = PhysicalPosition::new(0.0, 0.0);
        self.dragging = false;
        self.last_cursor = None;
        self.pending_click = None;
        self.drag_distance = 0.0;
        self.speed_factor = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rebound_movement_and_screenshot_keys_and_inverted_look_take_effect() {
        let mut state = InputState::new();
        state
            .bindings
            .rebind(crate::settings::Binding::Forward, KeyCode::KeyZ)
            .unwrap();
        state
            .bindings
            .rebind(crate::settings::Binding::Screenshot, KeyCode::F10)
            .unwrap();
        state.invert_y = true;
        state.key(PhysicalKey::Code(KeyCode::KeyW), ElementState::Pressed);
        assert_eq!(state.take_input(12.0).0.forward, 0.0);
        state.key(PhysicalKey::Code(KeyCode::KeyZ), ElementState::Pressed);
        state.mouse_look(0.0, 2.0);
        let input = state.take_input(12.0).0;
        assert_eq!(input.forward, 1.0);
        assert!(input.pitch_delta > 0.0);
        state.key(PhysicalKey::Code(KeyCode::F12), ElementState::Pressed);
        assert!(!state.take_screenshot());
        state.key(PhysicalKey::Code(KeyCode::F10), ElementState::Pressed);
        assert!(state.take_screenshot());
        state.key(PhysicalKey::Code(KeyCode::F10), ElementState::Pressed);
        assert!(!state.take_screenshot());
        state.reset();
        assert_eq!(state.take_input(12.0).0.forward, 0.0);
    }
    #[test]
    fn focus_reset_cancels_held_movement_and_late_mouse_release() {
        let mut state = InputState::new();
        state.key(PhysicalKey::Code(KeyCode::KeyW), ElementState::Pressed);
        state.mouse(MouseButton::Left, ElementState::Pressed);
        state.reset();
        state.mouse(MouseButton::Left, ElementState::Released);
        assert!(state.take_click().is_none());
        let (motion, _) = state.take_input(12.0);
        assert_eq!(motion.forward, 0.0);
        assert_eq!(motion.yaw_delta, 0.0);
    }
    #[test]
    fn one_scroll_changes_speed_once_across_successive_frames() {
        let mut state = InputState::new();
        state.wheel(MouseScrollDelta::LineDelta(0.0, 1.0));
        let (_, mut speed) = state.take_input(12.0);
        assert_eq!(speed, 15.0);
        for _ in 0..600 {
            speed = state.take_input(speed).1;
            assert_eq!(
                speed, 15.0,
                "an idle wheel must not accelerate the next frame"
            );
        }
        state.wheel(MouseScrollDelta::LineDelta(0.0, -1.0));
        assert_eq!(state.take_input(speed).1, 12.0);
    }

    fn press(state: &mut InputState, code: KeyCode) {
        state.key(PhysicalKey::Code(code), ElementState::Pressed);
    }

    fn release(state: &mut InputState, code: KeyCode) {
        state.key(PhysicalKey::Code(code), ElementState::Released);
    }

    #[test]
    fn held_keys_drive_movement_axes() {
        let mut state = InputState::new();
        press(&mut state, KeyCode::KeyW);
        press(&mut state, KeyCode::KeyD);
        let (input, speed) = state.take_input(12.0);
        assert_eq!(input.forward, 1.0);
        assert_eq!(input.strafe, 1.0);
        assert_eq!(input.vertical, 0.0);
        assert_eq!(input.yaw_delta, 0.0);
        assert_eq!(speed, 12.0);
        release(&mut state, KeyCode::KeyW);
        press(&mut state, KeyCode::KeyS);
        let (input, _) = state.take_input(12.0);
        assert_eq!(input.forward, -1.0);
    }

    #[test]
    fn drag_deltas_accumulate_and_clear() {
        let mut state = InputState::new();
        state.mouse(MouseButton::Left, ElementState::Pressed);
        // The first move after a press is only the baseline.
        state.cursor(PhysicalPosition::new(10.0, 4.0));
        state.cursor(PhysicalPosition::new(16.0, 14.0));
        state.cursor(PhysicalPosition::new(22.0, 14.0));
        let (input, _) = state.take_input(12.0);
        // Deltas are (6, 10) then (6, 0), so the camera turns by their sum.
        assert!((input.yaw_delta - (-12.0 * LOOK_PER_PIXEL)).abs() < 1e-6);
        assert!((input.pitch_delta - (-10.0 * LOOK_PER_PIXEL)).abs() < 1e-6);
        let (input, _) = state.take_input(12.0);
        assert_eq!(input.yaw_delta, 0.0);
        // A long drag is not a click.
        state.mouse(MouseButton::Left, ElementState::Released);
        assert_eq!(state.take_click(), None);
        // Without a held button the cursor does not rotate the view.
        state.cursor(PhysicalPosition::new(500.0, 500.0));
        state.cursor(PhysicalPosition::new(510.0, 510.0));
        let (input, _) = state.take_input(12.0);
        assert_eq!(input.yaw_delta, 0.0);
    }

    #[test]
    fn short_presses_report_clicks_by_button() {
        let mut state = InputState::new();
        state.mouse(MouseButton::Left, ElementState::Pressed);
        state.cursor(PhysicalPosition::new(50.0, 50.0));
        state.cursor(PhysicalPosition::new(52.0, 51.0));
        state.mouse(MouseButton::Left, ElementState::Released);
        assert_eq!(state.take_click(), Some(MouseButton::Left));
        assert_eq!(state.take_click(), None);

        state.mouse(MouseButton::Right, ElementState::Pressed);
        state.cursor(PhysicalPosition::new(50.0, 50.0));
        state.mouse(MouseButton::Right, ElementState::Released);
        assert_eq!(state.take_click(), Some(MouseButton::Right));

        // A release without a preceding press reports nothing and stays quiet.
        state.mouse(MouseButton::Middle, ElementState::Released);
        assert_eq!(state.take_click(), None);
    }

    #[test]
    fn wheel_scales_speed_within_bounds() {
        let mut state = InputState::new();
        state.wheel(MouseScrollDelta::LineDelta(0.0, 2.0));
        let (_, speed) = state.take_input(12.0);
        assert!((speed - 12.0 * SPEED_STEP * SPEED_STEP).abs() < 1e-4);
        for _ in 0..40 {
            state.wheel(MouseScrollDelta::LineDelta(0.0, -1.0));
        }
        let (_, speed) = state.take_input(12.0);
        assert_eq!(speed, SPEED_RANGE.0);
        state.wheel(MouseScrollDelta::LineDelta(0.0, 1000.0));
        let (_, speed) = state.take_input(speed);
        assert_eq!(speed, SPEED_RANGE.1);
        assert_eq!(state.take_input(speed).1, SPEED_RANGE.1);
    }

    #[test]
    fn captured_mouse_uses_first_person_signs() {
        let mut state = InputState::new();
        state.mouse_look(10.0, 0.0);
        let (motion, _) = state.take_input(12.0);
        // Mouse right turns right: yaw decreases, facing -z means the
        // forward vector swings toward +x.
        assert!(motion.yaw_delta < 0.0);
        state.mouse_look(0.0, -8.0);
        let (motion, _) = state.take_input(12.0);
        // Mouse up looks up: positive pitch delta.
        assert!(motion.pitch_delta > 0.0);
        state.queue_click(MouseButton::Right);
        assert_eq!(state.take_click(), Some(MouseButton::Right));
    }

    #[test]
    fn screenshot_fires_once_per_press() {
        let mut state = InputState::new();
        assert!(!state.take_screenshot());
        press(&mut state, KeyCode::F12);
        assert!(state.take_screenshot());
        assert!(!state.take_screenshot());
        release(&mut state, KeyCode::F12);
        press(&mut state, KeyCode::F12);
        assert!(state.take_screenshot());
    }
}
