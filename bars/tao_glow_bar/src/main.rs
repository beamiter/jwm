mod edge_shell;
#[path = "../../native_x11.rs"]
mod native_x11;
mod redraw;
mod shell_actions;
mod shell_host;
mod shell_ipc;
mod shell_scene;
#[path = "../../tao_scroll.rs"]
mod tao_scroll;
mod texture_upload;

use anyhow::{Context as _, Result};
use log::{info, warn};
use pango::FontDescription;
use std::env;
use std::ffi::CString;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::{
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy},
    monitor::MonitorHandle,
    window::{Window, WindowBuilder},
};

use glow::HasContext;
use glutin::prelude::GlSurface;
use redraw::RedrawGate;
use texture_upload::TextureUpload;
use x11rb::rust_connection::RustConnection;
use xbar_core::glass::{
    GlassBackdrop, GlassParams, MAX_GLASS_IMAGE_BYTES, WallpaperSource, fallback_rgb,
};
use xbar_core::{
    AlignedWakeThread, BarEdge, BarPlacement, BarRuntime, MonitorGeometry, RuntimeUpdate,
    TransportRecoveryConfig, TransportWakeSlot, WakeAck,
    logging::init as initialize_logging,
    presentation::{Point, logical_bar_height},
    render::cairo::{CairoBar, CpuCanvas, PointerButton, PointerInput},
};
use xbar_linux_actions::{EffectRouter, GeometryRequest};

const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

fn bgra_frame_len(width: u32, height: u32) -> Result<usize> {
    // The GL presenter clamps a zero axis to one while resizing its surface,
    // so validate that effective allocation as well as non-empty CPU frames.
    let surface_len = (width.max(1) as usize)
        .checked_mul(height.max(1) as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("BGRA surface dimensions overflow"))?;
    if surface_len > MAX_GLASS_IMAGE_BYTES {
        anyhow::bail!(
            "BGRA surface needs {surface_len} bytes; frame budget is {MAX_GLASS_IMAGE_BYTES} bytes"
        );
    }
    Ok(if width == 0 || height == 0 {
        0
    } else {
        surface_len
    })
}

#[derive(Debug)]
enum UserEvent {
    #[cfg(target_os = "linux")]
    GtkWheel(PointerInput),
    Tick,
    SharedUpdated(WakeAck),
    EdgeAction(edge_shell::Action),
}

struct GlPresenter {
    gl: glow::Context,
    surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
    context: glutin::context::PossiblyCurrentContext,
    /// Kept alive to back the GL context and surface.
    #[allow(dead_code)]
    display: glutin::display::Display,
    program: glow::NativeProgram,
    vao: glow::NativeVertexArray,
    texture: glow::NativeTexture,
    canvas: CpuCanvas,
    upload: TextureUpload,
    alpha_supported: bool,
}

impl GlPresenter {
    fn new(window: &Window, physical_size: PhysicalSize<u32>) -> Result<Self> {
        use glutin::config::GlConfig as _;
        let (display, config, surface, context, gl) = create_gl_surface(window)?;
        let alpha_supported = config.alpha_size() > 0;
        let program = build_quad_program(&gl)?;
        let vao = build_quad_mesh(&gl);
        let texture = create_texture(&gl);

        // The default framebuffer may be sRGB; the Cairo output is already
        // in sRGB byte values, so a plain copy is correct.
        unsafe {
            gl.viewport(
                0,
                0,
                physical_size.width as i32,
                physical_size.height as i32,
            );
            gl.enable(glow::BLEND);
            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
        }

        Ok(Self {
            gl,
            surface,
            context,
            display,
            program,
            vao,
            texture,
            canvas: CpuCanvas::new(),
            upload: TextureUpload::default(),
            alpha_supported,
        })
    }

    fn redraw(
        &mut self,
        bar: &mut CairoBar,
        width: u32,
        height: u32,
        scale: f64,
        backdrop: Option<&cairo::ImageSurface>,
    ) -> Result<()> {
        let frame = self
            .canvas
            .render_over(bar, width, height, scale, backdrop)?;
        let _ = bar.runtime_mut().take_changes();
        self.upload.upload(&self.gl, self.texture, &frame)?;
        draw_fullscreen_quad(&self.gl, self.vao, self.program, self.texture);
        self.surface
            .swap_buffers(&self.context)
            .map_err(|error| anyhow::anyhow!("swap buffers failed: {error}"))
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        let width = NonZeroU32::new(size.width.max(1)).unwrap();
        let height = NonZeroU32::new(size.height.max(1)).unwrap();
        self.surface.resize(&self.context, width, height);
        unsafe {
            self.gl
                .viewport(0, 0, width.get() as i32, height.get() as i32);
        }
    }
}

