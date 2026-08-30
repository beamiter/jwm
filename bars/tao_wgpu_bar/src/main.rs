use anyhow::{Context as _, Result};
use log::warn;
use pango::FontDescription;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tao::event_loop::EventLoopBuilder;
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::{
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopProxy},
    window::{Window, WindowBuilder, WindowId},
};
use x11rb::rust_connection::RustConnection;
use xbar_core::glass::DEFAULT_BACKGROUND_OPACITY;
use xbar_core::{
    AlignedWakeThread, BarPlacement, BarRuntime, RuntimeUpdate, TransportRecoveryConfig,
    TransportWakeSlot, WakeAck,
    logging::init as initialize_logging,
    presentation::{Point, logical_bar_height},
    render::cairo::{CairoBar, CpuCanvas, PointerButton, PointerInput},
};
use xbar_linux_actions::{EffectRouter, GeometryRequest};
use xbar_present_wgpu::{PresentRect, WgpuPresenter};

const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

// ---------------- Compositor detection ----------------
/// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
/// selection. With one the bar asks tao for a transparent window and paints
/// real alpha for the compositor to blend; without one it paints a solid bar.
/// Sampled once at startup, on a side connection tao never sees, because
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

/// Whether the window tao built really sits on a depth-32 visual. tao falls
/// back to an opaque visual silently, and translucent pixels painted into one
/// of those would render as a dark wash instead of glass.
fn window_is_argb(conn: &RustConnection, xid: u32) -> bool {
    use x11rb::protocol::xproto::ConnectionExt as _;
    conn.get_geometry(xid)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|geometry| geometry.depth == 32)
        .unwrap_or(false)
}

/// The X11 window id behind the tao window, if this is an X11 session with a
/// realized window — tao's GTK backend has no id to give out before that.
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
    scale_factor: f64,
    logical_size: LogicalSize<f64>,
    default_logical_size: LogicalSize<f64>,
    last_physical_size: PhysicalSize<u32>,
    last_cursor_pos: Option<Point>,
    gpu: Option<WgpuPresenter>,
    canvas: CpuCanvas,
    proxy: EventLoopProxy<UserEvent>,
    transport_wake: TransportWakeSlot,
    effects: EffectRouter,
    /// The startup compositor verdict; the depth check and surface
    /// negotiation may still veto translucency.
    compositor_active: bool,
    /// The detection connection, kept open to ask the server what depth the
    /// window tao built actually got.
    x11_side: Option<RustConnection>,
    /// The configured background opacity, applied only when the window ends
    /// up genuinely translucent; a solid bar ignores it and paints opaque.
    background_opacity: Option<f64>,
    /// Whether the translucent-or-solid question has been answered. It stays
    /// open while tao's GTK window has no X id to check yet.
    mode_resolved: bool,
}

impl App {
    fn new(
        bar: CairoBar,
        logical_size: LogicalSize<f64>,
        scale_factor: f64,
        proxy: EventLoopProxy<UserEvent>,
        compositor_active: bool,
        x11_side: Option<RustConnection>,
        background_opacity: Option<f64>,
    ) -> Self {
        Self {
            window_id: None,
            window: None,
            bar,
            scale_factor,
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
            mode_resolved: false,
        }
    }

    fn init_window_and_gpu(&mut self, event_loop: &EventLoop<UserEvent>) -> Result<()> {
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
        self.default_logical_size = self.logical_size;

        let window = Arc::new(
            WindowBuilder::new()
                .with_title("tao_wgpu_bar")
                .with_inner_size(self.logical_size)
                .with_decorations(false)
                .with_resizable(true)
                .with_visible(true)
                .with_transparent(self.compositor_active)
                .build(event_loop)
                .context("failed to create tao window")?,
        );
        let size = window.inner_size();
        let safe_width = size.width.max(1);
        let safe_height = size.height.max(1);
        // Transparency was only requested above; whether tao's GTK backend
        // delivered a depth-32 window is checked against the server. When the
        // X id is not out yet, ask for alpha anyway — the final opacity
        // decision in `resolve_background_mode` re-checks before any
        // translucent pixel is painted.
        let want_alpha = self.compositor_active
            && window_xid(window.as_ref())
                .zip(self.x11_side.as_ref())
                .map(|(xid, conn)| window_is_argb(conn, xid))
                .unwrap_or(true);
        let gpu =
            WgpuPresenter::new_blocking(Arc::clone(&window), safe_width, safe_height, want_alpha)
                .context("failed to initialize wgpu")?;

        self.window_id = Some(window.id());
        self.window = Some(window);
        self.last_physical_size = size;
        self.gpu = Some(gpu);
        self.resolve_background_mode();

        let tick = self.bar.tick();
        self.handle_runtime_update(tick);
        let shared = self.bar.poll_transport();
        self.handle_runtime_update(shared);
        self.sync_transport_wake();
        self.request_redraw();
        Ok(())
    }

    /// Decide translucent versus solid once the window can be interrogated.
    ///
    /// Translucency needs everything to line up: a compositor at startup, a
    /// depth-32 window, and a surface whose alpha bytes actually reach the
    /// compositor. Anything short of that paints fully opaque — a 0.55 wash
    /// over an undefined clear is never acceptable. tao's GTK window may not
    /// expose an X id before it is realized, so this runs again from `redraw`
    /// until it has an answer; the renderer defaults to opaque meanwhile.
    fn resolve_background_mode(&mut self) {
        if self.mode_resolved {
            return;
        }
        let (Some(window), Some(gpu)) = (self.window.as_ref(), self.gpu.as_ref()) else {
            return;
        };
        let xid = window_xid(window.as_ref());
        if xid.is_none() && self.compositor_active {
            return;
        }
        let argb = self.compositor_active
            && xid
                .zip(self.x11_side.as_ref())
                .map(|(xid, conn)| window_is_argb(conn, xid))
                .unwrap_or(false);
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
        self.mode_resolved = true;
    }

