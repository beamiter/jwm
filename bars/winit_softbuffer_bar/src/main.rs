use anyhow::Result;
use log::warn;
use pango::FontDescription;
use std::env;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};
use winit::event_loop::OwnedDisplayHandle;
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize},
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    window::{Window, WindowAttributes, WindowId},
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

const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

type SoftSurface = softbuffer::Surface<OwnedDisplayHandle, Rc<Window>>;

#[derive(Debug)]
enum UserEvent {
    Tick,
    SharedUpdated(WakeAck),
}

struct App {
    window: Option<Rc<Window>>,
    window_id: Option<WindowId>,
    bar: CairoBar,
    scale_factor: f64,
    logical_size: LogicalSize<f64>,
    default_logical_size: LogicalSize<f64>,
    last_physical_size: PhysicalSize<u32>,
    last_cursor_pos: Option<Point>,
    canvas: CpuCanvas,
    soft_surface: Option<SoftSurface>,
    proxy: EventLoopProxy<UserEvent>,
    transport_wake: TransportWakeSlot,
    effects: EffectRouter,
    /// The startup compositor answer, plus the side connection that gave it —
    /// kept alive so the window's actual depth can be verified once winit has
    /// created it.
    x11: Option<RustConnection>,
    compositor_active: bool,
}

impl App {
    fn new(
        bar: CairoBar,
        logical_size: LogicalSize<f64>,
        scale_factor: f64,
        proxy: EventLoopProxy<UserEvent>,
        x11: Option<RustConnection>,
        compositor_active: bool,
    ) -> Self {
        Self {
            window: None,
            window_id: None,
            bar,
            scale_factor,
            logical_size,
            default_logical_size: logical_size,
            last_physical_size: PhysicalSize::new(
                logical_size.width.round() as u32,
                logical_size.height.round() as u32,
            ),
            last_cursor_pos: None,
            canvas: CpuCanvas::new(),
            soft_surface: None,
            proxy,
            transport_wake: TransportWakeSlot::new(true),
            effects: EffectRouter::default(),
            x11,
            compositor_active,
        }
    }

