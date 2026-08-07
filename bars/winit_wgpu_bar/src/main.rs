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

use x11rb::rust_connection::RustConnection;
use xbar_core::glass::DEFAULT_BACKGROUND_OPACITY;
use xbar_core::{
    AlignedWakeThread, BarRuntime, RuntimeUpdate, TransportRecoveryConfig, TransportWakeSlot,
    WakeAck,
    logging::init as initialize_logging,
    presentation::{Point, PointerAction},
    render::cairo::{CairoBar, CpuCanvas},
};
use xbar_linux_actions::{EffectRouter, GeometryRequest};
use xbar_present_wgpu::{PresentRect, WgpuPresenter};

const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

// ---------------- Compositor detection ----------------
/// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
/// selection. With one the bar asks winit for a transparent window and paints
/// real alpha for the compositor to blend; without one it paints a solid bar.
/// Sampled once at startup, on a side connection winit never sees, because
/// transparency is a window-creation decision — a compositor toggled
/// afterwards needs a bar restart — and owning the selection only promises
/// compositing, not that anything blurs behind us.
fn compositor_active(conn: &RustConnection, screen_num: usize) -> bool {
    use x11rb::protocol::xproto::ConnectionExt as _;
    let name = format!("_NET_WM_CM_S{screen_num}");
    let Ok(cookie) = conn.intern_atom(false, name.as_bytes()) else {
        return false;
    };
    let Ok(atom) = cookie.reply() else {
        return false;
    };
    conn.get_selection_owner(atom.atom)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.owner != x11rb::NONE)
        .unwrap_or(false)
}

/// Whether the window winit built really sits on a depth-32 visual. winit
/// falls back to an opaque visual silently, and translucent pixels painted
/// into one of those would render as a dark wash instead of glass.
fn window_is_argb(conn: &RustConnection, xid: u32) -> bool {
    use x11rb::protocol::xproto::ConnectionExt as _;
    conn.get_geometry(xid)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|geometry| geometry.depth == 32)
        .unwrap_or(false)
}

/// The X11 window id behind the winit window, if this is an X11 session.
fn window_xid(window: &impl raw_window_handle::HasWindowHandle) -> Option<u32> {
    use raw_window_handle::RawWindowHandle;
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Xlib(handle) => u32::try_from(handle.window).ok(),
        RawWindowHandle::Xcb(handle) => Some(handle.window.get()),
        _ => None,
    }
}

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
    /// The startup compositor verdict; the depth check and surface
    /// negotiation in `resumed` may still veto translucency.
    compositor_active: bool,
    /// The detection connection, kept open so `resumed` can ask the server
    /// what depth the window winit built actually got.
    x11_side: Option<RustConnection>,
    /// The configured background opacity, applied only when the window ends
    /// up genuinely translucent; a solid bar ignores it and paints opaque.
    background_opacity: Option<f64>,
}

impl App {
    fn new(
        bar: CairoBar,
        logical_size: LogicalSize<f64>,
        scale: f64,
        proxy: EventLoopProxy<UserEvent>,
        compositor_active: bool,
        x11_side: Option<RustConnection>,
        background_opacity: Option<f64>,
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
            compositor_active,
            x11_side,
            background_opacity,
        }
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
        let frame = self
            .canvas
            .render(&mut self.bar, width, height, self.scale_factor)?;
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

    fn apply_monitor_geometry(&mut self, geometry: xbar_core::MonitorGeometry) {
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
                .and_then(usable_screen_size)
                .unwrap_or(PhysicalSize::new(1920, 1080));
            let width_px = screen_size.width;
            let bar_height = f64::from(self.bar.config().bar_height);

            self.logical_size = LogicalSize::new((width_px as f64) / self.scale_factor, bar_height);
            self.default_logical_size = self.logical_size;

            let attrs = WindowAttributes::default()
                .with_title("winit_wgpu_bar")
                .with_inner_size(self.logical_size)
                .with_decorations(false)
                .with_resizable(true)
                .with_visible(true)
                .with_transparent(self.compositor_active);

            // 创建 Window（owned）
            let window = event_loop
                .create_window(attrs)
                .expect("create_window failed");
            let win_id = window.id();
            let arc = Arc::new(window);

            // Transparency was only requested above; whether it arrived is a
            // separate question, because winit falls back to an opaque visual
            // silently. Ask the server for the window's actual depth.
            let argb = self.compositor_active
                && self
                    .x11_side
                    .as_ref()
                    .zip(window_xid(arc.as_ref()))
                    .map(|(conn, xid)| window_is_argb(conn, xid))
                    .unwrap_or(false);

            // 初始化 wgpu
            let physical_size = arc.inner_size();
            self.last_physical_size = physical_size;

            self.window = Some(arc.clone());
            let gpu = WgpuPresenter::new_blocking(
                arc.clone(),
                physical_size.width,
                physical_size.height,
                argb,
            )
            .expect("wgpu init failed");
            // Only when the surface really carries alpha may the background
            // go translucent; anywhere short of that the bar paints fully
            // opaque — a 0.55 wash over an undefined clear is never
            // acceptable.
            let translucent = argb && gpu.is_transparent();
            self.bar
                .renderer_mut()
                .set_background_opacity(if translucent {
                    Some(
                        self.background_opacity
                            .unwrap_or(DEFAULT_BACKGROUND_OPACITY),
                    )
                } else {
                    None
                });
            self.gpu = Some(gpu);
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

/// A monitor size is only usable when the platform really knows one. An X
/// server without real RandR outputs answers `1x1`, and a bar sized from that
/// would be born a sliver, with nothing to correct it later.
fn usable_screen_size(size: PhysicalSize<u32>) -> Option<PhysicalSize<u32>> {
    (size.width > 1 && size.height > 1).then_some(size)
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
    // The macOS-style template icons every bar renders from.
    presentation.apply_nerd_font_icons_if_stock();
    let bar = CairoBar::new(
        runtime,
        presentation,
        FontDescription::from_string(&app_config.font),
    );

    // A session with no X display at all — native Wayland, say — reads as "no
    // compositor selection", which correctly lands the bar in solid mode.
    let x11_side = x11rb::connect(None).ok();
    let compositing = x11_side
        .as_ref()
        .map(|(conn, screen_num)| compositor_active(conn, *screen_num))
        .unwrap_or(false);

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
        compositing,
        x11_side.map(|(conn, _)| conn),
        app_config.background_opacity,
    );

    // 运行
    event_loop.run_app(&mut app)?;
    Ok(())
}
