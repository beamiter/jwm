//! egui_bar - A modern system status bar application

mod animation;
mod app;
mod events;
mod ipc;
mod modules;
mod state;
mod theme;

use app::EguiBarApp;
use log::info;
use std::env;
use xbar_core::initialize_logging;

/// Application entry point
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse command line arguments
    let args: Vec<String> = env::args().collect();
    let shared_path = args.get(1).cloned().unwrap_or_default();

    // Initialize logging
    if let Err(e) = initialize_logging("egui_bar", &shared_path) {
        eprintln!("Failed to initialize logging: {}", e);
        std::process::exit(1);
    }

    info!("Starting egui_bar V1.0");

    // Transparent: env var overrides default
    let transparent = env::var("EGUI_BAR_TRANSPARENT")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

    let height = 40.0;

    // Capture the tokio runtime handle so modules (e.g. system tray) can
    // spawn async tasks even from eframe's non-tokio threads.
    let rt_handle = tokio::runtime::Handle::current();

    // Run eframe
    log::info!("Using eframe/X11 backend");
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_position(egui::Pos2::new(0.0, 0.0))
            .with_inner_size([1080.0, height])
            .with_min_inner_size([480.0, height])
            .with_decorations(false)
            .with_resizable(true)
            .with_transparent(transparent),
        vsync: true,
        ..Default::default()
    };

    eframe::run_native(
        "egui_bar",
        native_options,
        Box::new(move |cc| match EguiBarApp::new(cc, shared_path, rt_handle) {
            Ok(app) => {
                info!("Application created successfully");
                Ok(Box::new(app))
            }
            Err(e) => {
                log::error!("Failed to create application: {}", e);
                std::process::exit(1);
            }
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {}", e))
}
