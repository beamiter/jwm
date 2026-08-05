use egui::{Align, Button, Label, Layout, Stroke};
use log::{info, warn};

use anyhow::Result;
use xbar_core::glass::wallpaper::WallpaperFile;
use xbar_core::glass::{GlassStrip, fallback_rgb};
use xbar_core::{BarEffect, MonitorGeometry, PlatformEffectHandler, UserAction};
use xbar_linux_actions::ProcessActionHandler;

use crate::modules::ModuleRegistry;
use crate::state::AppState;
use crate::theme::{self, colors, icons};

/// Main egui application
pub struct EguiBarApp {
    /// Application state
    state: AppState,
    /// Module registry
    modules: ModuleRegistry,
    process_actions: ProcessActionHandler,
    active_monitor_geometry: Option<MonitorGeometry>,
    last_pixels_per_point: f32,

    // --- Frosted glass ---
    /// Wallpaper source and frosting cache, absent when no wallpaper is
    /// configured. The bar then keeps its opaque panel background.
    glass: Option<GlassStrip<WallpaperFile>>,
    glass_texture: Option<egui::TextureHandle>,
    /// Generation of the strip currently held in `glass_texture`.
    glass_generation: u64,
}

impl EguiBarApp {
    /// Create new application instance
    pub fn new(cc: &eframe::CreationContext<'_>, shared_path: String) -> Result<Self> {
        theme::apply_theme(&cc.egui_ctx);
        let state = AppState::new(shared_path);

        #[cfg(feature = "debug_mode")]
        {
            cc.egui_ctx.set_debug_on_hover(true);
        }

        theme::setup_custom_fonts(&cc.egui_ctx)?;
        theme::configure_text_styles(&cc.egui_ctx);

        let modules = ModuleRegistry::new();

        // Only the `[glass]` section applies here; the rest of this bar's
        // appearance is its own theme module.
        let app_config = xbar_core::config::BarConfig::load_default().unwrap_or_else(|error| {
            warn!("falling back to the default bar config: {error}");
            xbar_core::config::BarConfig::default()
        });
        // A provisional screen size: `refresh_glass` corrects it as soon as
        // eframe knows the real one.
        let (screen_width, screen_height) = monitor_size_px(&cc.egui_ctx).unwrap_or((1920, 1080));
        let glass = app_config.glass.file_strip(
            screen_width,
            screen_height,
            fallback_rgb(app_config.theme),
        );

        Ok(Self {
            state,
            modules,
            process_actions: ProcessActionHandler::default(),
            active_monitor_geometry: None,
            last_pixels_per_point: cc.egui_ctx.pixels_per_point(),
            glass,
            glass_texture: None,
            glass_generation: 0,
        })
    }

    // Main UI via Module System
    // ================================

