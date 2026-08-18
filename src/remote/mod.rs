//! JWM-to-JWM remote viewing and input control for trusted X11 LANs.
//!
//! The first implementation deliberately runs out of process.  It reads the
//! shared X Composite overlay used by both JWM X11 backends and injects input
//! through an independent XTEST connection, keeping all socket and JPEG work
//! outside the window manager's latency-sensitive event loop.

use std::error::Error;

pub type RemoteError = Box<dyn Error + Send + Sync>;
pub type RemoteResult<T> = Result<T, RemoteError>;

pub mod client;
pub mod frame;
pub mod host;
pub mod key;
pub mod messages;
pub mod protocol;
pub mod x11_capture;
pub mod x11_input;
pub mod x11_keymap;
pub mod x11_viewer;
