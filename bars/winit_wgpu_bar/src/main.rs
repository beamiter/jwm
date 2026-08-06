use anyhow::Result;
use log::warn;
use pango::FontDescription;
use std::env;
use std::sync::Arc;
use std::time::Duration;
use winit::window::Window;
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize},
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    window::{WindowAttributes, WindowId},
};

use xbar_core::config::GlassConfig;
use xbar_core::glass::wallpaper::WallpaperFile;
use xbar_core::glass::{DEFAULT_BACKGROUND_OPACITY, GlassBackdrop, fallback_rgb};
use xbar_core::{
    AlignedWakeThread, BarRuntime, RuntimeUpdate, TransportRecoveryConfig, TransportWakeSlot,
    WakeAck,
    logging::init as initialize_logging,
    presentation::{Point, PointerAction, PresentationLabels},
    render::cairo::{CairoBar, CpuCanvas},
};
use xbar_linux_actions::{EffectRouter, GeometryRequest};
use xbar_present_wgpu::{PresentRect, WgpuPresenter};

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

    // DPI/尺寸
    scale_factor: f64,
    logical_size: LogicalSize<f64>,
    default_logical_size: LogicalSize<f64>,
    last_physical_size: PhysicalSize<u32>,

    // 最近一次鼠标逻辑坐标
    last_cursor_pos: Option<Point>,

    gpu: Option<WgpuPresenter>,
    canvas: CpuCanvas,
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
        scale: f64,
        proxy: EventLoopProxy<UserEvent>,
        glass_config: GlassConfig,
        glass_fallback: [u8; 3],
    ) -> Self {
        Self {
            window_id: None,
            window: None,
            bar,
            scale_factor: scale,
            logical_size,
            default_logical_size: logical_size,
            last_physical_size: PhysicalSize::new(
                logical_size.width.round() as u32,
                logical_size.height.round() as u32,
            ),
            last_cursor_pos: None,
            gpu: None,
            canvas: CpuCanvas::new(),
            proxy,
            transport_wake: TransportWakeSlot::new(true),
            effects: EffectRouter::default(),
            glass: None,
            glass_config,
            glass_fallback,
        }
    }

    /// Window position in screen coordinates, where the wallpaper is sampled.
    fn bar_origin(&self) -> (i32, i32) {
        self.window
            .as_ref()
            .and_then(|window| window.outer_position().ok())
            .map_or((0, 0), |position| (position.x, position.y))
    }

    fn redraw(&mut self) -> anyhow::Result<()> {
        if self.window_id.is_none() || self.gpu.is_none() {
            return Ok(());
        }

        let width = self.last_physical_size.width;
        let height = self.last_physical_size.height;
        if width == 0 || height == 0 {
            return Ok(());
        }

        // Cairo builds the scene in logical coordinates; the CPU frame stays
        // in physical pixels.
        let (origin_x, origin_y) = self.bar_origin();
        let backdrop = self
            .glass
            .as_mut()
            .and_then(|glass| glass.ensure(origin_x, origin_y, width, height));
        let frame =
            self.canvas
                .render_over(&mut self.bar, width, height, self.scale_factor, backdrop)?;
        let damage = frame.damage.map(|rect| PresentRect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        });
        self.gpu
            .as_mut()
            .expect("GPU presence checked above")
            .present_bgra(frame.data, frame.stride, damage)?;
        Ok(())
    }

    #[inline]
    fn request_redraw(&self) {
        if let Some(win) = self.window.as_ref() {
            win.request_redraw();
        }
    }

    fn update_hover_and_redraw(&mut self, point: Point) {
        if self.bar.pointer_motion(point) {
            self.request_redraw();
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
                            let _ = window.request_inner_size(self.default_logical_size);
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

    fn apply_monitor_geometry(&self, geometry: xbar_core::MonitorGeometry) {
        if let Some(window) = &self.window {
            let height = (f64::from(self.bar.config().bar_height) * self.scale_factor)
                .round()
                .clamp(1.0, f64::from(u32::MAX)) as u32;
            window.set_outer_position(PhysicalPosition::new(geometry.x, geometry.y));
            let _ = window.request_inner_size(PhysicalSize::new(geometry.width, height));
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window_id.is_none() {
            // 初始尺寸：主显示器宽度，bar 高度
            let primary = event_loop
                .primary_monitor()
                .or_else(|| event_loop.available_monitors().next());
            let scale = primary.as_ref().map(|m| m.scale_factor()).unwrap_or(1.0);
            self.scale_factor = scale;

            let screen_size: PhysicalSize<u32> = primary
                .as_ref()
                .map(|m| m.size())
                .unwrap_or(PhysicalSize::new(1920, 1080));
            let width_px = screen_size.width;
            let bar_height = f64::from(self.bar.config().bar_height);

            self.logical_size = LogicalSize::new((width_px as f64) / self.scale_factor, bar_height);
            self.default_logical_size = self.logical_size;
            // The wallpaper is laid out across the whole screen, so the strip
            // the bar frosts depends on the screen size, not the bar's.
            self.glass = self.glass_config.file_backdrop(
                screen_size.width,
                screen_size.height,
                self.glass_fallback,
            );

            let attrs = WindowAttributes::default()
                .with_title("winit_wgpu_bar")
                .with_inner_size(self.logical_size)
                .with_decorations(false)
                .with_resizable(true)
                .with_visible(true)
                .with_transparent(false);

            // 创建 Window（owned）
            let window = event_loop
                .create_window(attrs)
                .expect("create_window failed");
            let win_id = window.id();
            let arc = Arc::new(window);

            // 初始化 wgpu
            let physical_size = arc.inner_size();
            self.last_physical_size = physical_size;

            self.window = Some(arc.clone());
            self.gpu = Some(
                WgpuPresenter::new_blocking(arc.clone(), physical_size.width, physical_size.height)
                    .expect("wgpu init failed"),
            );
            self.window_id = Some(win_id);

            let tick = self.bar.tick();
            self.handle_runtime_update(tick);
            let shared = self.bar.poll_transport();
            self.handle_runtime_update(shared);
            self.sync_transport_wake();
            self.request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Tick => {
                self.tick_and_poll();
            }
            UserEvent::SharedUpdated(_ack) => {
                let update = self.bar.poll_transport();
                self.handle_runtime_update(update);
                self.sync_transport_wake();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if Some(window_id) != self.window_id {
            return;
        }

        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::Resized(new_size) => {
                self.last_physical_size = new_size;
                self.logical_size = new_size.to_logical::<f64>(self.scale_factor);

                if let Some(gpu) = self.gpu.as_mut() {
                    let w = (self.logical_size.width * self.scale_factor).round() as u32;
                    let h = (self.logical_size.height * self.scale_factor).round() as u32;
                    gpu.resize(w, h);
                }

                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor;
                self.logical_size = self.last_physical_size.to_logical::<f64>(self.scale_factor);

                if let Some(gpu) = self.gpu.as_mut() {
                    let w = (self.logical_size.width * self.scale_factor).round() as u32;
                    let h = (self.logical_size.height * self.scale_factor).round() as u32;
                    gpu.resize(w, h);
                }

                if let Some(geometry) = self.bar.runtime().view().geometry {
                    self.apply_monitor_geometry(geometry);
                }

                self.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let logical = position.to_logical::<f64>(self.scale_factor);
                let point = Point::new(logical.x as f32, logical.y as f32);
                self.last_cursor_pos = Some(point);
                self.update_hover_and_redraw(point);
            }
            WindowEvent::CursorLeft { .. } => {
                self.last_cursor_pos = None;
                if self.bar.pointer_leave() {
                    self.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                use winit::event::{ElementState, MouseButton};
                if state == ElementState::Pressed
                    && let Some(point) = self.last_cursor_pos
                {
                    let action = match button {
                        MouseButton::Left => Some(PointerAction::Primary),
                        MouseButton::Right => Some(PointerAction::Secondary),
                        MouseButton::Middle
                        | MouseButton::Back
                        | MouseButton::Forward
                        | MouseButton::Other(_) => None,
                    };
                    if let Some(action) = action {
                        self.handle_pointer_action(point, action);
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                use winit::event::MouseScrollDelta;
                if let Some(point) = self.last_cursor_pos {
                    let y = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y as f64,
                        MouseScrollDelta::PixelDelta(pos) => pos.y,
                    };

                    let action = if y > 0.0 {
                        Some(PointerAction::ScrollUp)
                    } else if y < 0.0 {
                        Some(PointerAction::ScrollDown)
                    } else {
                        None
                    };

                    if let Some(action) = action {
                        self.handle_pointer_action(point, action);
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.redraw() {
                    warn!("redraw error (RedrawRequested): {}", e);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {}
}

fn main() -> Result<()> {
    // 参数
    let args: Vec<String> = env::args().collect();
    let shared_path = args.iter().skip(1).last().cloned().unwrap_or_default();

    // 日志
    if let Err(e) = initialize_logging("winit_wgpu_bar", &shared_path) {
        eprintln!("Failed to initialize logging: {}", e);
        std::process::exit(1);
    }

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

    // 事件循环与代理（winit 0.30.12）
    let event_loop: EventLoop<UserEvent> = EventLoop::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    let tick_proxy = proxy.clone();
    let _tick_forwarder = AlignedWakeThread::spawn(move || tick_proxy.send_event(UserEvent::Tick))?;

    // 初始逻辑尺寸，实际在 resumed 中根据显示器设置
    let logical_size = LogicalSize::new(800.0, 38.0);
    let mut app = App::new(
        bar,
        logical_size,
        1.0,
        proxy,
        app_config.glass.clone(),
        fallback_rgb(app_config.theme),
    );

    // 运行
    event_loop.run_app(&mut app)?;
    Ok(())
}