    fn redraw(&mut self) -> Result<()> {
        let width = self.last_physical_size.width;
        let height = self.last_physical_size.height;
        if self.window.is_none() || width == 0 || height == 0 {
            return Ok(());
        }

        let frame = self
            .canvas
            .render(&mut self.bar, width, height, self.scale_factor)?;
        let surface = match self.soft_surface.as_mut() {
            Some(surface) => surface,
            None => return Ok(()),
        };
        let mut target = surface
            .buffer_mut()
            .map_err(|error| anyhow::anyhow!("softbuffer buffer acquisition failed: {error}"))?;
        let width_usize = width as usize;
        let height_usize = height as usize;
        if target.len() < width_usize * height_usize {
            anyhow::bail!("softbuffer returned an undersized frame");
        }

        let stride = frame.stride as usize;
        if stride == width_usize * 4 {
            let source: &[u32] = bytemuck::cast_slice(&frame.data[..height_usize * stride]);
            target[..width_usize * height_usize].copy_from_slice(source);
        } else {
            for row in 0..height_usize {
                let start = row * stride;
                let source: &[u32] =
                    bytemuck::cast_slice(&frame.data[start..start + width_usize * 4]);
                target[row * width_usize..(row + 1) * width_usize].copy_from_slice(source);
            }
        }
        match frame.damage {
            Some(rect) if !rect.is_empty() => {
                let damage = softbuffer::Rect {
                    x: rect.x,
                    y: rect.y,
                    width: NonZeroU32::new(rect.width).expect("checked non-empty"),
                    height: NonZeroU32::new(rect.height).expect("checked non-empty"),
                };
                target.present_with_damage(&[damage])
            }
            // First frame, a resize, or an unchanged scene after an expose:
            // present everything.
            _ => target.present(),
        }
        .map_err(|error| anyhow::anyhow!("softbuffer present failed: {error}"))?;
        Ok(())
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn resize_surface(&mut self, size: PhysicalSize<u32>) {
        self.last_physical_size = size;
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.logical_size = size.to_logical(self.scale_factor);
        if let Some(surface) = self.soft_surface.as_mut()
            && let (Some(width), Some(height)) =
                (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
            && let Err(error) = surface.resize(width, height)
        {
            warn!("softbuffer resize failed: {error}");
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
            let _ = window.request_inner_size(PhysicalSize::new(placement.width, placement.height));
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

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

        let attributes = WindowAttributes::default()
            .with_title("winit_softbuffer_bar")
            .with_inner_size(self.logical_size)
            .with_decorations(false)
            .with_resizable(true)
            .with_visible(true)
            .with_transparent(self.compositor_active);
        let window = Rc::new(
            event_loop
                .create_window(attributes)
                .expect("create_window failed"),
        );
        // A transparent window is a request, not a guarantee: winit falls
        // back to an opaque visual silently, and only a depth-32 window
        // carries the frame's alpha to the compositor. Anything else drops
        // the bar back to the fully opaque background.
        let translucent = self.compositor_active
            && self.x11.as_ref().is_some_and(|conn| {
                window_xid(window.as_ref()).is_some_and(|xid| window_is_argb(conn, xid))
            });
        if !translucent {
            self.bar.renderer_mut().set_background_opacity(None);
        }
        let context = softbuffer::Context::new(event_loop.owned_display_handle())
            .map_err(|error| anyhow::anyhow!("softbuffer context initialization failed: {error}"))
            .expect("softbuffer context");
        let mut surface = SoftSurface::new(&context, Rc::clone(&window))
            .map_err(|error| anyhow::anyhow!("softbuffer surface initialization failed: {error}"))
            .expect("softbuffer surface");
        let size = window.inner_size();
        if let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        {
            surface
                .resize(width, height)
                .expect("initial softbuffer resize failed");
        }

        self.window_id = Some(window.id());
        self.window = Some(window);
        self.last_physical_size = size;
        self.soft_surface = Some(surface);

        let tick = self.bar.tick();
        self.handle_runtime_update(tick);
        let shared = self.bar.poll_transport();
        self.handle_runtime_update(shared);
        self.sync_transport_wake();
        self.request_redraw();
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Tick => self.tick_and_poll(),
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
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.scale_factor = self
                    .window
                    .as_ref()
                    .map_or(self.scale_factor, |window| window.scale_factor());
                if let Some(height) = logical_bar_height(size.height, self.scale_factor) {
                    self.bar.config_mut().bar_height = height;
                }
                self.resize_surface(size);
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor;
                if let Some(window) = &self.window {
                    self.resize_surface(window.inner_size());
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
            WindowEvent::MouseWheel { delta, .. } => {
                use winit::event::MouseScrollDelta;
                if let Some(point) = self.last_cursor_pos {
                    let vertical = match delta {
                        MouseScrollDelta::LineDelta(_, value) => f64::from(value),
                        MouseScrollDelta::PixelDelta(position) => position.y,
                    };
                    self.handle_pointer_input(PointerInput::Scroll {
                        point,
                        delta_y: vertical,
                    });
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(error) = self.redraw() {
                    warn!("redraw failed: {error}");
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
/// would stay invisible until the window manager corrects it.
fn usable_screen_size(size: PhysicalSize<u32>) -> Option<PhysicalSize<u32>> {
    (size.width > 1 && size.height > 1).then_some(size)
}

/// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
/// selection.
///
/// Sampled once, before the window exists: transparency is a creation-time
/// choice in winit, so a compositor started or stopped after launch is not
/// followed until the bar restarts. Owning the selection also only promises
/// compositing — whether anything blurs behind the bar is the compositor's
/// own policy.
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

/// The X11 window id behind the winit window, if this is an X11 session.
fn window_xid(handle: &impl raw_window_handle::HasWindowHandle) -> Option<u32> {
    use raw_window_handle::RawWindowHandle;
    match handle.window_handle().ok()?.as_raw() {
        RawWindowHandle::Xlib(handle) => u32::try_from(handle.window).ok(),
        RawWindowHandle::Xcb(handle) => Some(handle.window.get()),
        _ => None,
    }
}

fn window_is_argb(conn: &impl x11rb::connection::Connection, xid: u32) -> bool {
    use x11rb::protocol::xproto::ConnectionExt as _;
    conn.get_geometry(xid)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|geometry| geometry.depth == 32)
        .unwrap_or(false)
}

fn main() -> Result<()> {
    let shared_path = env::args().skip(1).last().unwrap_or_default();
    initialize_logging("winit_softbuffer_bar", &shared_path)?;
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
    let mut bar = CairoBar::new(
        runtime,
        presentation,
        FontDescription::from_string(&app_config.font),
    );
    // One side connection answers the compositor question and later verifies
    // the window's depth; losing the display entirely just means solid mode.
    let (x11, compositor_active) = match x11rb::connect(None) {
        Ok((conn, screen_num)) => {
            let active = compositor_active(&conn, screen_num);
            (Some(conn), active)
        }
        Err(_) => (None, false),
    };
    // A translucent background only reads as a material when a compositor is
    // there to blend the desktop behind it; without one the bar keeps the
    // palette background at full opacity.
    if compositor_active {
        bar.renderer_mut().set_background_opacity(Some(
            app_config
                .background_opacity
                .unwrap_or(DEFAULT_BACKGROUND_OPACITY),
        ));
    } else {
        bar.renderer_mut().set_background_opacity(None);
    }

    let event_loop: EventLoop<UserEvent> = EventLoop::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let tick_proxy = proxy.clone();
    let _tick_forwarder = AlignedWakeThread::spawn(move || tick_proxy.send_event(UserEvent::Tick))?;

    let mut app = App::new(
        bar,
        LogicalSize::new(800.0, 38.0),
        1.0,
        proxy,
        x11,
        compositor_active,
    );
    event_loop.run_app(&mut app)?;
    Ok(())
}
