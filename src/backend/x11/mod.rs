//! Logical X11 namespace for backend code.
//!
//! This module is an architectural facade over older top-level modules:
//! - `wm`: shared X11 WM/EWMH/property helpers used by both `xcb` and `x11rb`
//! - `compositor`: the shared X11 compositor implementation reused by both backends
//! - `compositor_common`: shared support modules and X11 protocol traits used by the compositor

pub mod compositor;
#[path = "compositor/common/mod.rs"]
pub mod compositor_common;
pub mod wm;
