//! Optional immediate-mode UI primitives, input capture and replaceable skins.
//!
//! Hosts own application state and authority. This crate only draws and
//! resolves widget interactions against the current declarations.
#![forbid(unsafe_code)]

pub mod canvas;
#[cfg(feature = "gpu")]
pub mod gpu;
pub mod input;
pub mod style;
pub mod theme;

pub use canvas::{Canvas, Rect, Sprite, Text};
#[cfg(feature = "gpu")]
pub use gpu::Overlay;
pub use input::{Control, Event, Pointer};
pub use theme::{Part, Theme};
