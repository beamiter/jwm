//! egui_bar — the shared xbar_core bar, rendered with egui.

mod app;
mod fonts;
mod platform;
mod scene;

use app::{BAR_NAME, EguiBarApp};
use log::{info, warn};
use std::env;
use xbar_core::logging::init as initialize_logging;
use xbar_core::{BarPlacement, MonitorGeometry};

fn main() -> anyhow::Result<()> {
    let shared_path = env::args().skip(1).last().unwrap_or_default();
    initialize_logging(BAR_NAME, &shared_path)?;

    let x11 = platform::X11Session::open()
        .inspect_err(|error| warn!("no X11 connection: {error}"))
        .ok();

    // A compositing manager means the bar can present per-pixel alpha and have
    // the desktop blurred behind it. Asking for a transparent window is what
    // gets winit to pick a 32-bit visual, which is in turn what makes JWM's
    // compositor treat the bar as translucent — but only if the wgpu surface
    // eframe builds can composite alpha at all, which the probe answers up
    // front. Both facts are sampled once, here, because transparency is a
    // creation-time decision: a compositor toggled later is not noticed until
    // the bar restarts, and the claimed selection promises compositing, not
    // that anything is blurred. When either check fails the bar paints the
    // opaque fallback colour instead.
    let translucent = x11
        .as_ref()
        .is_some_and(platform::X11Session::compositor_active)
        && platform::surface_alpha_capable();
    info!("starting {BAR_NAME} (translucent: {translucent})");

    let config = xbar_core::config::BarConfig::load_default().unwrap_or_default();
    let (width, height) = x11
        .as_ref()
        .map_or((1920, 1080), platform::X11Session::screen_size);
    let placement = BarPlacement::at_edge(
        MonitorGeometry {
            x: 0,
            y: 0,
            width,
            height,
        },
        config.presentation.logical_thickness(1.0),
        1.0,
        config.presentation.edge,
    )?;

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(BAR_NAME)
            .with_position(egui::Pos2::new(placement.x as f32, placement.y as f32))
            .with_inner_size([placement.width as f32, placement.height as f32])
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
