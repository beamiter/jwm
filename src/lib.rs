#![warn(dead_code, unused, unreachable_pub)]
// Keep the automatic gate focused on bug-prone diagnostics. Enabling every
// style and pedantic lint across the renderer/backends produced thousands of
// repeated warnings, which hid new correctness findings in CI. Individual
// modules can opt into stricter policy while the existing style debt is paid
// down (doctor.rs already does).
#![deny(clippy::correctness, clippy::suspicious, clippy::perf)]
#![allow(clippy::style, clippy::complexity, clippy::pedantic)]

pub mod alloc_counter;
pub mod application;
pub mod backend;
pub mod command_line;
pub mod config;
pub mod core;
pub mod doctor;
#[path = "jwm/features/external_command.rs"]
pub(crate) mod external_command;
pub mod ipc;
pub mod ipc_server;
pub mod jwm;
#[cfg(feature = "remote-x11")]
pub mod remote;
pub mod renderer;
pub mod sync_ext;
pub mod terminal_prober;

pub use jwm::Jwm;

// Xnest and Xephyr is all you need!
// Xnest:
// Xnest :2 -geometry 1024x768 &
// export DISPLAY=:2
// exec jwm

// Xephyr:
// Xephyr :2 -screen 1024x768 &
// DISPLAY=:2 jwm

// For dual monitor:
// xrandr --output HDMI-1 --rotate normal --left-of eDP-1 --auto &