    fn redraw(&mut self) -> Result<()> {
        if self.window_id.is_none() || self.gpu.is_none() {
            return Ok(());
        }
        self.resolve_background_mode();
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

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
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
        if let Some(window) = &self.window {
            let placement = match BarPlacement::top(
                geometry,
                f64::from(self.bar.config().bar_height),
                self.scale_factor,
            ) {
                Ok(placement) => placement,
                Err(error) => {
                    warn!("ignoring invalid monitor placement: {error}");
                    return;
                }
            };
            window.set_outer_position(PhysicalPosition::new(placement.x, placement.y));
            window.set_inner_size(PhysicalSize::new(placement.width, placement.height));
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
                self.last_physical_size = size;
                if let Some(height) = logical_bar_height(size.height, self.scale_factor) {
                    self.bar.config_mut().bar_height = height;
                }
                if size.width > 0 && size.height > 0 {
                    self.logical_size = size.to_logical(self.scale_factor);
                    if let Some(gpu) = self.gpu.as_mut() {
                        gpu.resize(size.width, size.height);
                    }
                }
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged {
                scale_factor,
                new_inner_size,
            } => {
                self.scale_factor = scale_factor;
                self.last_physical_size = *new_inner_size;
                if let Some(height) =
                    logical_bar_height(self.last_physical_size.height, self.scale_factor)
                {
                    self.bar.config_mut().bar_height = height;
                }
                self.logical_size = self.last_physical_size.to_logical::<f64>(self.scale_factor);
                if self.last_physical_size.width > 0
                    && self.last_physical_size.height > 0
                    && let Some(gpu) = self.gpu.as_mut()
                {
                    gpu.resize(
                        self.last_physical_size.width,
                        self.last_physical_size.height,
                    );
                }
                if let Some(geometry) = self.bar.runtime().view().geometry {
                    self.apply_monitor_geometry(geometry);
                }
                self.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let position = position.to_logical::<f64>(self.scale_factor);
                let point = Point::new(position.x as f32, position.y as f32);
                self.last_cursor_pos = Some(point);
                self.handle_pointer_input(PointerInput::Move(point));
            }
            WindowEvent::CursorLeft { .. } => {
                self.last_cursor_pos = None;
                self.handle_pointer_input(PointerInput::Leave);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                use tao::event::MouseScrollDelta;
                if let Some(point) = self.last_cursor_pos {
                    let vertical = match delta {
                        MouseScrollDelta::LineDelta(_, value) => f64::from(value),
                        MouseScrollDelta::PixelDelta(position) => position.y,
                        _ => 0.0,
                    };
                    self.handle_pointer_input(PointerInput::Scroll {
                        point,
                        delta_y: vertical,
                    });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                use tao::event::{ElementState, MouseButton};
                if let Some(point) = self.last_cursor_pos {
                    let button = match button {
                        MouseButton::Left => Some(PointerButton::Primary),
                        MouseButton::Right => Some(PointerButton::Secondary),
                        MouseButton::Middle | MouseButton::Other(_) => None,
                        _ => None,
                    };
                    let input = button.and_then(|button| match state {
                        ElementState::Pressed => Some(PointerInput::Press { point, button }),
                        ElementState::Released => Some(PointerInput::Release { point, button }),
                        _ => None,
                    });
                    if let Some(input) = input {
                        self.handle_pointer_input(input);
                    }
                }
            }
            _ => {}
        }
        None
    }
}

/// A monitor size is only usable when the platform really knows one. An X
/// server without real RandR outputs answers `1x1`, and a bar sized from that
/// would be born a sliver, with nothing to correct it later.
fn usable_screen_size(size: PhysicalSize<u32>) -> Option<PhysicalSize<u32>> {
    (size.width > 1 && size.height > 1).then_some(size)
}

fn main() -> Result<()> {
    let shared_path = env::args().skip(1).last().unwrap_or_default();
    initialize_logging("tao_wgpu_bar", &shared_path)?;

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

    let mut event_loop: EventLoop<UserEvent> = EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let tick_proxy = proxy.clone();
    let _tick_forwarder = AlignedWakeThread::spawn(move || tick_proxy.send_event(UserEvent::Tick))?;

    let mut app = App::new(
        bar,
        LogicalSize::new(800.0, 38.0),
        1.0,
        proxy,
        compositing,
        x11_side.map(|(conn, _)| conn),
        app_config.background_opacity,
    );
    app.init_window_and_gpu(&event_loop)?;

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
            Event::MainEventsCleared => {
                let now = Instant::now();
                if app
                    .bar
                    .next_dock_deadline(now)
                    .is_some_and(|deadline| deadline <= now)
                {
                    let update = app.bar.poll_transport();
                    app.handle_runtime_update(update);
                    app.sync_transport_wake();
                }
                if let Some(deadline) = app.bar.next_dock_deadline(Instant::now()) {
                    *control_flow = ControlFlow::WaitUntil(deadline);
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
