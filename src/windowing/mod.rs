pub mod eframe_backend;

use anyhow::Result;

/// Trait abstracting over different windowing backends
pub trait WindowingBackend {
    fn run(self: Box<Self>) -> Result<()>;
}

/// Select the appropriate backend based on environment
pub fn select_backend(
    shared_path: String,
    transparent: bool,
    height: f32,
    rt_handle: tokio::runtime::Handle,
) -> Box<dyn WindowingBackend> {
    log::info!("Using eframe/X11 backend");
    Box::new(eframe_backend::EframeBackend::new(
        shared_path,
        transparent,
        height,
        rt_handle,
    ))
}
