//! Protocol-independent compositor helpers shared by Wayland and X11 backends.

pub mod annotation_overlay;
pub(crate) mod attention;
pub mod capture;
pub(crate) mod debug_hud;
pub(crate) mod dynamic_island;
pub mod effects;
pub mod event_coalescer;
pub mod expose;
pub(crate) mod genie;
pub mod layout_strip;
pub mod math;
pub mod media;
// Phase-one foundation: the cache is intentionally not connected to backend
// behaviour until both X11 and Wayland can honour its pinned-iconic contract.
#[allow(dead_code)]
pub(crate) mod minimized_thumbnail;
pub(crate) mod osd;
pub mod page_curl;
pub(crate) mod prism;
pub mod recording_nv12;
pub mod recording_sink;
pub mod rules;
pub mod screenshot;
pub mod screenshot_toolbar;
pub(crate) mod system_ui_panel;
pub(crate) mod toast;
pub mod transitions;
pub(crate) mod ui_theme;
pub mod wallpaper;
pub mod waterlily;
pub mod window_animation;
pub(crate) mod window_glow;
pub mod window_tabs;
pub mod wobbly;
