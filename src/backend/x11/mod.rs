//! Logical X11 namespace for backend code.
//!
//! This module is an architectural facade over older top-level modules:
//! - `wm`: shared X11 WM/EWMH/property helpers used by both `xcb` and `x11rb`
//! - `compositor`: the shared X11 compositor implementation reused by both backends
//! - `compositor_common`: support modules used by the X11 compositor stack
//! - `compositor_backend`: X11 protocol traits/adapters consumed by the shared compositor

pub mod compositor;
pub mod compositor_common;
/// X11 protocol traits required by the shared compositor implementation.
pub mod compositor_backend;
pub mod wm;
