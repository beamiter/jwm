//! Protocol-independent compositor helpers shared by Wayland and X11 backends.

pub mod annotation_overlay;
pub mod capture;
pub(crate) mod debug_hud;
pub(crate) mod dynamic_island;
pub mod effects;
pub mod event_coalescer;
pub mod expose;
pub mod layout_strip;
pub mod math;
pub mod media;
pub(crate) mod osd;
pub mod page_curl;
pub mod rules;
pub mod screenshot;
pub mod screenshot_toolbar;
pub(crate) mod toast;
pub mod transitions;
pub(crate) mod ui_theme;
pub mod wallpaper;
pub mod waterlily;
pub(crate) mod window_glow;
pub mod window_tabs;
pub mod wobbly;
