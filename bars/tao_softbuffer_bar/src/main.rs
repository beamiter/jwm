use anyhow::{Context as _, Result};
use log::warn;
use pango::FontDescription;
use std::env;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::{
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy},
    monitor::MonitorHandle,
    window::{Window, WindowBuilder},
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
#[derive(Debug)]
enum UserEvent {
    Tick,
    SharedUpdated(WakeAck),
}

struct App {
    window: Rc<Window>,
    canvas: CpuCanvas,
    soft_surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    bar: CairoBar,
    scale_factor: f64,
    logical_size: LogicalSize<f64>,
    default_logical_size: LogicalSize<f64>,
    last_physical_size: PhysicalSize<u32>,
    last_cursor_pos: Option<Point>,
    proxy: EventLoopProxy<UserEvent>,
    transport_wake: TransportWakeSlot,
    effects: EffectRouter,
    /// Side connection for the window-depth check that completes the
    /// translucency decision.
    x11: Option<RustConnection>,
    /// True while a compositor was seen at startup but the window's depth has
    /// not been read yet — GTK realizes the native window lazily, so the
    /// check may have to wait for the first frame.
    depth_check_pending: bool,
}

impl App {
    fn new(
        window: Rc<Window>,
        bar: CairoBar,
        logical_size: LogicalSize<f64>,
        scale_factor: f64,
        proxy: EventLoopProxy<UserEvent>,
        x11: Option<RustConnection>,
        compositor_active: bool,
    ) -> Result<Self> {
        let physical_size = window.inner_size();
        let soft_context = softbuffer::Context::new(Rc::clone(&window))
            .map_err(|error| anyhow::anyhow!("failed to create softbuffer context: {error}"))?;
        let mut soft_surface = softbuffer::Surface::new(&soft_context, Rc::clone(&window))
            .map_err(|error| anyhow::anyhow!("failed to create softbuffer surface: {error}"))?;
        resize_soft_surface(&mut soft_surface, physical_size)?;

        Ok(Self {
            window,
            canvas: CpuCanvas::new(),
            soft_surface,
            bar,
            scale_factor,
            logical_size,
            default_logical_size: logical_size,
            last_physical_size: physical_size,
            last_cursor_pos: None,
            proxy,
            transport_wake: TransportWakeSlot::new(true),
            effects: EffectRouter::default(),
            x11,
            depth_check_pending: compositor_active,
        })
    }

    /// Complete the translucency decision once the native window exists.
    ///
    /// A transparent window is a request, not a guarantee: with no ARGB
    /// visual tao silently builds an opaque one, and only a depth-32 window
    /// carries the frame's alpha to the compositor. Anything else drops the
    /// bar back to the fully opaque background.
    fn resolve_translucency(&mut self) {
        if !self.depth_check_pending {
            return;
        }
        use raw_window_handle::HasWindowHandle as _;
        let Ok(handle) = self.window.window_handle() else {
            // Not realized yet; the next frame will ask again.
            return;
        };
        self.depth_check_pending = false;
        let xid = raw_x11_window_id(handle.as_raw());
        let argb = xid
            .zip(self.x11.as_ref())
            .is_some_and(|(xid, conn)| window_is_argb(conn, xid));
        if !argb {
            self.bar.renderer_mut().set_background_opacity(None);
        }
    }

    fn redraw(&mut self) -> Result<()> {
        let PhysicalSize { width, height } = self.last_physical_size;
        if width == 0 || height == 0 {
            return Ok(());
        }

        self.resolve_translucency();
        let frame = self
            .canvas
            .render(&mut self.bar, width, height, self.scale_factor)?;
        let width = width as usize;
        let height = height as usize;
        let mut buffer = self
            .soft_surface
            .buffer_mut()
            .map_err(|error| anyhow::anyhow!("failed to acquire softbuffer frame: {error}"))?;
        if buffer.len() < width * height {
            anyhow::bail!("softbuffer returned an undersized frame");
        }

        let stride = frame.stride as usize;
        if stride == width * 4 {
            let source: &[u32] = bytemuck::cast_slice(&frame.data[..height * stride]);
            buffer[..width * height].copy_from_slice(source);
        } else {
            for y in 0..height {
                let row = &frame.data[y * stride..y * stride + width * 4];
                let source: &[u32] = bytemuck::cast_slice(row);
                buffer[y * width..(y + 1) * width].copy_from_slice(source);
            }
        }
        // The bar is only one narrow scanline band. A complete upload is about
        // 0.4 MiB at 2560x42 and avoids stale title pixels observed when the
        // X11 SHM backend receives a succession of partial-damage presents.
        buffer
            .present()
            .map_err(|error| anyhow::anyhow!("failed to present softbuffer frame: {error}"))?;

        // softbuffer's X11 SHM backend queues PutImage plus a GetInputFocus
        // completion cookie. `present*()` does not wait on that cookie; the
        // wait normally happens when the next buffer is acquired. A status
        // bar may have no next frame until the one-second clock tick, leaving
        // the just-rendered title visibly stale. Reacquiring and immediately
        // releasing the buffer completes that queued submission now.
        drop(
            self.soft_surface
                .buffer_mut()
                .map_err(|error| anyhow::anyhow!("failed to complete softbuffer frame: {error}"))?,
        );
        Ok(())
    }

