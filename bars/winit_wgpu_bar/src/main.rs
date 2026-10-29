#[path = "../../native_x11.rs"]
mod native_x11;

use anyhow::Result;
use log::warn;
use pango::FontDescription;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::window::Window;
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    window::{WindowAttributes, WindowId},
};

use x11rb::rust_connection::RustConnection;
use xbar_core::{
    AlignedWakeThread, BarPlacement, BarRuntime, MonitorGeometry, RuntimeUpdate,
    TransportRecoveryConfig, TransportWakeSlot, WakeAck,
    logging::init as initialize_logging,
    presentation::{Point, logical_bar_height},
    render::cairo::{CairoBar, CpuCanvas, PointerButton, PointerInput},
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

fn window_is_native_wayland(window: &impl raw_window_handle::HasWindowHandle) -> bool {
    matches!(
        window.window_handle().map(|handle| handle.as_raw()),
        Ok(raw_window_handle::RawWindowHandle::Wayland(_))
    )
}

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

fn is_terminal_window_event(event: &WindowEvent) -> bool {
    matches!(event, WindowEvent::CloseRequested | WindowEvent::Destroyed)
}

struct App {
    window_id: Option<WindowId>,
    window: Option<Arc<Window>>,
    bar: CairoBar,

    // DPI/尺寸
    scale_factor: f64,
    logical_size: LogicalSize<f64>,
    default_monitor: MonitorGeometry,
    dock_hints: native_x11::DockHints,
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
            default_monitor: MonitorGeometry {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            dock_hints: native_x11::DockHints::default(),
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
        let update = self.bar.take_pending_runtime();
        if !update.is_empty() {
            self.handle_runtime_update(update);
        }
        Ok(())
    }

    #[inline]
    fn request_redraw(&self) {
        if let Some(win) = self.window.as_ref() {
            win.request_redraw();
        }
    }

    fn handle_pointer_input(&mut self, input: PointerInput) {
        let update = self.bar.handle_pointer(input);
        let needs_redraw = update.needs_redraw();
        self.handle_runtime_update(update.into_runtime());
        if needs_redraw {
            self.request_redraw();
        }
    }

    fn handle_runtime_update(&mut self, update: RuntimeUpdate) {
        let mut effects = std::mem::take(&mut self.effects);
        let needs_redraw = effects
            .route::<_, std::convert::Infallible>(update, |request| {
                match request {
                    GeometryRequest::Apply(geometry) => self.apply_monitor_geometry(geometry),
                    GeometryRequest::Clear => self.place_on_output(),
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

    fn apply_monitor_geometry(&mut self, geometry: MonitorGeometry) {
        let config = self.bar.config();
        match BarPlacement::from_bar_geometry(
            geometry,
            config.logical_thickness(self.scale_factor),
            self.scale_factor,
            config.edge,
        ) {
            Ok(placement) => self.apply_native_placement(placement),
            Err(error) => warn!("ignoring invalid monitor placement: {error}"),
        }
    }

    fn place_on_output(&mut self) {
        if let Some(window) = self.window.as_ref() {
            if let Some(monitor) = window.current_monitor() {
                let origin = monitor.position();
                let size = monitor.size();
                self.default_monitor = MonitorGeometry {
                    x: origin.x,
                    y: origin.y,
                    width: size.width,
                    height: size.height,
                };
            }
        }
        let config = self.bar.config();
        match BarPlacement::at_edge(
            self.default_monitor,
            config.logical_thickness(self.scale_factor),
            self.scale_factor,
            config.edge,
        ) {
            Ok(placement) => self.apply_native_placement(placement),
            Err(error) => warn!("ignoring invalid output placement: {error}"),
        }
    }

    fn desired_placement(&self) -> Result<BarPlacement, xbar_core::placement::PlacementError> {
        let config = self.bar.config();
        match self.bar.runtime().view().geometry {
            Some(geometry) => BarPlacement::from_bar_geometry(
                geometry,
                config.logical_thickness(self.scale_factor),
                self.scale_factor,
                config.edge,
            ),
            None => BarPlacement::at_edge(
                self.default_monitor,
                config.logical_thickness(self.scale_factor),
                self.scale_factor,
                config.edge,
            ),
        }
    }

    fn reapply_geometry(&mut self) {
        if let Some(geometry) = self.bar.runtime().view().geometry {
            self.apply_monitor_geometry(geometry);
        } else {
            self.place_on_output();
        }
    }

    fn apply_native_placement(&mut self, placement: BarPlacement) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        window.set_outer_position(PhysicalPosition::new(placement.x, placement.y));
        let _ = window.request_inner_size(PhysicalSize::new(placement.width, placement.height));
        if let Err(error) = self.dock_hints.sync(
            window.as_ref(),
            placement,
            self.bar.config().edge,
            "winit_wgpu_bar",
        ) {
            warn!("could not update dock hints: {error:#}");
        }
        self.request_redraw();
    }

    fn sync_dock_hints(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let Ok(position) = window.outer_position() else {
            return;
        };
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        let placement = BarPlacement {
            x: position.x,
            y: position.y,
            width: size.width,
            height: size.height,
        };
        if let Err(error) = self.dock_hints.sync(
            window.as_ref(),
            placement,
            self.bar.config().edge,
            "winit_wgpu_bar",
        ) {
            warn!("could not update dock hints: {error:#}");
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
            let origin = primary
                .as_ref()
                .map(|monitor| monitor.position())
                .unwrap_or(PhysicalPosition::new(0, 0));
            self.default_monitor = MonitorGeometry {
                x: origin.x,
                y: origin.y,
                width: screen_size.width,
                height: screen_size.height,
            };
            let initial_placement = BarPlacement::at_edge(
                self.default_monitor,
                self.bar.config().logical_thickness(self.scale_factor),
                self.scale_factor,
                self.bar.config().edge,
            )
            .expect("validated bar thickness and output scale");
            self.logical_size =
                PhysicalSize::new(initial_placement.width, initial_placement.height)
                    .to_logical(self.scale_factor);

            let attrs = WindowAttributes::default()
                .with_title("winit_wgpu_bar")
                .with_inner_size(self.logical_size)
                .with_position(PhysicalPosition::new(
                    initial_placement.x,
                    initial_placement.y,
                ))
                .with_decorations(false)
                .with_resizable(true)
                .with_visible(true)
                .with_transparent(true);

            // 创建 Window（owned）
            let window = event_loop
                .create_window(attrs)
                .expect("create_window failed");
            let win_id = window.id();
            let arc = Arc::new(window);
            if let Err(error) = self.dock_hints.sync(
                arc.as_ref(),
                initial_placement,
                self.bar.config().edge,
                "winit_wgpu_bar",
            ) {
                warn!("could not announce dock: {error:#}");
            }

            // Transparency was only requested above; whether it arrived is a
            // separate question, because winit falls back to an opaque visual
            // silently. Ask the server for the window's actual depth.
            let argb = window_is_native_wayland(arc.as_ref())
                || (self.compositor_active
                    && self
                        .x11_side
                        .as_ref()
                        .zip(window_xid(arc.as_ref()))
                        .map(|(conn, xid)| window_is_argb(conn, xid))
                        .unwrap_or(false));

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
            // The verified native visual and the presenter jointly determine
            // alpha support. X11 ARGB remains valid with an Opaque GLES WSI;
            // other native surfaces keep their advertised capability gate.
            let translucent = argb && gpu.is_transparent();
            self.bar
                .follow_wm_background(self.background_opacity, translucent);
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
            event if is_terminal_window_event(&event) => event_loop.exit(),
            WindowEvent::Resized(new_size) => {
                self.last_physical_size = new_size;
                if let Some(height) = logical_bar_height(
                    self.bar
                        .config()
                        .edge
                        .physical_thickness(new_size.width, new_size.height),
                    self.scale_factor,
                ) {
                    self.bar.config_mut().bar_height = height;
                }
                self.logical_size = new_size.to_logical::<f64>(self.scale_factor);

                if let Some(gpu) = self.gpu.as_mut() {
                    let w = (self.logical_size.width * self.scale_factor).round() as u32;
                    let h = (self.logical_size.height * self.scale_factor).round() as u32;
                    gpu.resize(w, h);
                }

                self.request_redraw();
                self.sync_dock_hints();
            }
            WindowEvent::Moved(_) => self.sync_dock_hints(),
            WindowEvent::ScaleFactorChanged {
                scale_factor,
                mut inner_size_writer,
            } => {
                self.scale_factor = scale_factor;
                if let Ok(placement) = self.desired_placement() {
                    let _ = inner_size_writer
                        .request_inner_size(PhysicalSize::new(placement.width, placement.height));
                }
                self.logical_size = self.last_physical_size.to_logical(self.scale_factor);
                self.last_cursor_pos = None;
                self.handle_pointer_input(PointerInput::Leave);
                self.reapply_geometry();
                self.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let logical = position.to_logical::<f64>(self.scale_factor);
                let point = Point::new(logical.x as f32, logical.y as f32);
                self.last_cursor_pos = Some(point);
                self.handle_pointer_input(PointerInput::Move(point));
            }
            WindowEvent::CursorLeft { .. } => {
                self.last_cursor_pos = None;
                self.handle_pointer_input(PointerInput::Leave);
            }
            WindowEvent::Focused(false) => {
                self.handle_pointer_input(PointerInput::Cancel);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                use winit::event::{ElementState, MouseButton};
                let Some(point) = self.last_cursor_pos else {
                    return;
                };
                let button = match button {
                    MouseButton::Left => Some(PointerButton::Primary),
                    MouseButton::Right => Some(PointerButton::Secondary),
                    MouseButton::Middle
                    | MouseButton::Back
                    | MouseButton::Forward
                    | MouseButton::Other(_) => None,
                };
                if let Some(button) = button {
                    let input = match state {
                        ElementState::Pressed => PointerInput::Press { point, button },
                        ElementState::Released => PointerInput::Release { point, button },
                    };
                    self.handle_pointer_input(input);
                }
            }
            WindowEvent::MouseWheel { delta, phase, .. } => {
                use winit::event::{MouseScrollDelta, TouchPhase};
                use xbar_core::{ScrollDelta, ScrollPhase};
                if let Some(point) = self.last_cursor_pos {
                    let delta = match delta {
                        MouseScrollDelta::LineDelta(x, y) => ScrollDelta::Lines {
                            x: f64::from(x),
                            y: f64::from(y),
                        },
                        MouseScrollDelta::PixelDelta(position) => {
                            ScrollDelta::physical_pixels(position.x, position.y, self.scale_factor)
                        }
                    };
                    let phase = match phase {
                        TouchPhase::Started => ScrollPhase::Started,
                        TouchPhase::Moved => ScrollPhase::Moved,
                        TouchPhase::Ended => ScrollPhase::Ended,
                        TouchPhase::Cancelled => ScrollPhase::Cancelled,
                    };
                    self.handle_pointer_input(PointerInput::Wheel {
                        point,
                        delta,
                        phase,
                    });
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

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if self
            .bar
            .next_dock_deadline(now)
            .is_some_and(|deadline| deadline <= now)
        {
            let update = self.bar.poll_transport();
            self.handle_runtime_update(update);
            self.sync_transport_wake();
        }
        event_loop.set_control_flow(
            self.bar
                .next_dock_deadline(Instant::now())
                .map_or(ControlFlow::Wait, ControlFlow::WaitUntil),
        );
    }
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
    let desktop_header = presentation.apply_desktop_header_profile();
    // The macOS-style template icons every bar renders from.
    presentation.apply_nerd_font_icons_if_stock();
    let mut bar = CairoBar::new(
        runtime,
        presentation,
        FontDescription::from_string(&app_config.font),
    );
    bar.renderer_mut().set_capsule(desktop_header);

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
        app_config
            .background_opacity
            .or(desktop_header.then_some(0.58)),
    );

    // 运行
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_terminal_window_event;
    use winit::{dpi::PhysicalSize, event::WindowEvent};

    #[test]
    fn close_request_and_actual_destruction_both_end_the_bar_session() {
        assert!(is_terminal_window_event(&WindowEvent::CloseRequested));
        assert!(is_terminal_window_event(&WindowEvent::Destroyed));
        assert!(!is_terminal_window_event(&WindowEvent::Resized(
            PhysicalSize::new(1920, 42),
        )));
    }
}
