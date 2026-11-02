//! A data-only desktop rendering extension boundary.
//!
//! Untrusted producers run in separate processes. This crate parses fixed-size
//! headers and receives bounded RGBA frames on a worker thread. The compositor
//! only takes the newest complete frame through a nonblocking handoff. There are
//! no plugin entry points, input events, screen pixels, or executable payloads.
//!
//! See `protocol.md` for the language-independent wire contract.

#![cfg(target_os = "linux")]

mod protocol;
mod server;

pub use protocol::{
    AlphaMode, FRAME_MAGIC, HEADER_BYTES, HELLO_MAGIC, Header, Hello, MAX_DIMENSION,
    MAX_FRAME_BYTES, OutputSize, PROTOCOL_VERSION,
};
pub use server::{Frame, Server, Waker};
