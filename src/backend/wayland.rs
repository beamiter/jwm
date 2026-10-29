// Shared Smithay Wayland compositor state (used by udev/KMS and windowed X11 backend).
//
// Keep this as a small shim so the real implementation can live under `wayland_udev/`
// while preserving the public path `crate::backend::wayland::state`.

#[path = "wayland_udev/state.rs"]
pub mod state;

#[cfg(feature = "backend-wayland-nested")]
#[path = "wayland_xwayland.rs"]
pub(crate) mod xwayland;

#[cfg(feature = "backend-wayland-nested")]
#[path = "wayland_capture.rs"]
pub(crate) mod capture;

#[cfg(feature = "backend-wayland-nested")]
#[path = "wayland_preview.rs"]
pub(crate) mod preview;

#[cfg(feature = "backend-wayland-nested")]
#[path = "wayland_eva.rs"]
pub(crate) mod eva;