struct SoftwarePresenter {
    canvas: CpuCanvas,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
}

impl SoftwarePresenter {
    fn new(window: &Rc<Window>, size: PhysicalSize<u32>) -> Result<Self> {
        let context = softbuffer::Context::new(Rc::clone(window))
            .map_err(|error| anyhow::anyhow!("failed to create softbuffer context: {error}"))?;
        let mut surface = softbuffer::Surface::new(&context, Rc::clone(window))
            .map_err(|error| anyhow::anyhow!("failed to create softbuffer surface: {error}"))?;
        resize_soft_surface(&mut surface, size)?;
        Ok(Self {
            canvas: CpuCanvas::new(),
            surface,
        })
    }

    fn redraw(
        &mut self,
        bar: &mut CairoBar,
        width: u32,
        height: u32,
        scale: f64,
        backdrop: Option<&cairo::ImageSurface>,
    ) -> Result<()> {
        let frame = self
            .canvas
            .render_over(bar, width, height, scale, backdrop)?;
        let width = width as usize;
        let height = height as usize;
        let mut buffer = self
            .surface
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
        buffer
            .present()
            .map_err(|error| anyhow::anyhow!("failed to present softbuffer frame: {error}"))?;
        drop(
            self.surface
                .buffer_mut()
                .map_err(|error| anyhow::anyhow!("failed to complete softbuffer frame: {error}"))?,
        );
        Ok(())
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        if let Err(error) = resize_soft_surface(&mut self.surface, size) {
            warn!("failed to resize software surface: {error:#}");
        }
    }
}

enum Presenter {
    OpenGl(Box<GlPresenter>),
    Software(SoftwarePresenter),
}

impl Presenter {
    fn redraw(
        &mut self,
        bar: &mut CairoBar,
        width: u32,
        height: u32,
        scale: f64,
        backdrop: Option<&cairo::ImageSurface>,
    ) -> Result<()> {
        // Validate before either the explicit GL buffer or CpuCanvas can
        // allocate from compositor-controlled dimensions.
        let _ = bgra_frame_len(width, height)?;
        match self {
            Self::OpenGl(presenter) => presenter.redraw(bar, width, height, scale, backdrop),
            Self::Software(presenter) => presenter.redraw(bar, width, height, scale, backdrop),
        }
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        match self {
            Self::OpenGl(presenter) => presenter.resize(size),
            Self::Software(presenter) => presenter.resize(size),
        }
    }
}

struct App {
    edge_shell: Option<edge_shell::EdgeShell>,
    window: Rc<Window>,
    presenter: Presenter,
    bar: CairoBar,
    scale_factor: f64,
    logical_size: LogicalSize<f64>,
    default_monitor: MonitorGeometry,
    dock_hints: native_x11::DockHints,
    last_physical_size: PhysicalSize<u32>,
    last_cursor_pos: Option<Point>,
    redraw_gate: RedrawGate,
    proxy: EventLoopProxy<UserEvent>,
    transport_wake: TransportWakeSlot,
    effects: EffectRouter,
    /// Side connection for the window-depth check that completes the
    /// translucency decision.
    x11: Option<Rc<RustConnection>>,
    /// True while a compositor was seen at startup but the window's depth has
    /// not been read yet — GTK realizes the native window lazily, so the
    /// check may have to wait for the first frame.
    depth_check_pending: bool,
    /// Opaque windows (no compositor, or a 24-bit window) frost the root
    /// wallpaper instead of showing a solid slab.
    opaque: bool,
    glass: Option<GlassBackdrop<OpaqueWallpaper>>,
    frosted_params: GlassParams,
    last_wallpaper_revision: Option<u64>,
    last_wallpaper_poll: Option<Instant>,
    started_at: Instant,
    saw_root_pixmap: bool,
}

