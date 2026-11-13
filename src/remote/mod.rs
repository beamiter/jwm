//! JWM-to-JWM remote viewing and input control for trusted X11 LANs.
//!
//! The first implementation deliberately runs out of process.  It reads the
//! shared X Composite overlay used by both JWM X11 backends and injects input
//! through an independent XTEST connection, keeping all socket and JPEG work
//! outside the window manager's latency-sensitive event loop.

use std::error::Error;
use std::time::Duration;

/// Maximum gap between authenticated video frames once streaming has begun.
/// The host's unchanged-frame keepalive cadence is tested against this shared
/// value so the two sides cannot silently drift apart.
pub(super) const VIDEO_FRAME_IDLE_TIMEOUT: Duration = Duration::from_secs(8);

pub type RemoteError = Box<dyn Error + Send + Sync>;
pub type RemoteResult<T> = Result<T, RemoteError>;

pub mod client;
pub mod clipboard_files;
pub mod clipboard_policy;
pub mod clipboard_staging_budget;
pub mod clipboard_sync;
pub mod clipboard_wire;
pub mod close_wire;
mod control_repair_wire;
pub mod cursor;
mod deadline;
mod downscale;
pub mod frame;
pub mod host;
mod host_capture;
pub mod key;
#[cfg(feature = "remote-wayland")]
mod keyboard_keymap;
pub mod messages;
mod pacing;
mod pointer_repair_phase;
mod pointer_repair_wire;
pub mod profiles;
pub mod protocol;
pub mod replay_benchmark;
pub mod scaling;
mod sender;
pub use scaling::ScalingPolicy;
pub mod session_env;
pub mod source_session;
pub mod source_wire;
pub mod ssh;
mod tile;
mod triangle_patch;
pub mod usability;
#[cfg(feature = "remote-wayland")]
mod wayland_capture;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
mod wayland_capture_integration;
pub mod wayland_control_wire;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
mod wayland_keyboard_integration;
pub mod wayland_keyboard_wire;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
mod wayland_pointer_integration;
pub mod wayland_pointer_wire;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
mod wayland_popup_capture_integration;
pub mod wayland_text_wire;
mod window_capture;
#[cfg(test)]
mod window_capture_integration;
pub mod window_position;
pub mod window_resize;
mod window_resize_native;
pub mod window_size;
pub mod x11_capture;
pub mod x11_input;
pub mod x11_keymap;
pub mod x11_viewer;

#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
mod viewer_test_driver;

mod wayland_popup_wire;

pub mod viewer_backend;
#[cfg(feature = "remote-wayland")]
mod wayland_viewer;

mod follow_source_size;

mod png_atlas;

pub mod resumable_control;
