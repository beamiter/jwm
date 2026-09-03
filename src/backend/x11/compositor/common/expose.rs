//! X11-specific expose support types.
//!
//! The grid layout and animation algorithm is platform-neutral and lives in
//! [`crate::backend::compositor_common::expose`]; this module instantiates it
//! for u32 XIDs and keeps the X11-only overlay types.

pub use crate::backend::compositor_common::expose::{
    ExposeTickResult, build_expose_entries, tick_expose_entries,
};
pub(crate) use crate::backend::compositor_common::expose::{
    expose_grid_cols, move_expose_selection,
};

/// Expose entry keyed by an X11 window XID.
pub type ExposeEntry = crate::backend::compositor_common::expose::ExposeEntry<u32>;

/// Snap preview state.
pub struct SnapPreview {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub opacity: f32,
    pub start: std::time::Instant,
    pub fading_out: bool,
}
