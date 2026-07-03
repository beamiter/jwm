//! Logical X11 namespace for backend code.
//!
//! This module is an architectural facade over older top-level modules:
//! - `wm`: shared X11 WM/EWMH/property helpers used by both `xcb` and `x11rb`
//! - `compositor`: the shared X11 compositor implementation reused by both backends
//! - `compositor_backend`: X11 protocol traits/adapters consumed by the shared compositor
//!
//! The older modules remain in place for compatibility while call sites migrate.

/// Shared X11 WM/EWMH/property helpers used by both `xcb` and `x11rb`.
pub mod wm {
    pub use crate::backend::x11_shared::*;
}

/// Shared X11 compositor implementation used by the `xcb` and `x11rb` backends.
pub mod compositor {
    pub use crate::backend::shared_x11_compositor::*;
}

/// X11 protocol traits required by the shared compositor implementation.
pub mod compositor_backend;