impl App {
    fn new(
        window: Rc<Window>,
        bar: CairoBar,
        logical_size: LogicalSize<f64>,
        scale_factor: f64,
        proxy: EventLoopProxy<UserEvent>,
        native: (Option<Rc<RustConnection>>, bool),
        mut glass: Option<GlassBackdrop<OpaqueWallpaper>>,
    ) -> Result<Self> {
        let (x11, compositor_active) = native;
        let frosted_params = glass
            .as_mut()
            .map(|glass| *glass.cache_mut().params())
            .unwrap_or_default();
        let physical_size = window.inner_size();
        let _ = bgra_frame_len(physical_size.width, physical_size.height)
            .context("initial window surface exceeds the frame budget")?;
        let presenter = match GlPresenter::new(&window, physical_size) {
            Ok(presenter) => {
                info!("OpenGL presenter initialized");
                Presenter::OpenGl(Box::new(presenter))
            }
            Err(error) => {
                warn!("OpenGL presenter unavailable ({error:#}); using software fallback");
                Presenter::Software(SoftwarePresenter::new(&window, physical_size)?)
            }
        };

        Ok(Self {
            edge_shell: None,
            window,
            presenter,
            bar,
            scale_factor,
            logical_size,
            default_monitor: MonitorGeometry {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            dock_hints: native_x11::DockHints::default(),
            last_physical_size: physical_size,
            last_cursor_pos: None,
            redraw_gate: RedrawGate::default(),
            proxy,
            transport_wake: TransportWakeSlot::new(true),
            effects: EffectRouter::default(),
            x11,
            depth_check_pending: true,
            opaque: !compositor_active,
            last_wallpaper_revision: None,
            last_wallpaper_poll: None,
            started_at: Instant::now(),
            saw_root_pixmap: !compositor_active && glass.is_none(),
            glass,
            frosted_params,
        })
    }

    /// Complete the translucency decision once the native window exists.
    fn resolve_translucency(&mut self) {
        if !self.depth_check_pending {
            return;
        }
        use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
        let Ok(handle) = self.window.window_handle() else {
            return;
        };
        self.depth_check_pending = false;
        let native_wayland = matches!(handle.as_raw(), RawWindowHandle::Wayland(_));
        let native_alpha = matches!(&self.presenter, Presenter::OpenGl(gl) if gl.alpha_supported);
        let xid = raw_x11_window_id(handle.as_raw());
        let argb = (native_wayland && native_alpha)
            || (!self.opaque
                && xid
                    .zip(self.x11.as_ref())
                    .is_some_and(|(xid, conn)| window_is_argb(conn, xid)));
        self.opaque = !argb;
    }

    fn redraw(&mut self) -> Result<()> {
        if !self.redraw_gate.begin_frame() {
            return Ok(());
        }
        let PhysicalSize { width, height } = self.last_physical_size;
        if width == 0 || height == 0 {
            return Ok(());
        }

        self.resolve_translucency();
        let frosted = self.bar.runtime().view().blur_status_bar.unwrap_or(false);
        if let Some(glass) = self.glass.as_mut() {
            glass.cache_mut().set_params(if frosted {
                self.frosted_params
            } else {
                GlassParams::clear()
            });
        }
        let scale = self.scale_factor;
        let opaque = self.opaque;
        let position = self.window.outer_position().ok();
        let Self {
            presenter,
            bar,
            glass,
            ..
        } = self;
        let backdrop = if opaque {
            position.and_then(|pos| {
                glass
                    .as_mut()
                    .and_then(|glass| glass.ensure(pos.x, pos.y, width, height))
            })
        } else {
            None
        };

        bar.set_background_transparency_supported(!opaque || backdrop.is_some());
        presenter.redraw(bar, width, height, scale, backdrop)?;
        let update = self.bar.take_pending_runtime();
        if !update.is_empty() {
            self.handle_runtime_update(update);
        }
        Ok(())
    }

    fn request_redraw(&mut self) {
        if self.redraw_gate.request() {
            self.window.request_redraw();
        }
    }

    fn resize(&mut self, physical_size: PhysicalSize<u32>) {
        if let Err(error) = bgra_frame_len(physical_size.width, physical_size.height) {
            warn!("ignoring oversized bar surface resize: {error:#}");
            // Stop redraws against the old presenter extent until the
            // compositor supplies a valid size again.
            self.last_physical_size = PhysicalSize::new(0, 0);
            self.redraw_gate.set_drawable(false);
            return;
        }
        self.last_physical_size = physical_size;
        self.redraw_gate
            .set_drawable(physical_size.width > 0 && physical_size.height > 0);
        if let Some(height) = logical_bar_height(
            self.bar
                .config()
                .edge
                .physical_thickness(physical_size.width, physical_size.height),
            self.scale_factor,
        ) {
            self.bar.config_mut().bar_height = height;
        }
        self.logical_size = physical_size.to_logical(self.scale_factor);
        self.presenter.resize(physical_size);
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
                    GeometryRequest::Clear => self.place_on_output(),
                }
                Ok(())
            })
            .expect("geometry closure is infallible");
        self.effects = effects;
        self.sync_edge_shell();
        needs_redraw
    }

    fn sync_edge_shell(&mut self) {
        let geometry = self.bar.runtime().view().geometry;
        // A managed bar's geometry is a strip, not a full output. Match that
        // strip to the native monitor instead of treating its width as a
        // desktop height (especially on secondary/scaled outputs).
        let output = geometry
            .and_then(|geometry| {
                self.window.available_monitors().find_map(|monitor| {
                    let origin = monitor.position();
                    let size = monitor.size();
                    (geometry.x >= origin.x
                        && geometry.y >= origin.y
                        && i64::from(geometry.x) < i64::from(origin.x) + i64::from(size.width)
                        && i64::from(geometry.y) < i64::from(origin.y) + i64::from(size.height))
                    .then_some(MonitorGeometry {
                        x: origin.x,
                        y: origin.y,
                        width: size.width,
                        height: size.height,
                    })
                })
            })
            .unwrap_or(self.default_monitor);
        if let Some(shell) = self.edge_shell.as_mut() {
            shell.update(output, self.bar.runtime().snapshot());
        }
    }

    fn handle_edge_action(&mut self, action: edge_shell::Action) {
        use edge_shell::Action;
        let command = match action {
            Action::Bound { action, .. } => {
                self.handle_edge_action(*action);
                return;
            }
            Action::Model(action) => {
                let update = self.bar.runtime_mut().dispatch(action);
                self.handle_runtime_update(update);
                return;
            }
            Action::Level { audio, percent } => {
                let view = self.bar.runtime().view();
                let current = if audio {
                    view.audio.volume_percent
                } else {
                    view.brightness.percent
                };
                if let Some(current) = current {
                    let delta = percent.clamp(0, 100) - i32::from(current.rounded());
                    let action = if audio {
                        xbar_core::model::UserAction::AdjustVolume(delta)
                    } else {
                        xbar_core::model::UserAction::AdjustBrightness(delta)
                    };
                    let update = self.bar.runtime_mut().dispatch(action);
                    self.handle_runtime_update(update);
                }
                return;
            }
            Action::FocusWindow { id, .. } => {
                std::thread::spawn(move || {
                    if let Err(error) =
                        shell_ipc::request("focus_window", serde_json::json!({"id": id}))
                    {
                        warn!("cannot focus taskbar window: {error:#}");
                    }
                });
                return;
            }
            Action::MediaPrevious | Action::MediaNext => {
                let command = if action == Action::MediaPrevious {
                    "media_previous"
                } else {
                    "media_next"
                };
                std::thread::spawn(move || {
                    if let Err(error) = shell_ipc::request(command, serde_json::Value::Null) {
                        warn!("cannot change media track: {error:#}");
                    }
                });
                return;
            }
            Action::CycleTasks => return,
            Action::Files => std::env::var_os("HOME").map(|home| ("xdg-open", vec![home])),
            Action::Terminal => Some(("x-terminal-emulator", Vec::new())),
            Action::Browser => Some(("x-www-browser", Vec::new())),
        };
        if let Some((program, args)) = command {
            match std::process::Command::new(program).args(args).spawn() {
                Ok(mut child) => {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
                Err(error) => {
                    warn!("cannot launch {program}: {error}");
                    let update = self.bar.runtime_mut().dispatch(
                        xbar_core::model::UserAction::OpenShellHub(
                            xbar_core::model::ShellRoute::Applications,
                        ),
                    );
                    self.handle_runtime_update(update);
                }
            }
        }
    }

    fn handle_shared_update(&mut self, ack: WakeAck) {
        let update = self.bar.poll_transport();
        ack.ack();
        self.apply_transport_update(update);
    }

    fn apply_transport_update(&mut self, update: RuntimeUpdate) {
        let needs_redraw = self.apply_runtime_update(update);
        self.sync_transport_wake();
        if needs_redraw {
            // The native redraw turn paints the latest complete model. A
            // burst of WM snapshots must not synchronously rasterize every
            // intermediate state before mouse/resize events are serviced.
            self.request_redraw();
        }
    }

    fn tick_and_poll(&mut self) {
        let mut update = self.bar.tick();
        update.merge(self.bar.poll_transport());
        self.handle_runtime_update(update);
        self.sync_transport_wake();
        if self.opaque_wallpaper_changed() {
            self.request_redraw();
        }
    }

    fn opaque_wallpaper_changed(&mut self) -> bool {
        if !self.opaque {
            return false;
        }
        let now = Instant::now();
        let interval = if self.awaiting_root_wallpaper(now) {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(1)
        };
        if self
            .last_wallpaper_poll
            .is_some_and(|last| now.saturating_duration_since(last) < interval)
        {
            return false;
        }
        self.last_wallpaper_poll = Some(now);
        let Some(glass) = self.glass.as_mut() else {
            return false;
        };
        let revision = glass.source_mut().revision();
        if glass.source_mut().has_root_pixmap() {
            self.saw_root_pixmap = true;
        }
        if self.last_wallpaper_revision == Some(revision) {
            return false;
        }
        self.last_wallpaper_revision = Some(revision);
        true
    }

    fn wallpaper_poll_deadline(&self, now: Instant) -> Option<Instant> {
        self.awaiting_root_wallpaper(now).then(|| {
            self.last_wallpaper_poll
                .map_or(now, |last| last + Duration::from_millis(200))
        })
    }

    fn awaiting_root_wallpaper(&self, now: Instant) -> bool {
        self.opaque
            && !self.saw_root_pixmap
            && now.saturating_duration_since(self.started_at) < Duration::from_secs(30)
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
        if let Some(monitor) = self.window.current_monitor() {
            let origin = monitor.position();
            let size = monitor.size();
            self.default_monitor = MonitorGeometry {
                x: origin.x,
                y: origin.y,
                width: size.width,
                height: size.height,
            };
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
        let window = &self.window;
        window.set_outer_position(PhysicalPosition::new(placement.x, placement.y));
        window.set_inner_size(PhysicalSize::new(placement.width, placement.height));
        if let Err(error) = self.dock_hints.sync(
            window.as_ref(),
            placement,
            self.bar.config().edge,
            "tao_glow_bar",
        ) {
            warn!("could not update dock hints: {error:#}");
        }
        if let Some(glass) = self.glass.as_mut() {
            glass.cache_mut().invalidate();
        }
        self.last_wallpaper_revision = None;
        self.last_wallpaper_poll = None;
        self.request_redraw();
    }

    fn sync_dock_hints(&mut self) {
        let window = &self.window;
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
            "tao_glow_bar",
        ) {
            warn!("could not update dock hints: {error:#}");
        }
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

fn create_gl_surface(
    window: &Window,
) -> Result<(
    glutin::display::Display,
    glutin::config::Config,
    glutin::surface::Surface<glutin::surface::WindowSurface>,
    glutin::context::PossiblyCurrentContext,
    glow::Context,
)> {
    use glutin::config::ConfigTemplateBuilder;
    use glutin::context::{ContextApi, ContextAttributesBuilder, NotCurrentGlContext};
    use glutin::display::{Display, DisplayApiPreference, GlDisplay};
    use glutin::surface::{SurfaceAttributesBuilder, WindowSurface};
    use raw_window_handle::{HasDisplayHandle as _, HasWindowHandle as _};

    let display_handle = window
        .display_handle()
        .map_err(|error| anyhow::anyhow!("failed to get display handle: {error}"))?;
    let window_handle = window
        .window_handle()
        .map_err(|error| anyhow::anyhow!("failed to get window handle: {error}"))?;

    // EGL reports failures directly and is safe to abandon for the software
    // presenter. GLX requires an Xlib error-hook registrar that Tao does not
    // expose, so using it as an implicit fallback can leave a false-current
    // context instead of a recoverable error.
    let display = unsafe { Display::new(display_handle.into(), DisplayApiPreference::Egl) }
        .map_err(|error| anyhow::anyhow!("failed to create GL display: {error}"))?;

    let raw_window_handle = window_handle.into();
    let template = ConfigTemplateBuilder::new()
        .with_alpha_size(8)
        .compatible_with_native_window(raw_window_handle)
        .build();
    let config = match unsafe { display.find_configs(template) } {
        Ok(mut configs) => configs.next(),
        Err(error) => {
            warn!("alpha GL config lookup failed: {error}; trying without alpha");
            None
        }
    };
    let config = match config {
        Some(config) => config,
        None => {
            let template = ConfigTemplateBuilder::new()
                .with_alpha_size(0)
                .compatible_with_native_window(raw_window_handle)
                .build();
            unsafe { display.find_configs(template) }
                .map_err(|error| anyhow::anyhow!("failed to find GL config: {error}"))?
                .next()
                .ok_or_else(|| anyhow::anyhow!("no compatible GL config found"))?
        }
    };

    let context_attributes = ContextAttributesBuilder::new()
        .with_context_api(ContextApi::OpenGl(None))
        .build(Some(window_handle.into()));
    let not_current = unsafe { display.create_context(&config, &context_attributes) }
        .map_err(|error| anyhow::anyhow!("failed to create GL context: {error}"))?;

    let size = window.inner_size();
    let _ = bgra_frame_len(size.width, size.height)
        .context("GL window surface exceeds the frame budget")?;
    let surface_attributes = SurfaceAttributesBuilder::<WindowSurface>::new()
        .with_srgb(Some(true))
        .build(
            window_handle.into(),
            NonZeroU32::new(size.width.max(1)).unwrap(),
            NonZeroU32::new(size.height.max(1)).unwrap(),
        );
    let surface = unsafe { display.create_window_surface(&config, &surface_attributes) }
        .map_err(|error| anyhow::anyhow!("failed to create GL surface: {error}"))?;

    let context = not_current
        .make_current(&surface)
        .map_err(|error| anyhow::anyhow!("failed to make GL context current: {error}"))?;

    let gl = unsafe {
        glow::Context::from_loader_function(|symbol| {
            let symbol = CString::new(symbol).unwrap_or_default();
            display.get_proc_address(&symbol) as *const _
        })
    };

    Ok((display, config, surface, context, gl))
}

fn build_quad_program(gl: &glow::Context) -> Result<glow::NativeProgram> {
    unsafe {
        let program = gl
            .create_program()
            .map_err(|error| anyhow::anyhow!("create shader program: {error}"))?;

        let vertex = compile_shader(
            gl,
            glow::VERTEX_SHADER,
            r#"#version 330 core
            layout(location = 0) in vec2 a_position;
            layout(location = 1) in vec2 a_texcoord;
            out vec2 v_texcoord;
            void main() {
                gl_Position = vec4(a_position, 0.0, 1.0);
                v_texcoord = a_texcoord;
            }"#,
        )
        .context("compile vertex shader")?;

        let fragment = compile_shader(
            gl,
            glow::FRAGMENT_SHADER,
            r#"#version 330 core
            in vec2 v_texcoord;
            out vec4 f_color;
            uniform sampler2D u_texture;
            void main() {
                f_color = texture(u_texture, v_texcoord);
            }"#,
        )
        .context("compile fragment shader")?;

        gl.attach_shader(program, vertex);
        gl.attach_shader(program, fragment);
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            anyhow::bail!("shader link failed: {log}");
        }
        gl.detach_shader(program, vertex);
        gl.detach_shader(program, fragment);
        gl.delete_shader(vertex);
        gl.delete_shader(fragment);

        Ok(program)
    }
}

fn compile_shader(gl: &glow::Context, kind: u32, source: &str) -> Result<glow::NativeShader> {
    unsafe {
        let shader = gl
            .create_shader(kind)
            .map_err(|error| anyhow::anyhow!("create shader: {error}"))?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
        if !gl.get_shader_compile_status(shader) {
            let log = gl.get_shader_info_log(shader);
            anyhow::bail!("shader compile failed: {log}");
        }
        Ok(shader)
    }
}

fn build_quad_mesh(gl: &glow::Context) -> glow::NativeVertexArray {
    #[rustfmt::skip]
    let vertices: [f32; 16] = [
        // position     // texcoord
        -1.0, -1.0,    0.0, 1.0,
         1.0, -1.0,    1.0, 1.0,
         1.0,  1.0,    1.0, 0.0,
        -1.0,  1.0,    0.0, 0.0,
    ];
    let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];

    unsafe {
        let vao = gl.create_vertex_array().expect("create VAO");
        let vbo = gl.create_buffer().expect("create VBO");
        let ebo = gl.create_buffer().expect("create EBO");

        gl.bind_vertex_array(Some(vao));

        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        gl.buffer_data_u8_slice(
            glow::ARRAY_BUFFER,
            bytemuck::cast_slice(&vertices),
            glow::STATIC_DRAW,
        );

        gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(ebo));
        gl.buffer_data_u8_slice(
            glow::ELEMENT_ARRAY_BUFFER,
            bytemuck::cast_slice(&indices),
            glow::STATIC_DRAW,
        );

        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(
            0,
            2,
            glow::FLOAT,
            false,
            4 * std::mem::size_of::<f32>() as i32,
            0,
        );
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_f32(
            1,
            2,
            glow::FLOAT,
            false,
            4 * std::mem::size_of::<f32>() as i32,
            2 * std::mem::size_of::<f32>() as i32,
        );

        gl.bind_vertex_array(None);
        gl.delete_buffer(vbo);
        gl.delete_buffer(ebo);

        vao
    }
}

