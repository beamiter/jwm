// Prevents an additional console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;

use xbar_core::logging::init as initialize_logging;

fn main() {
    #[cfg(target_os = "linux")]
    unsafe {
        env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
    }

    let shared_path = env::args().nth(1).unwrap_or_default();
    if let Err(error) = initialize_logging("tauri_svelte_bar", &shared_path) {
        eprintln!("failed to initialize logging: {error}");
    }

    let builder = tauri::Builder::default().plugin(tauri_plugin_opener::init());
    let builder = xbar_tauri::configure(builder, xbar_tauri::BridgeConfig::new(shared_path))
        .expect("invalid xbar Tauri bridge configuration");

    builder
        .run(tauri::generate_context!())
        .expect("error while running Tauri application");
}
