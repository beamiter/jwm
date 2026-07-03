//! Shared X11 compositor implementation used by the `xcb` and `x11rb` backends.

#[path = "../../shared_x11_compositor/mod.rs"]
mod legacy_impl;

pub use legacy_impl::*;
