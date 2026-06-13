//! Re-exports the backend-independent bitmap text rasterizer.
//!
//! See [`crate::backend::compositor_font`] for the implementation and tests.
pub(super) use crate::backend::compositor_font::render_text_to_rgba;
