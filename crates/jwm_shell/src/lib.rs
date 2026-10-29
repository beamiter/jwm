//! Shared native sidebar rendering and window host. No top-bar toolkit owns it.
pub mod edge_shell;
pub mod ipc;
pub mod native_wayland;
pub mod wallpaper;

mod wayland_bootstrap;
