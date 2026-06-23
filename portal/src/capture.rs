//! ext-image-copy-capture-v1 client side: per-source capture session.
//!
//! Placeholder for the Phase D MVP — actual frame lifecycle (attach_buffer
//! → damage_buffer → capture → Ready/Failed → relay into PipeWire) is the
//! next slice of work. See [`crate::pipewire_stream`] and the plan.

#![allow(dead_code)]

#[derive(Debug, Default)]
pub struct CaptureSession {
    pub width: u32,
    pub height: u32,
    pub shm_format: Option<u32>,
    pub dmabuf_format: Option<u32>,
}
