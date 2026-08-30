use anyhow::{Context as _, Result};
use glow::HasContext;
use glutin::prelude::GlSurface;
use log::warn;
use pango::FontDescription;
use std::env;
use std::ffi::CString;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};
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
    presentation::Point,
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
    window: Option<Rc<Window>>,
    window_id: Option<WindowId>,
    gl: Option<glow::Context>,
    surface: Option<glutin::surface::Surface<glutin::surface::WindowSurface>>,
    context: Option<glutin::context::PossiblyCurrentContext>,
    /// Kept alive to back the GL context and surface.
    #[allow(dead_code)]
    display: Option<glutin::display::Display>,
    program: Option<glow::NativeProgram>,
    vao: Option<glow::NativeVertexArray>,
    texture: Option<glow::NativeTexture>,
    bar: CairoBar,
    scale_factor: f64,
    logical_size: LogicalSize<f64>,
    default_logical_size: LogicalSize<f64>,
    last_physical_size: PhysicalSize<u32>,
    last_cursor_pos: Option<Point>,
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
            gl: None,
            surface: None,
            context: None,
            display: None,
            program: None,
            vao: None,
            texture: None,
            bar,
            scale_factor,
            logical_size,
            default_logical_size: logical_size,
            last_physical_size: PhysicalSize::new(
                logical_size.width.round() as u32,
                logical_size.height.round() as u32,
            ),
            last_cursor_pos: None,
            proxy,
            transport_wake: TransportWakeSlot::new(true),
            effects: EffectRouter::default(),
            x11,
            compositor_active,
        }
    }

    fn init_gl(&mut self, window: &Window) -> Result<()> {
        let (display, _config, surface, context, gl) = create_gl_surface(window)?;

        let program = build_quad_program(&gl)?;
        let vao = build_quad_mesh(&gl);
        let texture = create_texture(&gl);

        unsafe {
            let size = window.inner_size();
            gl.viewport(0, 0, size.width as i32, size.height as i32);
            gl.enable(glow::BLEND);
            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
        }

        self.display = Some(display);
        self.surface = Some(surface);
        self.context = Some(context);
        self.gl = Some(gl);
        self.program = Some(program);
        self.vao = Some(vao);
        self.texture = Some(texture);
        Ok(())
    }

    fn redraw(&mut self) -> Result<()> {
        let width = self.last_physical_size.width;
        let height = self.last_physical_size.height;
        if self.window.is_none() || width == 0 || height == 0 {
            return Ok(());
        }

        let gl = self.gl.as_ref().expect("GL initialized in resumed");
        let surface = self.surface.as_ref().expect("GL initialized in resumed");
        let context = self.context.as_ref().expect("GL initialized in resumed");
        let program = self.program.expect("GL initialized in resumed");
        let vao = self.vao.expect("GL initialized in resumed");
        let texture = self.texture.expect("GL initialized in resumed");

        let mut frame = vec![
            0u8;
            (width as usize)
                .saturating_mul(height as usize)
                .saturating_mul(4)
        ];
        self.bar.render_into_bgra(
            &mut frame,
            width,
            height,
            width.saturating_mul(4),
            self.scale_factor,
        )?;
        let _ = self.bar.runtime_mut().take_changes();

        upload_bgra_frame(gl, texture, width, height, &frame);
        draw_fullscreen_quad(gl, vao, program, texture);

        surface
            .swap_buffers(context)
            .map_err(|error| anyhow::anyhow!("swap buffers failed: {error}"))?;

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
        if let (Some(surface), Some(context), Some(gl)) = (
            self.surface.as_ref(),
            self.context.as_ref(),
            self.gl.as_ref(),
        ) && let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        {
            surface.resize(context, width, height);
            unsafe {
                gl.viewport(0, 0, size.width as i32, size.height as i32);
            }
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
            .with_title("winit_glow_bar")
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

        if let Err(error) = self.init_gl(window.as_ref()) {
            panic!("failed to initialize OpenGL: {error:#}");
        }

        let size = window.inner_size();
        self.window_id = Some(window.id());
        self.window = Some(window);
        self.last_physical_size = size;

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

    let preference = DisplayApiPreference::EglThenGlx(Box::new(|_register| {
        // Do not install a custom Xlib error hook. Winit owns the display
        // connection; unhandled GLX errors are fatal regardless.
    }));
    let display = unsafe { Display::new(display_handle.into(), preference) }
        .map_err(|error| anyhow::anyhow!("failed to create GL display: {error}"))?;

    let raw_window_handle = window_handle.into();
    let template = ConfigTemplateBuilder::new().with_alpha_size(8).build();
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
            let template = ConfigTemplateBuilder::new().build();
            unsafe { display.find_configs(template) }
                .map_err(|error| anyhow::anyhow!("failed to find GL config: {error}"))?
                .next()
                .ok_or_else(|| anyhow::anyhow!("no compatible GL config found"))?
        }
    };

    let context_attributes = ContextAttributesBuilder::new()
        .with_context_api(ContextApi::OpenGl(None))
        .build(Some(raw_window_handle));
    let not_current = unsafe { display.create_context(&config, &context_attributes) }
        .map_err(|error| anyhow::anyhow!("failed to create GL context: {error}"))?;

    let size = window.inner_size();
    let surface_attributes = SurfaceAttributesBuilder::<WindowSurface>::new()
        .with_srgb(Some(true))
        .build(
            raw_window_handle,
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
    use glow::HasContext;
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
    use glow::HasContext;
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
    use glow::HasContext;
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
    use glow::HasContext;
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
    use glow::HasContext;
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
    use glow::HasContext;
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

/// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
/// selection.
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
    initialize_logging("winit_glow_bar", &shared_path)?;
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