    fn request_redraw(&self) {
        self.window.request_redraw();
    }

    fn resize(&mut self, physical_size: PhysicalSize<u32>) {
        self.last_physical_size = physical_size;
        if let Some(height) = logical_bar_height(physical_size.height, self.scale_factor) {
            self.bar.config_mut().bar_height = height;
        }
        self.logical_size = physical_size.to_logical(self.scale_factor);
        if let Err(error) = resize_soft_surface(&mut self.soft_surface, physical_size) {
            warn!("failed to resize softbuffer surface: {error:#}");
        }
        self.request_redraw();
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
        if self.apply_runtime_update(update) {
            self.request_redraw();
        }
    }

    fn apply_runtime_update(&mut self, update: RuntimeUpdate) -> bool {
        let mut effects = std::mem::take(&mut self.effects);
        let needs_redraw = effects
            .route::<_, std::convert::Infallible>(update, |request| {
                match request {
                    GeometryRequest::Apply(geometry) => self.apply_monitor_geometry(geometry),
                    GeometryRequest::Clear => {
                        self.window
                            .set_outer_position(LogicalPosition::new(0.0, 0.0));
                        self.window.set_inner_size(self.default_logical_size);
                    }
                }
                Ok(())
            })
            .expect("geometry closure is infallible");
        self.effects = effects;
        needs_redraw
    }

    fn handle_shared_update(&mut self, ack: WakeAck) {
        let update = self.bar.poll_transport();

        // Polling consumed the coalesced notification. Release the forwarder
        // before painting so another rapid workspace switch can wake us while
        // this frame is being presented.
        ack.ack();

        self.apply_transport_update(update);
    }

