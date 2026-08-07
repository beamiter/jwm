use anyhow::Result;
use log::warn;
use pango::FontDescription;
use pixels::wgpu::TextureFormat;
use pixels::{Pixels, PixelsBuilder, SurfaceTexture};
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
    default_logical_size: LogicalSize<f64>,
    last_physical_size: PhysicalSize<u32>,
    last_cursor_pos: Option<Point>,
    pixels: Option<Pixels<'static>>,
    pixels_width: u32,
    pixels_height: u32,
    proxy: EventLoopProxy<UserEvent>,
    transport_wake: TransportWakeSlot,
    effects: EffectRouter,
    /// The startup compositor answer; the window's depth and the surface's
    /// own alpha-mode probe in `resumed` finish the translucency decision.
    compositor_active: bool,
    /// The detection connection, kept open to ask the server what depth the
    /// window winit built actually got and how large it currently is.
    x11: Option<RustConnection>,
}

impl App {
    fn new(
        bar: CairoBar,
        logical_size: LogicalSize<f64>,
        scale_factor: f64,
        proxy: EventLoopProxy<UserEvent>,
        compositor_active: bool,
        x11: Option<RustConnection>,
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
            pixels: None,
            pixels_width: 0,
            pixels_height: 0,
            proxy,
            transport_wake: TransportWakeSlot::new(true),
            effects: EffectRouter::default(),
            compositor_active,
            x11,
        }
    }

    /// Bring the swapchain back in step with the window before presenting.
    ///
    /// `pixels` retries a refused surface acquisition forever, reconfiguring
    /// each time with the size it was last told about. So presenting into a
    /// swapchain that the window manager has already resized past does not
    /// fail — it spins inside `render`, and the resize event that would have
    /// corrected the size is never drained, because draining it needs the
    /// event loop this very call is blocking. winit happens to deliver the
    /// resize before the first redraw today, which is the only reason this
    /// bar has not wedged the way its tao twin does.
    ///
    /// The X server is the authority the driver compares the swapchain
    /// against, so ask it rather than the toolkit's cached allocation, which
    /// is only as fresh as the last configure the toolkit has processed.
    fn sync_surface_to_window(&mut self) {
        let Some(size) = self
            .window
            .as_deref()
            .and_then(window_xid)
            .zip(self.x11.as_ref())
            .and_then(|(xid, conn)| window_size(conn, xid))
        else {
            return;
        };
        if size.width != self.pixels_width || size.height != self.pixels_height {
            self.resize_pixels(size);
        }
    }

    fn redraw(&mut self) -> Result<()> {
        if self.window_id.is_none() || self.pixels.is_none() {
            return Ok(());
        }
        self.sync_surface_to_window();

        let width = self.last_physical_size.width;
        let height = self.last_physical_size.height;
        if width == 0 || height == 0 {
            return Ok(());
        }
        let pixels = self.pixels.as_mut().expect("pixels presence checked above");
        self.bar.render_into_bgra(
            pixels.frame_mut(),
            width,
            height,
            width.saturating_mul(4),
            self.scale_factor,
        )?;
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
        if self.window_id.is_some() {
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
            .with_title("winit_pixels_bar")
            .with_inner_size(self.logical_size)
            .with_decorations(false)
            .with_resizable(true)
            .with_visible(true)
            .with_transparent(self.compositor_active);
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .expect("create_window failed"),
        );
        let window_id = window.id();
        let size = window.inner_size();
        self.last_physical_size = size;

        let safe_width = size.width.max(1);
        let safe_height = size.height.max(1);
        let build = |want_alpha: bool| {
            let surface_texture = SurfaceTexture::new(safe_width, safe_height, Arc::clone(&window));
            let mut builder = PixelsBuilder::new(safe_width, safe_height, surface_texture)
                .texture_format(TextureFormat::Bgra8UnormSrgb)
                .enable_vsync(true)
                .request_adapter_options(pixels::wgpu::RequestAdapterOptions {
                    power_preference: pixels::wgpu::PowerPreference::LowPower,
                    ..Default::default()
                });
            if want_alpha {
                builder = builder
                    .alpha_mode(pixels::wgpu::CompositeAlphaMode::PreMultiplied)
                    .clear_color(pixels::wgpu::Color::TRANSPARENT)
                    // Cairo hands us premultiplied pixels, and pixels' default
                    // blend would multiply them by their alpha a second time,
                    // leaving the bar a shade darker than every other
                    // translucent frontend. The frame covers the whole surface,
                    // so writing it verbatim is both correct and cheaper.
                    .blend_state(pixels::wgpu::BlendState::REPLACE);
            }
            builder.build()
        };
        // Transparency was only requested above; whether winit found a
        // depth-32 visual is a separate question, and one the surface cannot
        // answer for us — wgpu reads its alpha modes from the driver, never
        // from the window's visual. A swapchain is committed once, so unlike
        // the bars that renegotiate every frame this has to be right the
        // first time: no depth answer means no alpha.
        let want_alpha = self.compositor_active
            && window_xid(window.as_ref())
                .zip(self.x11.as_ref())
                .map(|(xid, conn)| window_is_argb(conn, xid))
                .unwrap_or(false);
        // With a compositor around, ask the surface for premultiplied alpha
        // outright — pixels' default `Auto` resolves to Opaque on X11, which
        // would discard the frame's alpha channel. A refusal is the
        // capability answer: the bar then paints the fully opaque background
        // instead of a translucent wash over an undefined clear.
        let (pixels, translucent) = if want_alpha {
            match build(true) {
                Ok(pixels) => (Ok(pixels), true),
                Err(pixels::Error::InvalidAlphaMode(_)) => (build(false), false),
                Err(error) => (Err(error), false),
            }
        } else {
            (build(false), false)
        };
        let pixels = pixels
            .map_err(|error| anyhow::anyhow!("pixels initialization failed: {error}"))
            .expect("pixels create failed");
        if !translucent {
            self.bar.renderer_mut().set_background_opacity(None);
        }

        self.window_id = Some(window_id);
        self.window = Some(window);
        self.pixels_width = safe_width;
        self.pixels_height = safe_height;
        self.pixels = Some(pixels);

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
                self.resize_pixels(size);
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor;
                if let Some(window) = &self.window {
                    self.resize_pixels(window.inner_size());
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
                    let vertical = match delta {
                        MouseScrollDelta::LineDelta(_, value) => f64::from(value),
                        MouseScrollDelta::PixelDelta(position) => position.y,
                    };
                    if let Some(action) = PointerAction::from_vertical_delta(vertical) {
                        self.handle_pointer_action(point, action);
                    }
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
}

/// A monitor size is only usable when the platform really knows one. An X
/// server without real RandR outputs answers `1x1`, and a bar sized from that
/// would stay invisible until the window manager corrects it.
fn usable_screen_size(size: PhysicalSize<u32>) -> Option<PhysicalSize<u32>> {
    (size.width > 1 && size.height > 1).then_some(size)
}

/// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
/// selection, asked over a short-lived side connection.
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

/// The size the X server currently believes the window has — the size the
/// presentation driver validates the swapchain against.
fn window_size(conn: &RustConnection, xid: u32) -> Option<PhysicalSize<u32>> {
    use x11rb::protocol::xproto::ConnectionExt as _;
    let geometry = conn.get_geometry(xid).ok()?.reply().ok()?;
    Some(PhysicalSize::new(
        u32::from(geometry.width),
        u32::from(geometry.height),
    ))
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

fn main() -> Result<()> {
    let shared_path = env::args().skip(1).last().unwrap_or_default();
    initialize_logging("winit_pixels_bar", &shared_path)?;
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
    // One side connection answers the compositor question and later verifies
    // the window's depth and size; losing the display entirely just means
    // solid mode.
    let (x11, compositor_active) = match x11rb::connect(None) {
        Ok((conn, screen_num)) => {
            let active = compositor_active(&conn, screen_num);
            (Some(conn), active)
        }
        Err(_) => (None, false),
    };
    // A translucent background only reads as a material when a compositor is
    // there to blend the desktop behind it; without one the bar keeps the
    // palette background at full opacity. The window's depth and the surface
    // itself still get a veto — `resumed` downgrades to solid if either
    // refuses alpha.
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
        compositor_active,
        x11,
    );
    event_loop.run_app(&mut app)?;
    Ok(())
}
