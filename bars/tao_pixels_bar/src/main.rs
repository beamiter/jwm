use anyhow::{Context as _, Result};
use log::warn;
use pango::FontDescription;
use pixels::wgpu::TextureFormat;
use pixels::{Pixels, PixelsBuilder, SurfaceTexture};
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tao::event_loop::EventLoopBuilder;
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::{
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopProxy},
    window::{Window, WindowBuilder, WindowId},
};
use xbar_core::config::GlassConfig;
use xbar_core::glass::wallpaper::WallpaperFile;
use xbar_core::glass::{DEFAULT_BACKGROUND_OPACITY, GlassBackdrop, fallback_rgb};
use xbar_core::{
    AlignedWakeThread, BarRuntime, RuntimeUpdate, TransportRecoveryConfig, TransportWakeSlot,
    WakeAck,
    logging::init as initialize_logging,
    presentation::{Point, PointerAction, PresentationLabels},
    render::cairo::CairoBar,
};
use xbar_linux_actions::{EffectRouter, GeometryRequest};

const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug)]
enum UserEvent {
    Tick,
    SharedUpdated(WakeAck),
}

struct App {
    window_id: Option<WindowId>,
    window: Option<Arc<Window>>,
    bar: CairoBar,
    scale_factor: f64,
    logical_size: LogicalSize<f64>,
    last_physical_size: PhysicalSize<u32>,
    default_logical_size: LogicalSize<f64>,
    last_cursor_pos: Option<Point>,
    pixels: Option<Pixels<'static>>,
    pixels_width: u32,
    pixels_height: u32,
    proxy: EventLoopProxy<UserEvent>,
    transport_wake: TransportWakeSlot,
    effects: EffectRouter,
    /// Frosted backdrop, present only when a wallpaper was configured. These
    /// windows are opaque, so glass here always means a baked strip.
    glass: Option<GlassBackdrop<WallpaperFile>>,
    glass_config: GlassConfig,
    glass_fallback: [u8; 3],
}

impl App {
    fn new(
        bar: CairoBar,
        logical_size: LogicalSize<f64>,
        scale_factor: f64,
        proxy: EventLoopProxy<UserEvent>,
        glass_config: GlassConfig,
        glass_fallback: [u8; 3],
    ) -> Self {
        let physical_size = PhysicalSize::new(
            logical_size.width.round() as u32,
            logical_size.height.round() as u32,
        );
        Self {
            window_id: None,
            window: None,
            bar,
            scale_factor,
            logical_size,
            last_physical_size: physical_size,
            default_logical_size: logical_size,
            last_cursor_pos: None,
            pixels: None,
            pixels_width: 0,
            pixels_height: 0,
            proxy,
            transport_wake: TransportWakeSlot::new(true),
            effects: EffectRouter::default(),
            glass: None,
            glass_config,
            glass_fallback,
        }
    }

    fn init_window_and_pixels(&mut self, event_loop: &EventLoop<UserEvent>) -> Result<()> {
        let primary = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next());
        self.scale_factor = primary
            .as_ref()
            .map_or(1.0, |monitor| monitor.scale_factor());
        let screen_size = primary
            .as_ref()
            .map(|monitor| monitor.size())
            .and_then(usable_screen_size)
            .unwrap_or(PhysicalSize::new(1920, 1080));
        self.logical_size = LogicalSize::new(
            f64::from(screen_size.width) / self.scale_factor,
            f64::from(self.bar.config().bar_height),
        );
        // The wallpaper is laid out across the whole screen, so the strip the
        // bar frosts depends on the screen size, not the bar's.
        self.glass = self.glass_config.file_backdrop(
            screen_size.width,
            screen_size.height,
            self.glass_fallback,
        );

        let window = Arc::new(
            WindowBuilder::new()
                .with_title("tao_pixels_bar")
                .with_inner_size(self.logical_size)
                .with_decorations(false)
                .with_resizable(true)
                .with_visible(true)
                .with_transparent(false)
                .build(event_loop)
                .context("failed to create tao window")?,
        );
        let size = window.inner_size();
        let safe_width = size.width.max(1);
        let safe_height = size.height.max(1);
        let surface_texture = SurfaceTexture::new(safe_width, safe_height, Arc::clone(&window));
        let pixels = PixelsBuilder::new(safe_width, safe_height, surface_texture)
            .texture_format(TextureFormat::Bgra8UnormSrgb)
            .enable_vsync(true)
            .request_adapter_options(pixels::wgpu::RequestAdapterOptions {
                power_preference: pixels::wgpu::PowerPreference::LowPower,
                ..Default::default()
            })
            .build()
            .map_err(|error| anyhow::anyhow!("pixels initialization failed: {error}"))?;

        self.window_id = Some(window.id());
        self.window = Some(window);
        self.last_physical_size = size;
        self.default_logical_size = self.logical_size;
        self.pixels_width = safe_width;
        self.pixels_height = safe_height;
        self.pixels = Some(pixels);

