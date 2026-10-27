//! Native mpv playback, isolated from JWM's compositor process.

#[allow(dead_code)]
#[path = "../jwm_ipc.rs"]
mod jwm_ipc;
#[path = "../player/mod.rs"]
mod player;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();
    if let Err(error) = player::run().await {
        eprintln!("jwm-player: {error}");
        player::notify_error(&error).await;
        std::process::exit(1);
    }
}