    fn draw_main_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            // Left modules
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                for module in &mut self.modules.left {
                    module.render_bar(ui, &mut self.state);
                }
            });

            ui.columns(2, |ui| {
                // Center modules
                ui[0].with_layout(Layout::left_to_right(Align::Center), |ui| {
                    for module in &mut self.modules.center {
                        module.render_bar(ui, &mut self.state);
                    }
                });

                // Right modules (RTL)
                ui[1].with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Extra buttons not in module system (debug, screenshot, monitor)
                    self.draw_extra_buttons(ui);

                    // Right modules in reverse order (RTL layout)
                    for module in self.modules.right.iter_mut().rev() {
                        module.render_bar(ui, &mut self.state);
                    }
                });
            });
        });
    }

    /// Extra buttons that remain in app.rs (screenshot, monitor)
    fn draw_extra_buttons(&mut self, ui: &mut egui::Ui) {
        // Screenshot button
        {
            let label_response = ui.add(Button::new(icons::SCREENSHOT_ICON));
            if label_response.clicked() {
                self.state.dispatch(UserAction::Screenshot);
            }
        }

        // Monitor number
        if self.state.snapshot.wm_available {
            let monitor_num = usize::try_from(self.state.snapshot.monitor.0)
                .unwrap_or_default()
                .min(icons::MONITOR_NUMBERS.len().saturating_sub(1));
            ui.add(Label::new(
                egui::RichText::new(icons::MONITOR_NUMBERS[monitor_num].to_string()).strong(),
            ));
        }
    }

    /// Render all module popups
    fn render_popups(&mut self, ctx: &egui::Context) {
        // Render module popups
        for module in self
            .modules
            .right
            .iter_mut()
            .chain(self.modules.left.iter_mut())
            .chain(self.modules.center.iter_mut())
        {
            if module.has_popup() {
                module.render_popup(ctx, &mut self.state);
            }
        }
    }

    fn handle_platform_effects(&mut self, ctx: &egui::Context) {
        for effect in self.state.take_platform_effects() {
            match effect {
                BarEffect::ApplyMonitorGeometry(geometry) => {
                    self.active_monitor_geometry = Some(geometry);
                    Self::apply_monitor_geometry(ctx, geometry);
                }
                BarEffect::ClearMonitorGeometry => {
                    self.active_monitor_geometry = None;
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                        1080.0, 40.0,
                    )));
                }
                effect @ (BarEffect::Screenshot | BarEffect::OpenAudioControl) => {
                    if let Err(error) = self.process_actions.handle(effect) {
                        warn!("Failed to handle platform effect: {error}");
                    }
                }
                BarEffect::WindowManager(command) => {
                    warn!("No WM transport available for command: {command:?}");
                }
                BarEffect::ToggleMute
                | BarEffect::AdjustVolume(_)
                | BarEffect::AdjustBrightness(_)
                | BarEffect::RefreshBattery => {
                    warn!("No enabled runtime adapter handled effect: {effect:?}");
                }
            }
        }
    }

    /// Re-frost the wallpaper under the bar and re-upload it when it changed.
    ///
    /// Costs nothing on an unchanged wallpaper and geometry: the cache hands
    /// back the same generation and the texture is left alone.
    fn refresh_glass(&mut self, ctx: &egui::Context) {
        let Some(strip) = self.glass.as_mut() else {
            return;
        };
        // The wallpaper layout depends on the screen size, and the window
        // manager's own monitor geometry is the trustworthy source for it —
        // eframe's `monitor_size` is a placeholder on X11. Tracked every frame
        // rather than sampled once, so a resolution change is picked up.
        let (screen_width, screen_height) = self
            .active_monitor_geometry
            .map(|geometry| (geometry.width.max(1), geometry.height.max(1)))
            .or_else(|| monitor_size_px(ctx))
            .unwrap_or((1920, 1080));
        strip.source_mut().set_screen(screen_width, screen_height);
        let pixels_per_point = ctx.pixels_per_point().max(f32::EPSILON);
        // egui reports the viewport in points; the wallpaper is in pixels.
        let outer = ctx.input(|input| input.viewport().outer_rect);
        let origin = outer.map_or((0, 0), |rect| {
            (
                (rect.min.x * pixels_per_point).round() as i32,
                (rect.min.y * pixels_per_point).round() as i32,
            )
        });
        let size = ctx.viewport_rect().size() * pixels_per_point;
        let (width, height) = (size.x.round() as u32, size.y.round() as u32);
        if width == 0 || height == 0 {
            return;
        }

        let Some((generation, image)) = strip.ensure(origin.0, origin.1, width, height) else {
            return;
        };
        if generation == self.glass_generation && self.glass_texture.is_some() {
            return;
        }
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [image.width() as usize, image.height() as usize],
            &image.to_rgba8(),
        );
        self.glass_texture =
            Some(ctx.load_texture("glass_backdrop", color_image, egui::TextureOptions::LINEAR));
        self.glass_generation = generation;
    }

    /// Draw the backdrop and the panel tint over it.
    ///
    /// This runs first inside the panel, so everything the modules draw lands
    /// on top of it.
    fn paint_glass(&self, ui: &egui::Ui) {
        let Some(texture) = &self.glass_texture else {
            return;
        };
        let rect = ui.ctx().viewport_rect();
        let painter = ui.painter();
        painter.image(
            texture.id(),
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        // The panel's own fill went transparent so the backdrop could show;
        // this is what puts the theme colour back, as a tint.
        painter.rect_filled(rect, 0.0, colors::BG.gamma_multiply(GLASS_TINT_ALPHA));
    }

    fn apply_monitor_geometry(ctx: &egui::Context, geometry: MonitorGeometry) {
        // eframe's viewport commands use egui points and convert them back to
        // physical winit coordinates. The core geometry is already physical.
        let pixels_per_point = ctx.pixels_per_point().max(f32::EPSILON);
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
            geometry.x as f32 / pixels_per_point,
            geometry.y as f32 / pixels_per_point,
        )));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            geometry.width as f32 / pixels_per_point,
            40.0,
        )));
    }
}

