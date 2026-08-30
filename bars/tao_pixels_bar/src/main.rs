use anyhow::{Context as _, Result};
use log::{info, warn};
use pango::FontDescription;
use pixels::wgpu::TextureFormat;
use pixels::{Pixels, PixelsBuilder, SurfaceTexture};
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
    presentation::{Point, PresentationConfig},
    render::cairo::{CairoBar, PointerButton, PointerInput},
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
    /// The startup compositor answer; the window's depth and the surface's
    /// own alpha-mode probe in `init_window_and_pixels` finish the
    /// translucency decision.
    compositor_active: bool,
    /// The detection connection, kept open to ask the server what depth the
    /// window tao built actually got and how large it currently is.
    x11_side: Option<RustConnection>,
}

impl App {
    fn new(
        bar: CairoBar,
        logical_size: LogicalSize<f64>,
        scale_factor: f64,
        proxy: EventLoopProxy<UserEvent>,
        compositor_active: bool,
        x11_side: Option<RustConnection>,
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
            compositor_active,
            x11_side,
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

        let window = Arc::new(
            WindowBuilder::new()
                .with_title("tao_pixels_bar")
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
        let build = |alpha: Option<pixels::wgpu::CompositeAlphaMode>| {
            let surface_texture = SurfaceTexture::new(safe_width, safe_height, Arc::clone(&window));
            let mut builder = PixelsBuilder::new(safe_width, safe_height, surface_texture)
                .texture_format(TextureFormat::Bgra8UnormSrgb)
                .enable_vsync(true)
                .request_adapter_options(pixels::wgpu::RequestAdapterOptions {
                    power_preference: pixels::wgpu::PowerPreference::LowPower,
                    ..Default::default()
                });
            if let Some(alpha) = alpha {
                builder = builder
                    .alpha_mode(alpha)
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
        // Transparency was only requested above; whether tao's GTK backend
        // delivered a depth-32 window is a separate question, and the one that
        // decides the outcome. On X11 the visual is what carries a frame's
        // alpha to the compositor, so this — not anything the surface reports
        // — is the translucency gate. A swapchain is committed once, so unlike
        // the bars that renegotiate every frame it has to be right the first
        // time: no depth answer means no alpha.
        let argb = window_xid(window.as_ref())
            .zip(self.x11_side.as_ref())
            .map(|(xid, conn)| window_is_argb(conn, xid))
            .unwrap_or(false);
        let want_alpha = self.compositor_active && argb;
        // Ask for premultiplied alpha first: it says exactly what the frame
        // contains, and Mesa's X11 WSI offers it for an ARGB visual.
        //
        // A refusal is *not* the capability answer, though — on X11 the
        // window's visual is what carries alpha to the compositor, and
        // `supportedCompositeAlpha` is close to a formality. Mesa derives it
        // from the visual (depth 32 -> PreMultiplied|Inherit, depth 24 ->
        // Opaque|Inherit) while NVIDIA's X11 driver reports Opaque and nothing
        // else whatever the window is, and wgpu's GL backend hard-codes Opaque
        // for everyone. Measured on NVIDIA 570: a swapchain created Opaque,
        // cleared to premultiplied 50 %, still leaves alpha 127 in a depth-32
        // window's pixmap. So a bar that downgraded here would give up glass
        // on an entire vendor over a reporting quirk.
        //
        // `Auto` is the way through: pixels only validates a mode that isn't
        // `Auto`, and wgpu then resolves it against the surface — Opaque on
        // NVIDIA, Inherit on Mesa. Both deliver the same bytes.
        let (pixels, translucent) = if want_alpha {
            match build(Some(pixels::wgpu::CompositeAlphaMode::PreMultiplied)) {
                Ok(pixels) => (Ok(pixels), true),
                Err(pixels::Error::InvalidAlphaMode(_)) => {
                    (build(Some(pixels::wgpu::CompositeAlphaMode::Auto)), true)
                }
                Err(error) => (Err(error), false),
            }
        } else {
            (build(None), false)
        };
        let pixels =
            pixels.map_err(|error| anyhow::anyhow!("pixels initialization failed: {error}"))?;
        if !translucent {
            self.bar.renderer_mut().set_background_opacity(None);
        }
        // The one line that says which bargain the bar struck. Without it a
        // solid bar is indistinguishable from a translucent one over a dark
        // desktop, and the reason — no compositor, an opaque visual, or a
        // surface that refused — is invisible.
        info!(
            "glass: translucent={translucent} (compositor={}, argb_visual={argb})",
            self.compositor_active
        );

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
        let _ = self.bar.runtime_mut().take_changes();
        pixels
            .render()
            .map_err(|error| anyhow::anyhow!("pixels render failed: {error}"))?;
        Ok(())
    }

    /// Bring the swapchain back in step with the window before presenting.
    ///
    /// `pixels` retries a refused surface acquisition forever, reconfiguring
    /// each time with the size it was last told about. So presenting into a
    /// swapchain that the window manager has already resized past does not
    /// fail — it spins inside `render`, and the resize event that would have
    /// corrected the size is never drained, because draining it needs the
    /// event loop this very call is blocking. A bar that jwm resizes the
    /// instant it maps hits that on its first frame and never paints
    /// anything again.
    ///
    /// The X server is the authority the driver compares the swapchain
    /// against, so ask it rather than the toolkit's cached allocation, which
    /// is only as fresh as the last configure the toolkit has processed.
    fn sync_surface_to_window(&mut self) {
        let Some(size) = self
            .window
            .as_deref()
            .and_then(window_xid)
            .zip(self.x11_side.as_ref())
            .and_then(|(xid, conn)| window_size(conn, xid))
        else {
            return;
        };
        if size.width != self.pixels_width || size.height != self.pixels_height {
            self.resize_pixels(size);
        }
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
        // The WM's ConfigureNotify is authoritative. JWM deliberately owns
        // the reserved status-bar height, which may differ from xbar_core's
        // standalone default; keeping the presentation config in step avoids
        // drawing a shorter bar into a taller window (and requesting the old
        // height again on the next monitor update).
        if let Some(height) = logical_bar_height(size, self.scale_factor) {
            self.bar.config_mut().bar_height = height;
        }
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

    fn handle_pointer_input(&mut self, input: PointerInput) {
        let update = self.bar.handle_pointer(input);
        let needs_redraw = update.needs_redraw();
        self.handle_runtime_update(update.into_runtime());
        // Pressed/hovered visuals can change without producing a runtime
        // effect, so RuntimeUpdate alone is not enough to schedule this frame.
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
                let point = self.last_cursor_pos?;
                let button = match button {
                    MouseButton::Left => Some(PointerButton::Primary),
                    MouseButton::Right => Some(PointerButton::Secondary),
                    MouseButton::Middle | MouseButton::Other(_) => None,
                    _ => None,
                };
                if let Some(button) = button {
                    let input = match state {
                        ElementState::Pressed => PointerInput::Press { point, button },
                        ElementState::Released => PointerInput::Release { point, button },
                        _ => return None,
                    };
                    self.handle_pointer_input(input);
                }
            }
            _ => {}
        }
        None
    }
}

/// A monitor size is only usable when the platform really knows one. An X
/// server without real RandR outputs answers `1x1`, and a bar sized from that
/// would stay invisible until the window manager corrects it.
fn usable_screen_size(size: PhysicalSize<u32>) -> Option<PhysicalSize<u32>> {
    (size.width > 1 && size.height > 1).then_some(size)
}

/// Convert an authoritative physical ConfigureNotify height into the logical
/// height used by the bar model. Toolkit scale factors should always be
/// positive and finite, but rejecting a broken value keeps it out of the
/// long-lived presentation config.
fn logical_bar_height(size: PhysicalSize<u32>, scale_factor: f64) -> Option<f32> {
    if size.height == 0 || !scale_factor.is_finite() || scale_factor <= 0.0 {
        return None;
    }
    let height = f64::from(size.height) / scale_factor;
    (height.is_finite() && height > 0.0 && height <= f64::from(f32::MAX)).then_some(height as f32)
}

/// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
/// selection, asked over a short-lived side connection.
///
/// Sampled once, before the window exists: transparency is a creation-time
/// choice in tao, so a compositor started or stopped after launch is not
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

/// Select the private-use icon preset only when its backing font is installed.
///
/// The resolved family is written back into the presentation configuration so
/// [`CairoBar`] appends exactly the family whose presence authorized the PUA
/// labels. Without one, the stock emoji remain intact and the renderer is not
/// pointed at an unavailable configured family.
fn configure_icon_presentation(
    presentation: &mut PresentationConfig,
    installed_families: &[String],
) -> Option<String> {
    let selected = xbar_core::icon_font::select_installed_nerd_font_family(
        installed_families.iter().map(String::as_str),
        presentation.icon_font.as_deref(),
    );
    presentation.icon_font.clone_from(&selected);
    if selected.is_some() {
        presentation.apply_nerd_font_icons_if_stock();
    }
    selected
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
    // PUA labels are safe only when the font which defines them exists. Keep
    // the portable emoji preset on a stock desktop instead of asking an
    // arbitrary fontconfig fallback to interpret Nerd Font codepoints.
    let installed_icon_fonts = xbar_core::icon_font::installed_families();
    let icon_family = configure_icon_presentation(&mut presentation, &installed_icon_fonts);
    if let Some(family) = &icon_family {
        info!("icons: using {family}");
    } else {
        info!("icons: no installed Nerd Font selected; keeping stock emoji");
    }
    let mut bar = CairoBar::new(
        runtime,
        presentation,
        FontDescription::from_string(&app_config.font),
    );
    // One side connection answers the compositor question and later verifies
    // the window's depth and size; losing the display entirely just means
    // solid mode.
    let (x11_side, compositor_active) = match x11rb::connect(None) {
        Ok((conn, screen_num)) => {
            let active = compositor_active(&conn, screen_num);
            (Some(conn), active)
        }
        Err(_) => (None, false),
    };
    // A translucent background only reads as a material when a compositor is
    // there to blend the desktop behind it; without one the bar keeps the
    // palette background at full opacity. The window's depth and the surface
    // itself still get a veto — `init_window_and_pixels` downgrades to solid
    // if either refuses alpha.
    if compositor_active {
        bar.renderer_mut().set_background_opacity(Some(
            app_config
                .background_opacity
                .unwrap_or(DEFAULT_BACKGROUND_OPACITY),
        ));
    } else {
        bar.renderer_mut().set_background_opacity(None);
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
        compositor_active,
        x11_side,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn families(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn no_nerd_font_keeps_the_stock_emoji_presentation() {
        let stock = PresentationConfig::default();
        let mut presentation = stock.clone();

        assert_eq!(
            configure_icon_presentation(&mut presentation, &families(&["Lato", "Noto Sans"])),
            None
        );
        assert_eq!(presentation.labels, stock.labels);
        assert_eq!(presentation.tag_labels, stock.tag_labels);
        assert_eq!(presentation.icon_font, None);
    }

    #[test]
    fn installed_nerd_font_drives_both_the_preset_and_renderer_configuration() {
        let stock = PresentationConfig::default();
        let mut presentation = stock.clone();
        let selected = configure_icon_presentation(
            &mut presentation,
            &families(&["Lato", "Symbols Nerd Font Mono"]),
        );

        assert_eq!(selected.as_deref(), Some("Symbols Nerd Font Mono"));
        assert_eq!(presentation.icon_font, selected);
        assert_ne!(presentation.labels, stock.labels);
        assert_ne!(presentation.tag_labels, stock.tag_labels);
    }

    #[test]
    fn unavailable_configured_font_does_not_enable_private_use_icons() {
        let stock = PresentationConfig::default();
        let mut presentation = stock.clone();
        presentation.icon_font = Some("Missing Nerd Font".to_owned());

        assert_eq!(
            configure_icon_presentation(
                &mut presentation,
                &families(&["Lato", "Symbols Nerd Font Mono"]),
            ),
            None
        );
        assert_eq!(presentation.labels, stock.labels);
        assert_eq!(presentation.tag_labels, stock.tag_labels);
        assert_eq!(presentation.icon_font, None);
    }

    #[test]
    fn configured_window_height_becomes_the_bar_layout_height() {
        assert_eq!(
            logical_bar_height(PhysicalSize::new(1920, 42), 1.0),
            Some(42.0)
        );
        assert_eq!(
            logical_bar_height(PhysicalSize::new(3840, 84), 2.0),
            Some(42.0)
        );
    }

    #[test]
    fn invalid_configure_height_or_scale_does_not_poison_the_bar_config() {
        assert_eq!(logical_bar_height(PhysicalSize::new(1920, 0), 1.0), None);
        assert_eq!(logical_bar_height(PhysicalSize::new(1920, 42), 0.0), None);
        assert_eq!(
            logical_bar_height(PhysicalSize::new(1920, 42), f64::NAN),
            None
        );
    }
}
