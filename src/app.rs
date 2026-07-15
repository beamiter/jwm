use egui::{Align, Button, Label, Layout, Stroke};
use log::{info, warn};
use std::process::{Child, Command};

use anyhow::Result;
use xbar_core::{BarEffect, MonitorGeometry, UserAction};

use crate::modules::ModuleRegistry;
use crate::state::AppState;
use crate::theme::{self, colors, icons};

/// Main egui application
pub struct EguiBarApp {
    /// Application state
    state: AppState,
    /// Module registry
    modules: ModuleRegistry,
    platform_children: Vec<Child>,
    active_monitor_geometry: Option<MonitorGeometry>,
    last_pixels_per_point: f32,
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

        Ok(Self {
            state,
            modules,
            platform_children: Vec::new(),
            active_monitor_geometry: None,
            last_pixels_per_point: cc.egui_ctx.pixels_per_point(),
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
                BarEffect::Screenshot => self.spawn_platform_helper("flameshot", &["gui"]),
                BarEffect::OpenAudioControl => self.spawn_platform_helper("pavucontrol", &[]),
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

    fn spawn_platform_helper(&mut self, program: &str, args: &[&str]) {
        match Command::new(program).args(args).spawn() {
            Ok(child) => self.platform_children.push(child),
            Err(err) => warn!("Failed to launch {program}: {err}"),
        }
    }

    fn reap_platform_children(&mut self) {
        self.platform_children
            .retain_mut(|child| match child.try_wait() {
                Ok(Some(_)) => false,
                Ok(None) => true,
                Err(err) => {
                    warn!("Failed to reap platform helper: {err}");
                    false
                }
            });
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

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(colors::BG)
                    .stroke(Stroke::new(1.0, colors::STROKE_SUBTLE))
                    .inner_margin(egui::Margin::symmetric(10, 2)),
            )
            .show(ui, |ui| {
                self.draw_main_ui(ui);
                self.render_popups(&ctx);
            });

        self.handle_platform_effects(&ctx);
        self.reap_platform_children();
        ctx.request_repaint_after(std::time::Duration::from_millis(50));

        if self.state.ui_state.need_resize {
            info!("request for resize");
            ctx.request_repaint_after(std::time::Duration::from_millis(1));
        }
    }
}