        let tick = self.bar.tick();
        self.handle_runtime_update(tick);
        let shared = self.bar.poll_transport();
        self.handle_runtime_update(shared);
        self.sync_transport_wake();
        self.request_redraw();
        Ok(())
    }

    /// Window position in screen coordinates, where the wallpaper is sampled.
    fn bar_origin(&self) -> (i32, i32) {
        self.window
            .as_ref()
            .and_then(|window| window.outer_position().ok())
            .map_or((0, 0), |position| (position.x, position.y))
    }

    fn redraw(&mut self) -> Result<()> {
        if self.window_id.is_none() || self.pixels.is_none() {
            return Ok(());
        }

        let width = self.last_physical_size.width;
        let height = self.last_physical_size.height;
        if width == 0 || height == 0 {
            return Ok(());
        }
        let (origin_x, origin_y) = self.bar_origin();
        let backdrop = self
            .glass
            .as_mut()
            .and_then(|glass| glass.ensure(origin_x, origin_y, width, height));
        let pixels = self.pixels.as_mut().expect("pixels presence checked above");
        self.bar.render_into_bgra_over(
            pixels.frame_mut(),
            width,
            height,
            width.saturating_mul(4),
            self.scale_factor,
            backdrop,
        )?;
        let _ = self.bar.runtime_mut().take_changes();
        pixels
            .render()
            .map_err(|error| anyhow::anyhow!("pixels render failed: {error}"))?;
        Ok(())
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn resize_pixels(&mut self, size: PhysicalSize<u32>) {
        self.last_physical_size = size;
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.logical_size = size.to_logical(self.scale_factor);
        if self.pixels_width == size.width && self.pixels_height == size.height {
            return;
        }
        if let Some(pixels) = self.pixels.as_mut() {
            let surface_result = pixels.resize_surface(size.width, size.height);
            let buffer_result = pixels.resize_buffer(size.width, size.height);
            if let Err(error) = &surface_result {
                warn!("pixels surface resize failed: {error}");
            }
            if let Err(error) = &buffer_result {
                warn!("pixels buffer resize failed: {error}");
            }
            if surface_result.is_ok() && buffer_result.is_ok() {
                self.pixels_width = size.width;
                self.pixels_height = size.height;
            }
        }
    }

    fn handle_pointer_action(&mut self, point: Point, action: PointerAction) {
        let update = self.bar.pointer_action(point, action);
        self.handle_runtime_update(update);
    }

    fn handle_runtime_update(&mut self, update: RuntimeUpdate) {
        let mut effects = std::mem::take(&mut self.effects);
        let needs_redraw = effects
            .route::<_, std::convert::Infallible>(update, |request| {
                match request {
                    GeometryRequest::Apply(geometry) => self.apply_monitor_geometry(geometry),
                    GeometryRequest::Clear => {
                        if let Some(window) = &self.window {
                            window.set_outer_position(LogicalPosition::new(0.0, 0.0));
                            window.set_inner_size(self.default_logical_size);
                        }
                    }
                }
                Ok(())
            })
            .expect("geometry closure is infallible");
        self.effects = effects;
        if needs_redraw {
            self.request_redraw();
        }
    }

    fn tick_and_poll(&mut self) {
        let mut update = self.bar.tick();
        update.merge(self.bar.poll_transport());
        self.handle_runtime_update(update);
        self.sync_transport_wake();
    }

    fn sync_transport_wake(&mut self) {
        let proxy = self.proxy.clone();
        if let Err(error) = self.transport_wake.sync(self.bar.runtime(), move |ack| {
            proxy.send_event(UserEvent::SharedUpdated(ack))
        }) {
            warn!("failed to synchronize shared transport wake: {error}");
        }
    }

    fn apply_monitor_geometry(&mut self, geometry: xbar_core::MonitorGeometry) {
        self.relayout_wallpaper(geometry.width, geometry.height);
        if let Some(window) = &self.window {
            let height = (f64::from(self.bar.config().bar_height) * self.scale_factor)
                .round()
                .clamp(1.0, f64::from(u32::MAX)) as u32;
            window.set_outer_position(PhysicalPosition::new(geometry.x, geometry.y));
            window.set_inner_size(PhysicalSize::new(geometry.width, height));
        }
    }

    /// Lay the wallpaper out on the display the window manager just described.
    ///
    /// This size is authoritative: the platform's own monitor query is only a
    /// startup guess, and a wrong one would leave the backdrop wrong for the
    /// life of the process because nothing else re-lays it.
    fn relayout_wallpaper(&mut self, width: u32, height: u32) {
        if let Some(glass) = self.glass.as_mut() {
            glass.source_mut().set_screen(width, height);
        }
    }

    fn on_user_event(&mut self, event: UserEvent) {
        match event {
            UserEvent::Tick => self.tick_and_poll(),
            UserEvent::SharedUpdated(_ack) => {
                let update = self.bar.poll_transport();
                self.handle_runtime_update(update);
                self.sync_transport_wake();
            }
        }
    }

    fn on_window_event(&mut self, window_id: WindowId, event: WindowEvent) -> Option<ControlFlow> {
        if Some(window_id) != self.window_id {
            return None;
        }

        match event {
            WindowEvent::CloseRequested => return Some(ControlFlow::Exit),
            WindowEvent::Resized(size) => {
                self.resize_pixels(size);
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged {
                scale_factor,
                new_inner_size,
            } => {
                self.scale_factor = scale_factor;
                self.resize_pixels(*new_inner_size);
                if let Some(geometry) = self.bar.runtime().view().geometry {
                    self.apply_monitor_geometry(geometry);
                }
                self.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let position = position.to_logical::<f64>(self.scale_factor);
                let point = Point::new(position.x as f32, position.y as f32);
                self.last_cursor_pos = Some(point);
                if self.bar.pointer_motion(point) {
                    self.request_redraw();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.last_cursor_pos = None;
                if self.bar.pointer_leave() {
                    self.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                use tao::event::MouseScrollDelta;
                if let Some(point) = self.last_cursor_pos {
                    let vertical = match delta {
                        MouseScrollDelta::LineDelta(_, value) => f64::from(value),
                        MouseScrollDelta::PixelDelta(position) => position.y,
                        _ => 0.0,
                    };
                    if let Some(action) = PointerAction::from_vertical_delta(vertical) {
                        self.handle_pointer_action(point, action);
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                use tao::event::{ElementState, MouseButton};
                if state == ElementState::Pressed
                    && let Some(point) = self.last_cursor_pos
                {
                    let action = match button {
                        MouseButton::Left => Some(PointerAction::Primary),
                        MouseButton::Right => Some(PointerAction::Secondary),
                        MouseButton::Middle | MouseButton::Other(_) => None,
                        _ => None,
                    };
                    if let Some(action) = action {
                        self.handle_pointer_action(point, action);
                    }
                }
            }
            _ => {}
        }
        None
    }
}

/// A monitor size is only usable when the platform really knows one. An X
/// server without real RandR outputs answers `1x1`, and a wallpaper laid out on
/// that leaves the bar showing a flat fallback color instead of glass, with
/// nothing to correct it later.
fn usable_screen_size(size: PhysicalSize<u32>) -> Option<PhysicalSize<u32>> {
    (size.width > 1 && size.height > 1).then_some(size)
}

fn main() -> Result<()> {
    let shared_path = env::args().skip(1).last().unwrap_or_default();
    initialize_logging("tao_pixels_bar", &shared_path)?;

    let app_config = xbar_core::config::BarConfig::load_default()?;
    let runtime = if shared_path.is_empty() {
        BarRuntime::new(app_config.model_config())?
    } else {
        let recovery = TransportRecoveryConfig::new(shared_path.clone(), TRANSPORT_RETRY_INTERVAL)?;
        BarRuntime::with_managed_transport(app_config.model_config(), recovery)?
    };
    let mut presentation = app_config.presentation.clone();
    // Monochrome Nerd Font glyphs tinted by the text color read like macOS
    // template icons; only replace the stock emoji so a config that overrides
    // individual labels keeps its customization. Every other bar makes exactly
    // this substitution.
    if presentation.labels == PresentationLabels::default() {
        presentation.labels = PresentationLabels::nerd_font();
    }
    let mut bar = CairoBar::new(
        runtime,
        presentation,
        FontDescription::from_string(&app_config.font),
    );
    // A frosted backdrop only reads as a material if the bar's own background
    // lets some of it through, so glass changes what "no opacity configured"
    // should mean.
    match app_config.background_opacity {
        Some(opacity) => bar.renderer_mut().set_background_opacity(Some(opacity)),
        None if app_config.glass.wallpaper.is_some() => bar
            .renderer_mut()
            .set_background_opacity(Some(DEFAULT_BACKGROUND_OPACITY)),
        None => {}
    }

    let mut event_loop: EventLoop<UserEvent> = EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let tick_proxy = proxy.clone();
    let _tick_forwarder = AlignedWakeThread::spawn(move || tick_proxy.send_event(UserEvent::Tick))?;

    let mut app = App::new(
        bar,
        LogicalSize::new(800.0, 38.0),
        1.0,
        proxy,
        app_config.glass.clone(),
        fallback_rgb(app_config.theme),
    );
    app.init_window_and_pixels(&event_loop)?;

    let exit_code = event_loop.run_return(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(event) => app.on_user_event(event),
            Event::WindowEvent {
                window_id, event, ..
            } => {
                if let Some(next) = app.on_window_event(window_id, event) {
                    *control_flow = next;
                }
            }
            Event::RedrawRequested(window_id) if Some(window_id) == app.window_id => {
                if let Err(error) = app.redraw() {
                    warn!("redraw failed: {error}");
                }
            }
            _ => {}
        }
    });

    if exit_code == 0 {
        Ok(())
    } else {
        anyhow::bail!("tao event loop exited with status {exit_code}")
    }
}
