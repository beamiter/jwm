//! jwm-portal — xdg-desktop-portal ScreenCast backend.
//!
//! Process model:
//!   tokio main runtime hosts the zbus service.
//!   A dedicated OS thread hosts the Wayland client event loop.
//!   A second dedicated OS thread hosts the PipeWire main loop.
//!   Control + frame metadata flow over tokio mpsc + std::sync::Mutex/Arc.

use log::info;

mod dbus;
mod session;
mod wayland;
mod capture;
mod pipewire_stream;
mod picker;
mod ipc;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();

    info!("jwm-portal {} starting", env!("CARGO_PKG_VERSION"));

    let (wayland_snapshot, _wayland_shutdown) = wayland::spawn()?;
    let runtime = session::Runtime::start(wayland_snapshot).await?;
    let _conn = dbus::serve(runtime.clone()).await?;

    // Park the tokio runtime; zbus + spawned tasks own the lifetime.
    futures_util::future::pending::<()>().await;
    Ok(())
}
