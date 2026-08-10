//! jwm-bridge — freedesktop D-Bus services backed by jwm's built-in shell.
//!
//! jwm draws notifications itself and keeps their history, but applications
//! speak `org.freedesktop.Notifications` on the session bus. This process is
//! the translation layer, deliberately outside the compositor: the window
//! manager's event loop stays synchronous and D-Bus-free, and a wedged bus
//! cannot stall a frame.
//!
//! Process model:
//!   tokio runtime hosts the zbus service and the signal pump.
//!   One OS thread owns the blocking jwm event subscription.
//!   Commands run on `spawn_blocking` with bounded socket timeouts.

mod jwm_ipc;
mod mpris;
mod notifications;

use jwm_ipc::JwmIpc;

/// Each subscription has its own bounded queue. Once full, the blocking IPC
/// worker stops reading its Unix socket until the async consumer catches up.
const EVENT_QUEUE_CAPACITY: usize = 256;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();

    log::info!("jwm-bridge {} starting", env!("CARGO_PKG_VERSION"));

    let ipc = JwmIpc::new();
    // Fail loudly at startup rather than silently swallowing every
    // notification: without the compositor there is nothing to bridge to.
    if let Err(error) = ipc.query("get_version") {
        log::warn!(
            "jwm is not answering on {} yet ({error}); serving anyway and retrying per request",
            ipc.socket().display()
        );
    }

    // One subscription per consumer keeps the two features independent: a
    // stalled MPRIS call cannot delay a notification signal.
    let (notify_tx, notify_rx) = tokio::sync::mpsc::channel(EVENT_QUEUE_CAPACITY);
    jwm_ipc::subscribe(ipc.clone(), &["notification"], notify_tx);
    let (media_tx, media_rx) = tokio::sync::mpsc::channel(EVENT_QUEUE_CAPACITY);
    jwm_ipc::subscribe(ipc.clone(), &["media"], media_tx);

    let connection = zbus::connection::Builder::session()?
        .name(notifications::NAME)?
        .serve_at(
            notifications::PATH,
            notifications::Notifications::new(ipc.clone()),
        )?
        .build()
        .await?;
    log::info!("D-Bus service registered as {}", notifications::NAME);

    tokio::spawn(mpris::run(connection.clone(), ipc, media_rx));

    notifications::pump_signals(connection, notify_rx).await;
    log::warn!("jwm-bridge event pump ended");
    Ok(())
}
