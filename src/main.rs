use anyhow::{Context as _, Result};
use cairo::{Context as CairoContext, Format, ImageSurface};
use log::warn;
use pango::FontDescription;
use pixels::wgpu::TextureFormat;
use pixels::{Pixels, PixelsBuilder, SurfaceTexture};
use std::env;
use std::os::fd::AsRawFd;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tao::event_loop::EventLoopBuilder;
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::{
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopProxy},
    window::{Window, WindowBuilder, WindowId},
};
use xbar_core::{
    BarEffect, BarRuntime, ModelConfig, RuntimeUpdate, SharedEventNotifier, SharedTransport,
    logging::init as initialize_logging,
    presentation::{Point, PointerAction, PresentationConfig, Size},
    render::cairo::CairoBar,
};

const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
enum UserEvent {
    Tick,
    SharedUpdated(Arc<AtomicBool>),
}

/// Owns a forwarding thread and gives it a bounded shutdown path.
struct EventForwarder {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Drop for EventForwarder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && let Err(payload) = worker.join()
        {
            warn!("event forwarding thread panicked: {payload:?}");
        }
    }
}

fn spawn_tick_thread(proxy: EventLoopProxy<UserEvent>) -> EventForwarder {
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker = thread::spawn(move || {
        while !worker_stop.load(Ordering::Acquire) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO);
            let subsecond_nanos = u64::from(now.subsec_nanos());
            thread::sleep(Duration::from_nanos(
                1_000_000_000_u64.saturating_sub(subsecond_nanos).max(1),
            ));
            if worker_stop.load(Ordering::Acquire) || proxy.send_event(UserEvent::Tick).is_err() {
                break;
            }
        }
    });
    EventForwarder {
        stop,
        worker: Some(worker),
    }
}

fn spawn_shared_thread(
    proxy: EventLoopProxy<UserEvent>,
    notifier: Option<SharedEventNotifier>,
) -> Option<EventForwarder> {
    notifier.map(|notifier| {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        // The event-loop handler clears this only after it has drained the
        // transport, so at most one shared update can be queued at a time.
        let worker_pending = Arc::new(AtomicBool::new(false));
        let worker = thread::spawn(move || {
            let mut descriptor = libc::pollfd {
                fd: notifier.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            while !worker_stop.load(Ordering::Acquire) {
                descriptor.revents = 0;
                let ready = unsafe { libc::poll(&mut descriptor, 1, 250) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    warn!("shared notifier poll failed: {error}");
                    break;
                }
                if ready == 0 {
                    continue;
                }
                if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    warn!("shared notifier fd became unusable: {}", descriptor.revents);
                    break;
                }
                if descriptor.revents & libc::POLLIN != 0 {
                    match notifier.drain() {
                        Ok(0) => {}
                        Ok(_) => {
                            if worker_pending
                                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                                .is_ok()
                            {
                                let event = UserEvent::SharedUpdated(Arc::clone(&worker_pending));
                                if proxy.send_event(event).is_err() {
                                    worker_pending.store(false, Ordering::Release);
                                    break;
                                }
                            }
                            while worker_pending.load(Ordering::Acquire)
                                && !worker_stop.load(Ordering::Acquire)
                            {
                                thread::sleep(Duration::from_millis(10));
                            }
                        }
                        Err(error) => {
                            warn!("shared notifier drain failed: {error}");
                            break;
                        }
                    }
                }
            }
        });
        EventForwarder {
            stop,
            worker: Some(worker),
        }
    })
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
    shared_path: String,
    last_transport_attempt: Instant,
}

impl App {
    fn new(
        bar: CairoBar,
        logical_size: LogicalSize<f64>,
        scale_factor: f64,
        shared_path: String,
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
            shared_path,
            last_transport_attempt: Instant::now(),
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
            .map_or(PhysicalSize::new(1920, 1080), |monitor| monitor.size());
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
        self.request_redraw();
        Ok(())
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
        let width_i32 = i32::try_from(width).context("window width does not fit Cairo")?;
        let height_i32 = i32::try_from(height).context("window height does not fit Cairo")?;
        let stride = width_i32
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("Cairo stride overflow"))?;

