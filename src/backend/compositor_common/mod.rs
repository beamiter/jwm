//! Protocol-independent compositor helpers shared by Wayland and X11 backends.

pub mod capture;
pub mod effects;
pub mod event_coalescer;
pub mod math;
pub mod media;
pub mod rules;
pub mod screenshot;
pub mod transitions;
pub mod wallpaper;
pub mod waterlily;
pub(crate) mod window_glow;
pub mod wobbly;