fn create_texture(gl: &glow::Context) -> glow::NativeTexture {
    unsafe {
        let texture = gl.create_texture().expect("create texture");
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
        texture
    }
}

fn draw_fullscreen_quad(
    gl: &glow::Context,
    vao: glow::NativeVertexArray,
    program: glow::NativeProgram,
    texture: glow::NativeTexture,
) {
    unsafe {
        gl.clear_color(0.0, 0.0, 0.0, 0.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.use_program(Some(program));
        gl.bind_vertex_array(Some(vao));
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.draw_elements(glow::TRIANGLES, 6, glow::UNSIGNED_SHORT, 0);
        gl.bind_texture(glow::TEXTURE_2D, None);
        gl.bind_vertex_array(None);
    }
}

/// A monitor size is only usable when the platform really knows one.
fn usable_screen_size(size: PhysicalSize<u32>) -> Option<PhysicalSize<u32>> {
    (size.width > 1 && size.height > 1).then_some(size)
}

fn initial_logical_size(
    screen_size: PhysicalSize<u32>,
    scale_factor: f64,
    logical_thickness: f64,
    edge: BarEdge,
) -> LogicalSize<f64> {
    let placement = BarPlacement::at_edge(
        MonitorGeometry {
            x: 0,
            y: 0,
            width: screen_size.width,
            height: screen_size.height,
        },
        logical_thickness,
        scale_factor,
        edge,
    )
    .expect("validated bar thickness and output scale");
    PhysicalSize::new(placement.width, placement.height).to_logical(scale_factor)
}

use jwm_shell::wallpaper::{OpaqueWallpaper, RootPixmapSource, compositor_active};

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
    initialize_logging("tao_glow_bar", &shared_path)?;

    let app_config = xbar_core::config::BarConfig::load_default()?;
    let shell_owner = std::env::var("JWM_DESKTOP_SHELL").ok();
    let legacy_requested =
        std::env::var("XBAR_LEGACY_PRIVATE_SHELL").is_ok_and(|value| value == "1");
    // Ownership gates companion creation, while the original top-bar skin,
    // font and presentation configuration continue to apply unchanged.
    let suppress_private_shell =
        shell_owner.as_deref() == Some("1") || (shell_owner.is_some() && !legacy_requested);
    if shell_host::requested() {
        return shell_host::run(app_config, shared_path);
    }
    let runtime = if shared_path.is_empty() {
        BarRuntime::new(app_config.model_config())?
    } else {
        let recovery = TransportRecoveryConfig::new(shared_path.clone(), TRANSPORT_RETRY_INTERVAL)?;
        BarRuntime::with_managed_transport(app_config.model_config(), recovery)?
    };
    let mut presentation = app_config.presentation.clone();
    let desktop_header = presentation.apply_desktop_header_profile();
    let families = xbar_core::icon_font::installed_families();
    let nerd = xbar_core::icon_font::select_installed_nerd_font_family(
        families.iter().map(String::as_str),
        presentation.icon_font.as_deref(),
    );
    if nerd.is_some() {
        presentation.apply_nerd_font_icons_if_stock();
    }
    let font = FontDescription::from_string(&app_config.font);
    let mut bar = CairoBar::new(runtime, presentation, font);
    bar.renderer_mut().set_capsule(desktop_header);

    let (x11, compositor_active, screen_num) = match x11rb::connect(None) {
        Ok((conn, screen_num)) => {
            let active = compositor_active(&conn, screen_num);
            (Some(Rc::new(conn)), active, screen_num)
        }
        Err(_) => (None, false, 0),
    };
    bar.follow_wm_background(
        app_config
            .background_opacity
            .or(desktop_header.then_some(0.58)),
        compositor_active,
    );

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
    let origin = primary_monitor
        .as_ref()
        .map(|monitor| monitor.position())
        .unwrap_or(PhysicalPosition::new(0, 0));
    let initial_monitor = MonitorGeometry {
        x: origin.x,
        y: origin.y,
        width: screen_size.width,
        height: screen_size.height,
    };
    let initial_placement = BarPlacement::at_edge(
        initial_monitor,
        bar.config().logical_thickness(scale_factor),
        scale_factor,
        bar.config().edge,
    )?;
    let logical_size = initial_logical_size(
        screen_size,
        scale_factor,
        bar.config().logical_thickness(scale_factor),
        bar.config().edge,
    );

    let window = Rc::new(
        WindowBuilder::new()
            .with_title("tao_glow_bar")
            .with_inner_size(logical_size)
            .with_position(PhysicalPosition::new(
                initial_placement.x,
                initial_placement.y,
            ))
            .with_decorations(false)
            .with_resizable(true)
            .with_visible(true)
            .with_transparent(true)
            .build(&event_loop)
            .context("failed to build tao window")?,
    );

    #[cfg(target_os = "linux")]
    tao_scroll::install_gtk_bridge(
        window.as_ref(),
        proxy.clone(),
        UserEvent::GtkWheel,
        bar.config().scroll,
    );

    let tint = fallback_rgb(app_config.theme);
    let glass = {
        let params = app_config.glass.params();
        if let Some(file) =
            app_config
                .glass
                .file_source(screen_size.width, screen_size.height, tint)
        {
            Some(match x11.as_ref() {
                Some(conn) => GlassBackdrop::new(
                    OpaqueWallpaper::FileThenRoot {
                        file,
                        root: RootPixmapSource::new(Rc::clone(conn), screen_num),
                    },
                    params,
                )
                .with_fallback(tint),
                None => GlassBackdrop::new(OpaqueWallpaper::File(file), params).with_fallback(tint),
            })
        } else {
            x11.as_ref().map(|conn| {
                GlassBackdrop::new(
                    OpaqueWallpaper::Root(RootPixmapSource::new(Rc::clone(conn), screen_num)),
                    params,
                )
                .with_fallback(tint)
            })
        }
    };

    info!("creating OpenGL surface for tao window");
    let mut app = App::new(
        window,
        bar,
        logical_size,
        scale_factor,
        proxy,
        (x11, compositor_active),
        glass,
    )?;
    app.default_monitor = initial_monitor;
    app.apply_native_placement(initial_placement);

    let update = app.bar.tick();
    app.handle_runtime_update(update);
    let update = app.bar.poll_transport();
    app.handle_runtime_update(update);
    app.sync_transport_wake();
    let mut companion_config = app_config.clone();
    companion_config.edge_shell.enabled &= !suppress_private_shell;
    app.edge_shell = edge_shell::EdgeShell::start(
        companion_config,
        initial_monitor,
        app.bar.runtime().snapshot(),
        app.proxy.clone(),
    )?;
    app.sync_edge_shell();
    app.request_redraw();

    let exit_code = event_loop.run_return(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            #[cfg(target_os = "linux")]
            Event::UserEvent(UserEvent::GtkWheel(input)) => app.handle_pointer_input(input),
            Event::UserEvent(UserEvent::Tick) => {
                app.tick_and_poll();
            }
            Event::UserEvent(UserEvent::SharedUpdated(ack)) => app.handle_shared_update(ack),
            Event::UserEvent(UserEvent::EdgeAction(action)) => app.handle_edge_action(action),
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                    *control_flow = ControlFlow::Exit;
                }
                WindowEvent::Resized(size) => {
                    app.resize(size);
                    app.sync_dock_hints();
                }
                WindowEvent::Moved(_) => app.sync_dock_hints(),
                WindowEvent::ScaleFactorChanged {
                    scale_factor,
                    new_inner_size,
                } => {
                    app.scale_factor = scale_factor;
                    // Keep the configured thickness while the scale event still
                    // carries a size from the previous output. GTK commits this size.
                    if let Ok(placement) = app.desired_placement() {
                        *new_inner_size = PhysicalSize::new(placement.width, placement.height);
                    }
                    app.resize(*new_inner_size);
                    app.last_cursor_pos = None;
                    app.handle_pointer_input(PointerInput::Leave);
                    app.reapply_geometry();
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
                WindowEvent::Focused(false) => {
                    app.handle_pointer_input(PointerInput::Cancel);
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
                #[cfg(not(target_os = "linux"))]
                WindowEvent::MouseWheel { delta, phase, .. } => {
                    if let Some(point) = app.last_cursor_pos {
                        let input =
                            tao_scroll::pointer_scroll(point, delta, phase, app.scale_factor);
                        app.handle_pointer_input(input);
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
                if app.opaque_wallpaper_changed() {
                    app.request_redraw();
                }
                if app
                    .bar
                    .next_dock_deadline(now)
                    .is_some_and(|deadline| deadline <= now)
                {
                    let update = app.bar.poll_transport();
                    app.handle_runtime_update(update);
                    app.sync_transport_wake();
                }
                let dock = app.bar.next_dock_deadline(Instant::now());
                let wallpaper = app.wallpaper_poll_deadline(Instant::now());
                match (dock, wallpaper) {
                    (Some(a), Some(b)) => {
                        *control_flow = ControlFlow::WaitUntil(a.min(b));
                    }
                    (Some(deadline), None) | (None, Some(deadline)) => {
                        *control_flow = ControlFlow::WaitUntil(deadline);
                    }
                    (None, None) => {}
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
        let size = initial_logical_size(PhysicalSize::new(3840, 2160), 2.0, 52.5, BarEdge::Top);

        assert_eq!(size.width, 1920.0);
        assert_eq!(size.height, 52.5);
    }

    #[test]
    fn oversized_surface_is_rejected_before_presenter_allocation() {
        assert_eq!(bgra_frame_len(7680, 4320).unwrap(), 7680_usize * 4320 * 4);
        assert_eq!(bgra_frame_len(0, 2160).unwrap(), 0);
        assert!(bgra_frame_len(u32::from(u16::MAX), u32::from(u16::MAX)).is_err());
        assert!(bgra_frame_len(0, u32::MAX).is_err());
    }
}
