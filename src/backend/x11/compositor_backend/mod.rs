//! X11 protocol abstractions required by the shared X11 compositor.

pub mod x11_bootstrap;
pub mod x11_composite_redirect;
pub mod x11_connection;
pub mod x11_present;
pub mod x11_randr;
pub mod x11_texture_source;
pub mod x11_window_resource;

pub use x11_bootstrap::*;
pub use x11_composite_redirect::*;
pub use x11_connection::*;
pub use x11_present::*;
pub use x11_randr::*;
pub use x11_texture_source::*;
pub use x11_window_resource::*;
