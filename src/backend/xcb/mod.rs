pub mod backend;
pub mod batch;
pub mod compositor_protocol;
pub mod present;

pub mod event_coalescer {
    pub use crate::backend::x11::compositor_common::event_coalescer::*;
}