        let pixels = self.pixels.as_mut().expect("pixels presence checked above");
        {
            let frame = pixels.frame_mut();
            let required = usize::try_from(stride)?
                .checked_mul(usize::try_from(height_i32)?)
                .ok_or_else(|| anyhow::anyhow!("frame size overflow"))?;
            if frame.len() < required {
                anyhow::bail!(
                    "pixels frame is too small: expected {required}, got {}",
                    frame.len()
                );
            }
            let surface = unsafe {
                ImageSurface::create_for_data_unsafe(
                    frame.as_mut_ptr(),
                    Format::ARgb32,
                    width_i32,
                    height_i32,
                    stride,
                )?
            };
            let context = CairoContext::new(&surface)?;
            context.scale(self.scale_factor, self.scale_factor);
            self.bar.render(
                &context,
                Size::new(
                    self.logical_size.width as f32,
                    self.logical_size.height as f32,
                ),
            )?;
            let _ = self.bar.runtime_mut().take_changes();
            surface.flush();
        }
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
        let RuntimeUpdate {
            changes,
            platform_effects,
            issues,
        } = update;
        for issue in issues {
            warn!("xbar runtime issue: {issue:?}");
        }
        for effect in platform_effects {
            self.handle_platform_effect(effect);
        }
        if !changes.is_empty() {
            self.request_redraw();
        }
    }

    fn tick_and_poll(&mut self) {
        if !self.shared_path.is_empty()
            && self.bar.runtime().transport().is_none()
            && self.last_transport_attempt.elapsed() >= TRANSPORT_RETRY_INTERVAL
        {
            self.last_transport_attempt = Instant::now();
            match SharedTransport::open(&self.shared_path) {
                Ok(transport) => {
                    self.bar.runtime_mut().set_transport(Some(transport));
                    log::debug!("reconnected WM transport at {}", self.shared_path);
                }
                Err(error) => log::debug!("WM transport is still unavailable: {error}"),
            }
        }

        let mut update = self.bar.tick();
        update.merge(self.bar.poll_transport());
        self.handle_runtime_update(update);
    }

    fn handle_platform_effect(&mut self, effect: BarEffect) {
        match effect {
            BarEffect::ApplyMonitorGeometry(geometry) => self.apply_monitor_geometry(geometry),
            BarEffect::ClearMonitorGeometry => {
                if let Some(window) = &self.window {
                    window.set_outer_position(LogicalPosition::new(0.0, 0.0));
                    window.set_inner_size(self.default_logical_size);
                }
            }
            BarEffect::Screenshot => spawn_program("flameshot", &["gui"]),
            BarEffect::OpenAudioControl => spawn_program("pavucontrol", &[]),
            BarEffect::WindowManager(_)
            | BarEffect::ToggleMute
            | BarEffect::AdjustVolume(_)
            | BarEffect::AdjustBrightness(_)
            | BarEffect::RefreshBattery => {
                warn!("no frontend adapter handled platform effect: {effect:?}");
            }
        }
    }

    fn apply_monitor_geometry(&self, geometry: xbar_core::MonitorGeometry) {
        if let Some(window) = &self.window {
            let height = (f64::from(self.bar.config().bar_height) * self.scale_factor)
                .round()
                .clamp(1.0, f64::from(u32::MAX)) as u32;
            window.set_outer_position(PhysicalPosition::new(geometry.x, geometry.y));
            window.set_inner_size(PhysicalSize::new(geometry.width, height));
        }
    }

    fn on_user_event(&mut self, event: UserEvent) {
        match event {
            UserEvent::Tick => self.tick_and_poll(),
            UserEvent::SharedUpdated(pending) => {
                let update = self.bar.poll_transport();
                self.handle_runtime_update(update);
                pending.store(false, Ordering::Release);
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
                    let action = if vertical > 0.0 {
                        Some(PointerAction::ScrollUp)
                    } else if vertical < 0.0 {
                        Some(PointerAction::ScrollDown)
                    } else {
                        None
                    };
                    if let Some(action) = action {
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

fn spawn_program(program: &str, args: &[&str]) {
    let program = program.to_owned();
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    thread::spawn(move || match Command::new(&program).args(&args).status() {
        Ok(status) if status.success() => {}
        Ok(status) => warn!("{program} exited with {status}"),
        Err(error) => warn!("failed to run {program}: {error}"),
    });
}

fn main() -> Result<()> {
    let shared_path = env::args().skip(1).last().unwrap_or_default();
    initialize_logging("tao_pixels_bar", &shared_path)?;

    let transport = if shared_path.is_empty() {
        None
    } else {
        Some(
            SharedTransport::open(&shared_path)
                .with_context(|| format!("failed to open shared transport {shared_path}"))?,
        )
    };
    let notifier = transport
        .as_ref()
        .map(|transport| transport.notifier(true))
        .transpose()
        .context("failed to start shared transport notifier")?;
    let runtime = BarRuntime::with_transport(ModelConfig::default(), transport)?;
    let presentation = PresentationConfig {
        bar_height: 38.0,
        ..PresentationConfig::default()
    };
    let font = env::var("XBAR_FONT").unwrap_or_else(|_| "monospace 11".to_owned());
    let bar = CairoBar::new(runtime, presentation, FontDescription::from_string(&font));

    let mut event_loop: EventLoop<UserEvent> = EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let _tick_forwarder = spawn_tick_thread(proxy.clone());
    let _shared_forwarder = spawn_shared_thread(proxy, notifier);

    let mut app = App::new(bar, LogicalSize::new(800.0, 38.0), 1.0, shared_path);
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
