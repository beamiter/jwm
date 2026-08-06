//! The bar itself: shared runtime in, shared scene out.
//!
//! This file deliberately owns no widgets. Everything visible is built by
//! `xbar_core`'s layout engine and painted by [`crate::scene`], so this bar
//! shows the same cells, glyphs, and colours as `xcb_bar` and inherits any
//! change made to them.

use std::cell::RefCell;
use std::time::Duration;

use anyhow::Result;
use egui::{Color32, FontFamily, Pos2};
use log::{debug, warn};
use x11rb::protocol::xproto::Window;
use xbar_core::glass::wallpaper::WallpaperFile;
use xbar_core::glass::{DEFAULT_BACKGROUND_OPACITY, GlassStrip, fallback_rgb};
use xbar_core::presentation::{
    InteractionState, LayoutEngine, Point, PointerAction, PresentationConfig, PresentationLabels,
    Scene, Size,
};
use xbar_core::{
    BarEffect, BarPlacement, BarRuntime, DockWindowSpec, ModelConfig, MonitorGeometry,
    PlatformEffectHandler, RuntimeSchedule, RuntimeUpdate, TransportRecoveryConfig, UserAction,
};
use xbar_linux_actions::ProcessActionHandler;

use crate::platform::X11Session;
use crate::scene::{ClosureMeasurer, SceneStyle};

pub const BAR_NAME: &str = "egui_bar";

/// How often the bar wakes with no input. The runtime ticks its providers once
/// a second; this only has to be short enough that a transport message is not
/// left sitting until the next tick.
const HEARTBEAT: Duration = Duration::from_millis(100);

pub struct EguiBarApp {
    runtime: BarRuntime,
    schedule: RuntimeSchedule,
    presentation: PresentationConfig,
    /// Height asked for when the bar requests its own geometry. Kept apart
    /// from `presentation.bar_height`, which follows the window the manager
    /// actually gave us so the scene always fills it.
    configured_height: f32,
    interaction: InteractionState,
    scene: Scene,
    style: SceneStyle,
    /// Painted behind the scene when nothing frosted is available.
    fallback: Color32,
    /// True when a compositor blurs behind this window and the bar therefore
    /// presents real per-pixel alpha instead of baking a frosted strip.
    translucent: bool,
    process_actions: ProcessActionHandler,
    x11: Option<X11Session>,
    window: Option<Window>,
    active_monitor_geometry: Option<MonitorGeometry>,
    last_pixels_per_point: f32,

    // --- Baked frosted glass, for sessions with no compositor ---
    glass: Option<GlassStrip<WallpaperFile>>,
    glass_texture: Option<egui::TextureHandle>,
    glass_generation: u64,
}

