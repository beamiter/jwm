// The session backend, its DRM/KMS compositor, and the KMS colour pipeline
// carry the direct-scanout dependencies (drm, gbm, libseat) and only build
// with the udev backend. Everything else in this tree is Smithay protocol
// state shared by all Wayland backends through `backend::wayland::state`.
#[cfg(feature = "backend-wayland-udev")]
pub mod backend;

#[cfg(feature = "backend-wayland-udev")]
pub mod compositor;

pub mod output_management;

pub mod output_power;

pub mod screencopy;

pub mod tearing_control;

pub mod state;

pub mod workspace_protocol;

pub mod image_copy_capture;

pub mod gamma_control;

pub mod foreign_toplevel_management;

pub mod virtual_pointer;

pub mod color_management;

pub mod icc;

#[cfg(feature = "backend-wayland-udev")]
pub mod color_pipeline;
