// src/backend/mod.rs

pub mod api;
pub mod color_policy;
// MIME-level clipboard judgments shared by every backend that captures a
// selection; see the module for why they sit below the history.
pub mod clipboard_offer;

// X11 CLIPBOARD mechanics, shared by the x11rb backend and the remote helper.
// Gated on x11rb being present rather than on a backend, because a slim
// `backend-xcb,remote-x11` build has the helper without the x11rb backend.
#[cfg(any(feature = "backend-x11rb", feature = "remote-x11"))]
pub mod clipboard_x11;
pub mod common_define;
pub mod edid;
pub mod error;
pub mod hdr_metadata;

// Protocol-independent compositor helpers shared by Wayland and X11 backends.
// Slim build profiles compile only a subset of the consumers, so dead-code
// analysis is relaxed unless every consumer is present.
#[cfg_attr(
    not(all(feature = "x11-backends", feature = "backend-wayland-udev")),
    allow(dead_code)
)]
pub mod compositor_common;

// Backend-independent CPU bitmap text rasterizer shared by both compositors.
#[cfg_attr(
    not(all(feature = "x11-backends", feature = "backend-wayland-udev")),
    allow(dead_code)
)]
pub mod compositor_font;

// Backend-independent process RSS + CPU sampler used by the debug HUD.
pub mod sys_stats;
// Bounded regular-file reader shared by both compositor shader reload paths.
pub(crate) mod shader_source;
#[doc(hidden)]
pub mod update_notifier;

/// Diagnostics for the X11 damage pipeline. Counters are bumped at each stage
/// so the periodic render-frequency log line can show where DamageNotify
/// events stop flowing (raw arrival → resolved → marked). A window whose
/// NonEmpty damage object misses one subtract never reports again, so the
/// stages that can drop an event also log a warning.
pub mod damage_diag {
    use std::sync::atomic::AtomicU64;
    /// DamageNotify events decoded from the X event stream.
    pub static RAW: AtomicU64 = AtomicU64::new(0);
    /// Damage events dropped because the WindowId no longer resolves.
    pub static UNRESOLVED: AtomicU64 = AtomicU64::new(0);
    /// Damage marks applied to a tracked compositor window.
    pub static MARKED: AtomicU64 = AtomicU64::new(0);
    /// Damage events for windows the compositor does not track (these can
    /// never be subtracted, so ReportLevel::NonEmpty stops firing for them).
    pub static UNTRACKED: AtomicU64 = AtomicU64::new(0);
}

#[cfg(feature = "x11-backends")]
#[cfg_attr(
    not(all(feature = "backend-x11rb", feature = "backend-xcb")),
    allow(dead_code)
)]
pub mod x11;

// Shared Xcursor theme loader (theme name + size resolution, image selection)
// used by every backend that renders or installs a themed pointer.
pub mod xcursor_theme;

// Shared Smithay Wayland compositor state (used by udev/KMS and windowed X11 backend).
#[cfg(feature = "wayland-backends")]
pub mod wayland;

// Shared xkbcommon-based key mapping used by Smithay-backed backends.
#[cfg(feature = "wayland-backends")]
pub mod wayland_key_ops;

// Shared dummy ops used by Smithay-backed backends and by backend-neutral
// policy tests, so they stay available in every build profile.
pub mod wayland_dummy_ops;

#[cfg(feature = "backend-x11rb")]
pub mod x11rb;
#[cfg(feature = "backend-xcb")]
pub mod xcb;

#[cfg(feature = "wayland-backends")]
#[path = "wayland_udev/mod.rs"]
pub mod wayland_udev;

// Backwards-compat alias for older module paths.
#[cfg(feature = "backend-wayland-udev")]
pub mod udev {
    pub mod backend {
        pub use crate::backend::wayland_udev::backend::*;
    }
}

#[cfg(feature = "backend-wayland-nested")]
pub mod wayland_x11;

#[cfg(feature = "backend-wayland-nested")]
pub mod wayland_winit;