impl EguiBarApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        shared_path: String,
        x11: Option<X11Session>,
        translucent: bool,
    ) -> Result<Self> {
        let config = xbar_core::config::BarConfig::load_default().unwrap_or_else(|error| {
            warn!("falling back to the default bar config: {error}");
            xbar_core::config::BarConfig::default()
        });

        crate::fonts::install(&cc.egui_ctx, &config.font);

        let mut presentation = config.presentation.clone();
        // Monochrome Nerd Font glyphs tinted by the text colour read like macOS
        // template icons. Only the stock emoji are replaced, so a config that
        // overrides individual labels keeps its customization — the same rule
        // xcb_bar applies.
        if presentation.labels == PresentationLabels::default() {
            presentation.labels = PresentationLabels::nerd_font();
        }
        let configured_height = presentation.bar_height;

        let runtime = if shared_path.is_empty() {
            BarRuntime::new(ModelConfig::default())?
        } else {
            let recovery = TransportRecoveryConfig::new(shared_path, Duration::from_secs(2))?;
            BarRuntime::with_managed_transport(ModelConfig::default(), recovery)?
        };

        let fallback = fallback_rgb(config.theme);
        // Only a bar nobody will blur behind has to frost the wallpaper
        // itself; under a compositor the strip would be a second, staler blur
        // underneath the real one.
        let glass = (!translucent)
            .then(|| {
                let (width, height) = x11
                    .as_ref()
                    .map(X11Session::screen_size)
                    .unwrap_or((1920, 1080));
                config.glass.file_strip(width, height, fallback)
            })
            .flatten();

        let window = crate::platform::window_id(cc)
            .inspect_err(|error| debug!("no X11 window to mark as a dock: {error}"))
            .ok();
        if let (Some(x11), Some(window)) = (x11.as_ref(), window) {
            let (screen_width, _) = x11.screen_size();
            let spec = dock_spec(0, 0, screen_width, configured_height);
            if let Err(error) = x11.apply_dock_spec(window, &spec) {
                warn!("could not announce the bar as a dock: {error}");
            }
        }

        Ok(Self {
            runtime,
            schedule: RuntimeSchedule::default(),
            presentation,
            configured_height,
            interaction: InteractionState::default(),
            scene: Scene {
                viewport: Size::new(0.0, 0.0),
                clip: xbar_core::presentation::Rect::new(0.0, 0.0, 0.0, 0.0),
                nodes: Vec::new(),
                hits: Vec::new(),
            },
            style: SceneStyle {
                family: FontFamily::Proportional,
                background_opacity: config
                    .background_opacity
                    .unwrap_or(DEFAULT_BACKGROUND_OPACITY) as f32,
            },
            fallback: Color32::from_rgb(fallback[0], fallback[1], fallback[2]),
            translucent,
            process_actions: ProcessActionHandler::default(),
            x11,
            window,
            active_monitor_geometry: None,
            last_pixels_per_point: cc.egui_ctx.pixels_per_point(),
            glass,
            glass_texture: None,
            glass_generation: 0,
        })
    }

    /// Build a complete scene with text measured by the font stack that will
    /// paint it.
    fn build_scene(&self, ctx: &egui::Context, viewport: Size) -> Scene {
        let family = self.style.family.clone();
        let presentation = self.presentation.clone();
        // egui only lends its fonts inside this callback. Nothing in here may
        // touch `ctx` again: the callback already holds the context lock.
        ctx.fonts_mut(|fonts| {
            let fonts = RefCell::new(fonts);
            let measurer = ClosureMeasurer(|text: &str, size: f32| {
                if text.is_empty() || !size.is_finite() || size <= 0.0 {
                    return Size::new(0.0, 0.0);
                }
                let galley = fonts.borrow_mut().layout(
                    text.to_owned(),
                    crate::scene::font_id(size, &family),
                    Color32::WHITE,
                    f32::INFINITY,
                );
                // The painted box, not the advance box — see `galley_extents`.
                // Reporting the advance here reserves too little room for an
                // icon that overhangs it, and the pill then clips its glyph.
                let extent = crate::scene::galley_extents(&galley).size();
                Size::new(extent.x, extent.y)
            });
            LayoutEngine::new(presentation, measurer).build(
                self.runtime.view(),
                viewport,
                &self.interaction,
            )
        })
    }

    /// Feed this frame's pointer state through the scene's hit map.
    ///
    /// Returns true when hover changed, which means the scene has to be built
    /// again before it is painted — the same second pass `CairoBar` makes.
    fn handle_pointer(&mut self, ctx: &egui::Context, origin: Pos2) -> bool {
        let pointer = ctx.input(|input| {
            input
                .pointer
                .has_pointer()
                .then(|| input.pointer.latest_pos())
                .flatten()
        });
        let logical = pointer.map(|pos| Point::new(pos.x - origin.x, pos.y - origin.y));

        let mut actions = Vec::new();
        for event in ctx.input(|input| input.events.clone()) {
            match event {
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed: true,
                    ..
                } => {
                    let input = match button {
                        egui::PointerButton::Primary => Some(PointerAction::Primary),
                        egui::PointerButton::Secondary => Some(PointerAction::Secondary),
                        _ => None,
                    };
                    if let Some(input) = input {
                        let point = Point::new(pos.x - origin.x, pos.y - origin.y);
                        actions.extend(self.scene.action_at(point, input));
                    }
                }
                egui::Event::MouseWheel { delta, .. } => {
                    if let (Some(point), Some(input)) =
                        (logical, PointerAction::from_vertical_delta(f64::from(delta.y)))
                    {
                        actions.extend(self.scene.action_at(point, input));
                    }
                }
                _ => {}
            }
        }
        for action in actions {
            self.dispatch(ctx, action);
        }

        let mut interaction = self.interaction;
        let changed = match logical {
            Some(point) => interaction.update_hover(&self.scene, point),
            None => interaction.clear_hover(),
        };
        self.interaction = interaction;
        changed
    }

    fn dispatch(&mut self, ctx: &egui::Context, action: UserAction) {
        let update = self.runtime.dispatch(action);
        self.apply_runtime_update(ctx, update);
    }

    fn apply_runtime_update(&mut self, ctx: &egui::Context, update: RuntimeUpdate) {
        for issue in &update.issues {
            warn!("xbar runtime issue: {issue:?}");
        }
        for effect in update.platform_effects {
            match effect {
                BarEffect::ApplyMonitorGeometry(geometry) => {
                    self.active_monitor_geometry = Some(geometry);
                    self.apply_monitor_geometry(ctx, geometry);
                }
                BarEffect::ClearMonitorGeometry => {
                    self.active_monitor_geometry = None;
                }
                effect @ (BarEffect::Screenshot | BarEffect::OpenAudioControl) => {
                    if let Err(error) = self.process_actions.handle(effect) {
                        warn!("could not run platform effect: {error}");
                    }
                }
                BarEffect::WindowManager(command) => {
                    debug!("no WM transport available for command: {command:?}");
                }
                effect => debug!("no enabled runtime adapter handled effect: {effect:?}"),
            }
        }
    }

    fn apply_monitor_geometry(&self, ctx: &egui::Context, geometry: MonitorGeometry) {
        // Viewport commands are in egui points; the core's geometry is
        // physical pixels.
        let pixels_per_point = ctx.pixels_per_point().max(f32::EPSILON);
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
            geometry.x as f32 / pixels_per_point,
            geometry.y as f32 / pixels_per_point,
        )));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            geometry.width as f32 / pixels_per_point,
            self.configured_height,
        )));

        if let (Some(x11), Some(window)) = (self.x11.as_ref(), self.window) {
            let height = (self.configured_height * pixels_per_point).round().max(1.0) as u32;
            let spec = dock_spec(geometry.x, geometry.y, geometry.width, height as f32);
            if let Err(error) = x11.apply_struts(window, &spec) {
                warn!("could not update the bar's struts: {error}");
            }
        }
    }

    /// Re-frost the wallpaper under the bar and re-upload it when it changed.
    ///
    /// Cheap on an unchanged wallpaper and geometry: the cache hands back the
    /// same generation and the texture is left alone.
    fn refresh_glass(&mut self, ctx: &egui::Context) {
        let Some(strip) = self.glass.as_mut() else {
            return;
        };
        // The window manager's own geometry is the trustworthy screen size —
        // eframe reports a placeholder on X11. Read every frame so a
        // resolution change is picked up.
        let (screen_width, screen_height) = self
            .active_monitor_geometry
            .map(|geometry| (geometry.width.max(1), geometry.height.max(1)))
            .or_else(|| self.x11.as_ref().map(X11Session::screen_size))
            .unwrap_or((1920, 1080));
        strip.source_mut().set_screen(screen_width, screen_height);

        let pixels_per_point = ctx.pixels_per_point().max(f32::EPSILON);
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
}

