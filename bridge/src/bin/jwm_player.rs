//! Native mpv playback, isolated from JWM's compositor process.

#[allow(dead_code)]
#[path = "../jwm_ipc.rs"]
mod jwm_ipc;
#[path = "../player/mod.rs"]
mod player;

fn main() {
    // This runs before any runtime or logging thread exists. Operation metadata
    // belongs to this invocation and must never leak into mpv or later helpers.
    let eva_request = std::env::var("JWM_EVA_MEDIA_REQUEST").ok();
    unsafe {
        std::env::remove_var("JWM_EVA_MEDIA_REQUEST");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("player runtime");
    runtime.block_on(run(eva_request));
}
async fn run(eva_request: Option<String>) {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();
    if let Err(error) = player::run(eva_request).await {
        eprintln!("jwm-player: {error}");
        player::notify_error(&error).await;
        std::process::exit(1);
    }
}
