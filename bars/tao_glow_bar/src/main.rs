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
    dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy},
    monitor::MonitorHandle,
    window::{Window, WindowBuilder},
};

use glow::HasContext;
use glutin::prelude::GlSurface;
use x11rb::rust_connection::RustConnection;
use xbar_core::glass::DEFAULT_BACKGROUND_OPACITY;
use xbar_core::{
    AlignedWakeThread, BarPlacement, BarRuntime, RuntimeUpdate, TransportRecoveryConfig,
    TransportWakeSlot, WakeAck,
    logging::init as initialize_logging,
    presentation::Point,
    render::cairo::{CairoBar, CpuCanvas, PointerButton, PointerInput},
};
use xbar_linux_actions::{EffectRouter, GeometryRequest};

const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug)]
enum UserEvent {
    Tick,
    SharedUpdated(WakeAck),
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
}

impl GlPresenter {
    fn new(window: &Window, physical_size: PhysicalSize<u32>) -> Result<Self> {
        let (display, _config, surface, context, gl) = create_gl_surface(window)?;
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
        })
    }

    fn redraw(&mut self, bar: &mut CairoBar, width: u32, height: u32, scale: f64) -> Result<()> {
        let mut frame = vec![
            0u8;
            (width as usize)
                .saturating_mul(height as usize)
                .saturating_mul(4)
        ];
        bar.render_into_bgra(&mut frame, width, height, width.saturating_mul(4), scale)?;
        let _ = bar.runtime_mut().take_changes();

        upload_bgra_frame(&self.gl, self.texture, width, height, &frame);
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

    fn redraw(&mut self, bar: &mut CairoBar, width: u32, height: u32, scale: f64) -> Result<()> {
        let frame = self.canvas.render(bar, width, height, scale)?;
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
    fn redraw(&mut self, bar: &mut CairoBar, width: u32, height: u32, scale: f64) -> Result<()> {
        match self {
            Self::OpenGl(presenter) => presenter.redraw(bar, width, height, scale),
            Self::Software(presenter) => presenter.redraw(bar, width, height, scale),
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
    window: Rc<Window>,
    presenter: Presenter,
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
            window,
            presenter,
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
    fn resolve_translucency(&mut self) {
        if !self.depth_check_pending {
            return;
        }
        use raw_window_handle::HasWindowHandle as _;
        let Ok(handle) = self.window.window_handle() else {
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

        self.presenter
            .redraw(&mut self.bar, width, height, self.scale_factor)
    }

    fn request_redraw(&self) {
        self.window.request_redraw();
    }

    fn resize(&mut self, physical_size: PhysicalSize<u32>) {
        self.last_physical_size = physical_size;
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
        ack.ack();
        self.apply_transport_update(update);
    }

    fn apply_transport_update(&mut self, update: RuntimeUpdate) {
        let needs_redraw = self.apply_runtime_update(update);
        self.sync_transport_wake();
        if needs_redraw && let Err(error) = self.redraw() {
            warn!("immediate shared-state redraw failed: {error:#}");
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

fn upload_bgra_frame(
    gl: &glow::Context,
    texture: glow::NativeTexture,
    width: u32,
    height: u32,
    frame: &[u8],
) {
    unsafe {
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA as i32,
            width as i32,
            height as i32,
            0,
            glow::BGRA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(frame)),
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
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
    bar_height: f32,
) -> LogicalSize<f64> {
    LogicalSize::new(
        f64::from(screen_size.width) / scale_factor,
        f64::from(bar_height),
    )
}

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
    initialize_logging("tao_glow_bar", &shared_path)?;

    let app_config = xbar_core::config::BarConfig::load_default()?;
    let runtime = if shared_path.is_empty() {
        BarRuntime::new(app_config.model_config())?
    } else {
        let recovery = TransportRecoveryConfig::new(shared_path.clone(), TRANSPORT_RETRY_INTERVAL)?;
        BarRuntime::with_managed_transport(app_config.model_config(), recovery)?
    };
    let mut presentation = app_config.presentation.clone();
    presentation.apply_nerd_font_icons_if_stock();
    let font = FontDescription::from_string(&app_config.font);
    let mut bar = CairoBar::new(runtime, presentation, font);

    let (x11, compositor_active) = match x11rb::connect(None) {
        Ok((conn, screen_num)) => {
            let active = compositor_active(&conn, screen_num);
            (Some(conn), active)
        }
        Err(_) => (None, false),
    };
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
            .with_title("tao_glow_bar")
            .with_inner_size(logical_size)
            .with_decorations(false)
            .with_resizable(true)
            .with_visible(true)
            .with_transparent(compositor_active)
            .build(&event_loop)
            .context("failed to build tao window")?,
    );

    info!("creating OpenGL surface for tao window");
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