impl eframe::App for EguiBarApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let pixels_per_point = ctx.pixels_per_point();
        if pixels_per_point.to_bits() != self.last_pixels_per_point.to_bits() {
            self.last_pixels_per_point = pixels_per_point;
            if let Some(geometry) = self.active_monitor_geometry {
                Self::apply_monitor_geometry(&ctx, geometry);
            }
        }

        self.state.update();

        // Update all modules
        for module in self
            .modules
            .left
            .iter_mut()
            .chain(self.modules.center.iter_mut())
            .chain(self.modules.right.iter_mut())
        {
            module.update(&self.state);
        }

        #[cfg(feature = "debug_mode")]
        {
            let mut setting = true;
            egui::Window::new("🔧 Settings")
                .open(&mut setting)
                .vscroll(true)
                .show(&ctx, |ui| {
                    ctx.settings_ui(ui);
                });

            egui::Window::new("🔍 Inspection")
                .open(&mut setting)
                .vscroll(true)
                .show(&ctx, |ui| {
                    ctx.inspection_ui(ui);
                });
        }

        self.refresh_glass(&ctx);
        let panel_fill = if self.glass_texture.is_some() {
            // A frosted backdrop is painted inside the panel instead, because
            // the frame's own fill is drawn underneath the content and would
            // otherwise cover it.
            egui::Color32::TRANSPARENT
        } else {
            colors::BG
        };
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(panel_fill)
                    .stroke(Stroke::new(1.0, colors::STROKE_SUBTLE))
                    .inner_margin(egui::Margin::symmetric(10, 2)),
            )
            .show(ui, |ui| {
                self.paint_glass(ui);
                self.draw_main_ui(ui);
                self.render_popups(&ctx);
            });

        self.handle_platform_effects(&ctx);
        ctx.request_repaint_after(std::time::Duration::from_millis(50));

        if self.state.ui_state.need_resize {
            info!("request for resize");
            ctx.request_repaint_after(std::time::Duration::from_millis(1));
        }
    }
}

/// Alpha the panel colour keeps once a frosted backdrop shows through it.
const GLASS_TINT_ALPHA: f32 = 0.55;

/// Physical size of the monitor this bar is on, as eframe reports it.
///
/// Only a plausible answer counts: on X11 eframe reports a `1x1` placeholder
/// until — and in practice past — the point where the window is on screen, and
/// laying a wallpaper out on a one-pixel screen yields no backdrop at all.
fn monitor_size_px(ctx: &egui::Context) -> Option<(u32, u32)> {
    const SMALLEST_PLAUSIBLE: f32 = 240.0;
    let pixels_per_point = ctx.pixels_per_point().max(f32::EPSILON);
    let size = ctx.input(|input| input.viewport().monitor_size)?;
    let width = (size.x * pixels_per_point).round();
    let height = (size.y * pixels_per_point).round();
    (width >= SMALLEST_PLAUSIBLE && height >= SMALLEST_PLAUSIBLE)
        .then_some((width as u32, height as u32))
}