    fn apply_transport_update(&mut self, update: RuntimeUpdate) {
        let needs_redraw = self.apply_runtime_update(update);
        self.sync_transport_wake();
        if needs_redraw {
            // Transport updates already run on Tao's window thread. Present
            // immediately instead of taking another trip through Tao's redraw
            // queue, while normal expose/resize events retain that queue.
            if let Err(error) = self.redraw() {
                warn!("immediate shared-state redraw failed: {error:#}");
                self.request_redraw();
            }
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
        self.window
            .set_outer_position(PhysicalPosition::new(placement.x, placement.y));
        self.window
            .set_inner_size(PhysicalSize::new(placement.width, placement.height));
    }
}

fn resize_soft_surface(
    surface: &mut softbuffer::Surface<Rc<Window>, Rc<Window>>,
    size: PhysicalSize<u32>,
) -> Result<()> {
    let (Some(width), Some(height)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
    else {
        return Ok(());
    };
    surface
        .resize(width, height)
        .map_err(|error| anyhow::anyhow!("softbuffer resize failed: {error}"))
}

/// A monitor size is only usable when the platform really knows one. An X
/// server without real RandR outputs answers `1x1`, and a bar sized from that
/// would stay invisible until the window manager corrects it.
fn usable_screen_size(size: PhysicalSize<u32>) -> Option<PhysicalSize<u32>> {
    (size.width > 1 && size.height > 1).then_some(size)
}

fn initial_logical_size(
    screen_size: PhysicalSize<u32>,
    scale_factor: f64,
    bar_height: f32,
) -> LogicalSize<f64> {
    LogicalSize::new(
        f64::from(screen_size.width) / scale_factor,
        f64::from(bar_height),
    )
}

/// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
/// selection.
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

fn window_is_argb(conn: &impl x11rb::connection::Connection, xid: u32) -> bool {
    use x11rb::protocol::xproto::ConnectionExt as _;
    conn.get_geometry(xid)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|geometry| geometry.depth == 32)
        .unwrap_or(false)
}

fn raw_x11_window_id(handle: raw_window_handle::RawWindowHandle) -> Option<u32> {
    use raw_window_handle::RawWindowHandle;
    match handle {
        RawWindowHandle::Xlib(handle) => u32::try_from(handle.window).ok(),
        RawWindowHandle::Xcb(handle) => Some(handle.window.get()),
        _ => None,
    }
}

fn main() -> Result<()> {
    let shared_path = env::args().skip(1).last().unwrap_or_default();
    initialize_logging("tao_softbuffer_bar", &shared_path)?;

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
    let font = FontDescription::from_string(&app_config.font);
    let mut bar = CairoBar::new(runtime, presentation, font);
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

    let mut event_loop: EventLoop<UserEvent> = EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let tick_proxy = proxy.clone();
    let _tick_forwarder = AlignedWakeThread::spawn(move || tick_proxy.send_event(UserEvent::Tick))?;

    let primary_monitor: Option<MonitorHandle> = event_loop
        .primary_monitor()
        .or_else(|| event_loop.available_monitors().next());
    let scale_factor = primary_monitor
        .as_ref()
        .map(MonitorHandle::scale_factor)
        .unwrap_or(1.0);
    let screen_size = primary_monitor
        .as_ref()
        .map(MonitorHandle::size)
        .and_then(usable_screen_size)
        .unwrap_or(PhysicalSize::new(1920, 1080));
    let logical_size = initial_logical_size(
        screen_size,
        scale_factor,
        app_config.presentation.bar_height,
    );

    let window = Rc::new(
        WindowBuilder::new()
            .with_title("tao_softbuffer_bar")
            .with_inner_size(logical_size)
            .with_decorations(false)
            .with_resizable(true)
            .with_visible(true)
            .with_transparent(compositor_active)
            .build(&event_loop)
            .context("failed to build tao window")?,
    );
    let mut app = App::new(
        window,
        bar,
        logical_size,
        scale_factor,
        proxy,
        x11,
        compositor_active,
    )?;

    let update = app.bar.tick();
    app.handle_runtime_update(update);
    let update = app.bar.poll_transport();
    app.handle_runtime_update(update);
    app.sync_transport_wake();
    app.request_redraw();
    let exit_code = event_loop.run_return(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::UserEvent(UserEvent::Tick) => {
                app.tick_and_poll();
            }
            Event::UserEvent(UserEvent::SharedUpdated(ack)) => app.handle_shared_update(ack),
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                    *control_flow = ControlFlow::Exit;
                }
                WindowEvent::Resized(size) => app.resize(size),
                WindowEvent::ScaleFactorChanged {
                    scale_factor,
                    new_inner_size,
                } => {
                    app.scale_factor = scale_factor;
                    app.resize(*new_inner_size);
                    if let Some(geometry) = app.bar.runtime().view().geometry {
                        app.apply_monitor_geometry(geometry);
                    }
                }
                WindowEvent::CursorMoved { position, .. } => {
                    let logical = position.to_logical::<f64>(app.scale_factor);
                    let point = Point::new(logical.x as f32, logical.y as f32);
                    app.last_cursor_pos = Some(point);
                    app.handle_pointer_input(PointerInput::Move(point));
                }
                WindowEvent::CursorLeft { .. } => {
                    app.last_cursor_pos = None;
                    app.handle_pointer_input(PointerInput::Leave);
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    use tao::event::{ElementState, MouseButton};
                    if let Some(point) = app.last_cursor_pos {
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
                            app.handle_pointer_input(input);
                        }
                    }
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    use tao::event::MouseScrollDelta;
                    if let Some(point) = app.last_cursor_pos {
                        let y = match delta {
                            MouseScrollDelta::LineDelta(_, y) => f64::from(y),
                            MouseScrollDelta::PixelDelta(position) => position.y,
                            _ => 0.0,
                        };
                        app.handle_pointer_input(PointerInput::Scroll { point, delta_y: y });
                    }
                }
                _ => {}
            },
            Event::RedrawRequested(_) => {
                if let Err(error) = app.redraw() {
                    warn!("redraw failed: {error:#}");
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

    #[test]
    fn initial_size_uses_configured_height_and_monitor_scale() {
        let size = initial_logical_size(PhysicalSize::new(3840, 2160), 2.0, 52.5);

        assert_eq!(size.width, 1920.0);
        assert_eq!(size.height, 52.5);
    }
}
