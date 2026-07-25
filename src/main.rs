use anyhow::{Context as _, Result};
use log::warn;
use pango::FontDescription;
use std::env;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::Duration;
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::{
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy},
    monitor::MonitorHandle,
    window::{Window, WindowBuilder},
};

use xbar_core::{
    AlignedWakeThread, BarRuntime, RuntimeUpdate, TransportRecoveryConfig, TransportWakeSlot,
    WakeAck,
    logging::init as initialize_logging,
    presentation::{Point, PointerAction},
    render::cairo::{CairoBar, CpuCanvas},
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
}

impl App {
    fn new(
        window: Rc<Window>,
        bar: CairoBar,
        logical_size: LogicalSize<f64>,
        scale_factor: f64,
        proxy: EventLoopProxy<UserEvent>,
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
        })
    }

    fn redraw(&mut self) -> Result<()> {
        let PhysicalSize { width, height } = self.last_physical_size;
        if width == 0 || height == 0 {
            return Ok(());
        }

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
        match frame.damage {
            Some(rect) if !rect.is_empty() => {
                let damage = softbuffer::Rect {
                    x: rect.x,
                    y: rect.y,
                    width: NonZeroU32::new(rect.width).expect("checked non-empty"),
                    height: NonZeroU32::new(rect.height).expect("checked non-empty"),
                };
                buffer.present_with_damage(&[damage])
            }
            // First frame, a resize, or an unchanged scene after an expose:
            // present everything.
            _ => buffer.present(),
        }
        .map_err(|error| anyhow::anyhow!("failed to present softbuffer frame: {error}"))?;
        Ok(())
    }

    fn request_redraw(&self) {
        self.window.request_redraw();
    }

    fn resize(&mut self, physical_size: PhysicalSize<u32>) {
        self.last_physical_size = physical_size;
        self.logical_size = physical_size.to_logical(self.scale_factor);
        if let Err(error) = resize_soft_surface(&mut self.soft_surface, physical_size) {
            warn!("failed to resize softbuffer surface: {error:#}");
        }
        self.request_redraw();
    }

    fn update_hover(&mut self, point: Point) {
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
                        self.window
                            .set_outer_position(LogicalPosition::new(0.0, 0.0));
                        self.window.set_inner_size(self.default_logical_size);
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
        let height = (f64::from(self.bar.config().bar_height) * self.scale_factor)
            .round()
            .clamp(1.0, f64::from(u32::MAX)) as u32;
        self.window
            .set_outer_position(PhysicalPosition::new(geometry.x, geometry.y));
        self.window
            .set_inner_size(PhysicalSize::new(geometry.width, height));
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
    let presentation = app_config.presentation.clone();
    let font = FontDescription::from_string(&app_config.font);
    let mut bar = CairoBar::new(runtime, presentation, font);
    if let Some(opacity) = app_config.background_opacity {
        bar.renderer_mut().set_background_opacity(Some(opacity));
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
        .unwrap_or(PhysicalSize::new(1920, 1080));
    let logical_size = LogicalSize::new(screen_size.width as f64 / scale_factor, 38.0);

    let window = Rc::new(
        WindowBuilder::new()
            .with_title("tao_softbuffer_bar")
            .with_inner_size(logical_size)
            .with_decorations(false)
            .with_resizable(true)
            .with_visible(true)
            .with_transparent(false)
            .build(&event_loop)
            .context("failed to build tao window")?,
    );
    let mut app = App::new(window, bar, logical_size, scale_factor, proxy)?;

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
            Event::UserEvent(UserEvent::SharedUpdated(_ack)) => {
                let update = app.bar.poll_transport();
                app.handle_runtime_update(update);
                app.sync_transport_wake();
            }
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
                    app.update_hover(point);
                }
                WindowEvent::CursorLeft { .. } => {
                    app.last_cursor_pos = None;
                    if app.bar.pointer_leave() {
                        app.request_redraw();
                    }
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    use tao::event::{ElementState, MouseButton};
                    if state == ElementState::Pressed
                        && let Some(point) = app.last_cursor_pos
                    {
                        let action = match button {
                            MouseButton::Left => Some(PointerAction::Primary),
                            MouseButton::Right => Some(PointerAction::Secondary),
                            MouseButton::Middle | MouseButton::Other(_) => None,
                            _ => None,
                        };
                        if let Some(action) = action {
                            app.handle_pointer_action(point, action);
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
                        let action = if y > 0.0 {
                            Some(PointerAction::ScrollUp)
                        } else if y < 0.0 {
                            Some(PointerAction::ScrollDown)
                        } else {
                            None
                        };
                        if let Some(action) = action {
                            app.handle_pointer_action(point, action);
                        }
                    }
                }
                _ => {}
            },
            Event::RedrawRequested(_) => {
                if let Err(error) = app.redraw() {
                    warn!("redraw failed: {error:#}");
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
