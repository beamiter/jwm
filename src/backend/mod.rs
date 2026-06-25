// src/backend/mod.rs

pub mod api;
pub mod common_define;
pub mod edid;
pub mod error;
pub mod hdr_metadata;

// Backend-independent CPU bitmap text rasterizer shared by both compositors.
pub mod compositor_font;

// Backend-independent process RSS + CPU sampler used by the debug HUD.
pub mod sys_stats;

// Shared Smithay Wayland compositor state (used by udev/KMS and windowed X11 backend).
pub mod wayland;

// Shared xkbcommon-based key mapping used by Smithay-backed backends.
pub mod wayland_key_ops;

// Shared dummy ops used by Smithay-backed backends.
pub mod wayland_dummy_ops;

pub mod x11;

#[path = "wayland_udev/mod.rs"]
pub mod wayland_udev;

// Backwards-compat alias for older module paths.
pub mod udev {
	pub mod backend {
		pub use crate::backend::wayland_udev::backend::*;
	}
}

pub mod wayland_x11;

pub mod wayland_winit;
