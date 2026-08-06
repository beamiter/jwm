//! egui_bar — the shared xbar_core bar, rendered with egui.

mod app;
mod fonts;
mod platform;
mod scene;

use app::{BAR_NAME, EguiBarApp};
use log::{info, warn};
use std::env;
use xbar_core::logging::init as initialize_logging;

fn main() -> anyhow::Result<()> {
    let shared_path = env::args().skip(1).last().unwrap_or_default();
    initialize_logging(BAR_NAME, &shared_path)?;

    let x11 = platform::X11Session::open()
        .inspect_err(|error| warn!("no X11 connection: {error}"))
        .ok();

    // A compositing manager means the bar can present per-pixel alpha and have
    // the desktop blurred behind it. Asking for a transparent window is what
    // gets winit to pick a 32-bit visual, which is in turn what makes JWM's
    // compositor treat the bar as translucent. Without a compositor the window
    // stays opaque and the bar frosts its own wallpaper strip instead.
    let translucent = x11
        .as_ref()
        .is_some_and(platform::X11Session::compositor_active);
    info!("starting {BAR_NAME} (translucent: {translucent})");

    let config = xbar_core::config::BarConfig::load_default().unwrap_or_default();
    let height = config.presentation.bar_height.max(1.0);
    let width = x11
        .as_ref()
        .map_or(1920.0, |session| session.screen_size().0 as f32);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(BAR_NAME)
            .with_position(egui::Pos2::new(0.0, 0.0))
            .with_inner_size([width, height])
            .with_decorations(false)
            .with_resizable(true)
            .with_taskbar(false)
            .with_transparent(translucent),
        ..Default::default()
    };

    eframe::run_native(
        BAR_NAME,
        native_options,
        Box::new(move |cc| {
            EguiBarApp::new(cc, shared_path, x11, translucent)
                .map(|app| Box::new(app) as Box<dyn eframe::App>)
                .map_err(|error| error.into())
        }),
    )
    .map_err(|error| anyhow::anyhow!("eframe error: {error}"))
}