fn dock_spec(x: i32, y: i32, width: u32, height: f32) -> DockWindowSpec {
    DockWindowSpec::top(
        BAR_NAME,
        BarPlacement {
            x,
            y,
            width,
            height: height.round().clamp(1.0, f32::from(u16::MAX)) as u32,
        },
    )
}

impl eframe::App for EguiBarApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        let pixels_per_point = ctx.pixels_per_point();
        if pixels_per_point.to_bits() != self.last_pixels_per_point.to_bits() {
            self.last_pixels_per_point = pixels_per_point;
            if let Some(geometry) = self.active_monitor_geometry {
                self.apply_monitor_geometry(&ctx, geometry);
            }
        }

        let update = self.schedule.service(&mut self.runtime);
        self.apply_runtime_update(&ctx, update);

        if !self.translucent {
            self.refresh_glass(&ctx);
        }

        let viewport = ctx.viewport_rect();
        let size = Size::new(viewport.width(), viewport.height());
        // A window manager may hand the bar a different height than it asked
        // for. Laying the scene out against the window it actually has is what
        // keeps the background filling it edge to edge.
        self.presentation.bar_height = size.height;

        self.scene = self.build_scene(&ctx, size);
        if self.handle_pointer(&ctx, viewport.min) {
            self.scene = self.build_scene(&ctx, size);
        }

        // Paint straight onto the background layer: the scene owns every pixel
        // of the bar, so a panel frame would only add margins the layout
        // engine has already accounted for.
        let painter = ctx.layer_painter(egui::LayerId::background());
        if let Some(texture) = &self.glass_texture {
            painter.image(
                texture.id(),
                viewport,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        crate::scene::paint(&painter, viewport.min, &self.scene, &self.style);

        ctx.request_repaint_after(HEARTBEAT);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if self.translucent {
            // The compositor blends and blurs behind the bar, so every pixel
            // the scene does not cover must leave the framebuffer clear.
            Color32::TRANSPARENT.to_normalized_gamma_f32()
        } else {
            self.fallback.to_normalized_gamma_f32()
        }
    }
}
