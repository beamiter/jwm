use anyhow::{Result, anyhow};
use glow::HasContext;
use glutin::prelude::GlSurface;
use log::{debug, warn};
use pango::FontDescription;
use std::cell::{Cell, RefCell};
use std::env;
use std::ffi::CString;
use std::num::NonZeroU32;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd};
use std::ptr::NonNull;
use std::time::{Duration, Instant};
use xbar_core::glass::DEFAULT_BACKGROUND_OPACITY;
use xbar_core::linux::{AlignedTimer, Epoll};
use xbar_core::presentation::Point;
use xbar_core::render::cairo::{CairoBar, PointerInput};
use xbar_core::{
    BarPlacement, BarRuntime, DockProperty, DockPropertyValue, DockWindowSpec, MonitorGeometry,
    NotifierChange, RuntimeUpdate, TransportNotifierSlot, TransportRecoveryConfig,
};
use xbar_linux_actions::{EffectRouter, GeometryRequest};
use xcb::{self, Xid, x};

const BAR_NAME: &str = "xcb_glow_bar";
const X_TOKEN: u64 = 1;
const TIMER_TOKEN: u64 = 2;
const SHARED_TOKEN: u64 = 3;
const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

// ---------------- GL state ----------------
struct GlState {
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

impl GlState {
    fn new(
        raw_display: raw_window_handle::RawDisplayHandle,
        raw_window: raw_window_handle::RawWindowHandle,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        use glutin::config::ConfigTemplateBuilder;
        use glutin::context::{ContextApi, ContextAttributesBuilder, NotCurrentGlContext};
        use glutin::display::{Display, DisplayApiPreference, GlDisplay};
        use glutin::surface::{SurfaceAttributesBuilder, WindowSurface};

        let preference = DisplayApiPreference::EglThenGlx(Box::new(|_register| {
            // Do not install a custom Xlib error hook. The X11 connection is
            // owned by the xcb crate; unhandled GLX errors are fatal regardless.
        }));
        let display = unsafe { Display::new(raw_display, preference) }
            .map_err(|error| anyhow!("failed to create GL display: {error}"))?;

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
                    .map_err(|error| anyhow!("failed to find GL config: {error}"))?
                    .next()
                    .ok_or_else(|| anyhow!("no compatible GL config found"))?
            }
        };

        let context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(None))
            .build(Some(raw_window));
        let not_current = unsafe { display.create_context(&config, &context_attributes) }
            .map_err(|error| anyhow!("failed to create GL context: {error}"))?;

        let surface_attributes = SurfaceAttributesBuilder::<WindowSurface>::new()
            .with_srgb(Some(true))
            .build(
                raw_window,
                NonZeroU32::new(width.max(1)).unwrap(),
                NonZeroU32::new(height.max(1)).unwrap(),
            );
        let surface = unsafe { display.create_window_surface(&config, &surface_attributes) }
            .map_err(|error| anyhow!("failed to create GL surface: {error}"))?;

        let context = not_current
            .make_current(&surface)
            .map_err(|error| anyhow!("failed to make GL context current: {error}"))?;

        let gl = unsafe {
            glow::Context::from_loader_function(|symbol| {
                let symbol = CString::new(symbol).unwrap_or_default();
                display.get_proc_address(&symbol) as *const _
            })
        };

        let program = build_quad_program(&gl)?;
        let vao = build_quad_mesh(&gl);
        let texture = create_texture(&gl);

        unsafe {
            gl.viewport(0, 0, width as i32, height as i32);
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

    fn resize(&self, width: u32, height: u32) {
        if let (Some(width), Some(height)) = (NonZeroU32::new(width), NonZeroU32::new(height)) {
            self.surface.resize(&self.context, width, height);
            unsafe {
                self.gl
                    .viewport(0, 0, width.get() as i32, height.get() as i32);
            }
        }
    }

    fn redraw(
        &self,
        window: &WindowAdapter<'_>,
        bar: &mut CairoBar,
        width: u32,
        height: u32,
        scale_factor: f64,
    ) -> Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }

        let mut frame = vec![
            0u8;
            (width as usize)
                .saturating_mul(height as usize)
                .saturating_mul(4)
        ];
        loop {
            bar.render_into_bgra(
                &mut frame,
                width,
                height,
                width.saturating_mul(4),
                scale_factor,
            )?;

            upload_bgra_frame(&self.gl, self.texture, width, height, &frame);
            draw_fullscreen_quad(&self.gl, self.vao, self.program, self.texture);

            self.surface
                .swap_buffers(&self.context)
                .map_err(|error| anyhow!("swap buffers failed: {error}"))?;

            let update = bar.take_pending_runtime();
            let needs_redraw = if update.is_empty() {
                false
            } else {
                window.apply_runtime_update(update)?
            };
            let _ = bar.runtime_mut().take_changes();
            if !needs_redraw {
                break;
            }
        }

        Ok(())
    }
}

// ---------------- Compositor detection ----------------
/// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
/// selection. With a compositor the bar renders per-pixel alpha through a
/// 32-bit visual and lets the compositor blend what lies behind it; without
/// one it paints the opaque fallback background. Sampled once at startup —
/// the visual is a creation-time decision, so a compositor toggled later
/// only takes effect on restart — and owning the selection promises
/// compositing, not that anything actually blurs behind the bar.
fn compositor_active(conn: &xcb::Connection, screen_num: i32) -> bool {
    let Ok(atom) = intern_atom(conn, &format!("_NET_WM_CM_S{screen_num}")) else {
        return false;
    };
    let cookie = conn.send_request(&x::GetSelectionOwner { selection: atom });
    conn.wait_for_reply(cookie)
        .map(|reply| !reply.owner().is_none())
        .unwrap_or(false)
}

/// A 32-bit TrueColor visual for translucent rendering, if the server has one.
fn find_argb_visual(screen: &x::Screen) -> Option<x::Visualtype> {
    for depth in screen.allowed_depths() {
        if depth.depth() == 32 {
            for visual in depth.visuals() {
                if visual.class() == x::VisualClass::TrueColor {
                    return Some(*visual);
                }
            }
        }
    }
    None
}

// ---------------- EWMH ----------------
fn intern_atom(conn: &xcb::Connection, name: &str) -> Result<x::Atom> {
    let cookie = conn.send_request(&x::InternAtom {
        only_if_exists: false,
        name: name.as_bytes(),
    });
    Ok(conn.wait_for_reply(cookie)?.atom())
}

/// Write core-described dock properties with this connection. Atom names come
/// from `DockWindowSpec`; only interning and the property calls live here.
fn write_dock_properties(
    conn: &xcb::Connection,
    win: x::Window,
    properties: &[DockProperty],
) -> Result<()> {
    let atom_type = intern_atom(conn, "ATOM")?;
    let cardinal_type = intern_atom(conn, "CARDINAL")?;
    for property in properties {
        let name = intern_atom(conn, property.name)?;
        match &property.value {
            DockPropertyValue::Atoms(values) => {
                let values = values
                    .iter()
                    .map(|value| Ok(intern_atom(conn, value)?.resource_id()))
                    .collect::<Result<Vec<u32>>>()?;
                change_property_32(conn, win, name, atom_type, &values)?;
            }
            DockPropertyValue::Cardinals(values) => {
                change_property_32(conn, win, name, cardinal_type, values)?;
            }
            DockPropertyValue::Utf8Text(text) => {
                let utf8_string = intern_atom(conn, "UTF8_STRING")?;
                change_property_8(conn, win, name, utf8_string, text.as_bytes())?;
            }
        }
    }
    Ok(())
}

fn dock_spec(x: i32, y: i32, width: u32, bar_height: u16) -> DockWindowSpec {
    DockWindowSpec::top(
        BAR_NAME,
        BarPlacement {
            x,
            y,
            width,
            height: u32::from(bar_height),
        },
    )
}

fn change_property_32(
    conn: &xcb::Connection,
    win: x::Window,
    property: x::Atom,
    property_type: x::Atom,
    data: &[u32],
) -> Result<()> {
    conn.send_and_check_request(&x::ChangeProperty {
        mode: x::PropMode::Replace,
        window: win,
        property,
        r#type: property_type,
        data,
    })?;
    Ok(())
}

fn change_property_8(
    conn: &xcb::Connection,
    win: x::Window,
    property: x::Atom,
    property_type: x::Atom,
    data: &[u8],
) -> Result<()> {
    conn.send_and_check_request(&x::ChangeProperty {
        mode: x::PropMode::Replace,
        window: win,
        property,
        r#type: property_type,
        data,
    })?;
    Ok(())
}

// ---------------- Platform integration ----------------
struct WindowAdapter<'a> {
    conn: &'a xcb::Connection,
    screen: &'a x::Screen,
    win: x::Window,
    bar_height: Cell<u16>,
    effects: RefCell<EffectRouter>,
}

impl WindowAdapter<'_> {
    fn sync_bar_height(&self, bar: &mut CairoBar, height: u16) {
        self.bar_height.set(height);
        bar.config_mut().bar_height = f32::from(height);
    }

    fn apply_runtime_update(&self, update: RuntimeUpdate) -> Result<bool> {
        self.effects.borrow_mut().route(update, |request| {
            let geometry = match request {
                GeometryRequest::Apply(geometry) => geometry,
                GeometryRequest::Clear => MonitorGeometry {
                    x: 0,
                    y: 0,
                    width: u32::from(self.screen.width_in_pixels()),
                    height: u32::from(self.screen.height_in_pixels()),
                },
            };
            self.apply_geometry(geometry)
        })
    }

    fn apply_geometry(&self, geometry: MonitorGeometry) -> Result<()> {
        let width = geometry.width.max(1);
        let bar_height = self.bar_height.get();
        self.conn.send_and_check_request(&x::ConfigureWindow {
            window: self.win,
            value_list: &[
                x::ConfigWindow::X(geometry.x),
                x::ConfigWindow::Y(geometry.y),
                x::ConfigWindow::Width(width),
                x::ConfigWindow::Height(u32::from(bar_height)),
            ],
        })?;
        let spec = dock_spec(geometry.x, geometry.y, width, bar_height);
        write_dock_properties(self.conn, self.win, &spec.strut_properties())?;
        self.conn.flush()?;
        Ok(())
    }
}

fn route_pointer_input(
    window: &WindowAdapter<'_>,
    bar: &mut CairoBar,
    input: PointerInput,
) -> Result<bool> {
    let update = bar.handle_pointer(input);
    let pointer_redraw = update.needs_redraw();
    let runtime_redraw = window.apply_runtime_update(update.into_runtime())?;
    Ok(pointer_redraw || runtime_redraw)
}

fn build_quad_program(gl: &glow::Context) -> Result<glow::NativeProgram> {
    unsafe {
        let program = gl
            .create_program()
            .map_err(|error| anyhow!("create shader program: {error}"))?;

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
        )?;

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
        )?;

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
            .map_err(|error| anyhow!("create shader: {error}"))?;
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

#[allow(clippy::too_many_arguments)]
fn handle_x_event(
    event: xcb::Event,
    gl_state: &GlState,
    window: &WindowAdapter<'_>,
    current_width: &mut u16,
    current_height: &mut u16,
    bar: &mut CairoBar,
) -> Result<()> {
    let mut should_redraw = false;

    match event {
        xcb::Event::X(x::Event::Expose(event)) => {
            if event.count() == 0 {
                should_redraw = true;
            }
        }
        xcb::Event::X(x::Event::ConfigureNotify(event)) if event.window() == window.win => {
            *current_width = event.width();
            *current_height = event.height();
            window.sync_bar_height(bar, event.height());
            gl_state.resize(u32::from(event.width()), u32::from(event.height()));
            should_redraw = true;
        }
        xcb::Event::X(x::Event::EnterNotify(event)) => {
            should_redraw = route_pointer_input(
                window,
                bar,
                PointerInput::Move(Point::new(
                    f32::from(event.event_x()),
                    f32::from(event.event_y()),
                )),
            )?;
        }
        xcb::Event::X(x::Event::LeaveNotify(_)) => {
            should_redraw = route_pointer_input(window, bar, PointerInput::Leave)?;
        }
        xcb::Event::X(x::Event::MotionNotify(event)) => {
            should_redraw = route_pointer_input(
                window,
                bar,
                PointerInput::Move(Point::new(
                    f32::from(event.event_x()),
                    f32::from(event.event_y()),
                )),
            )?;
        }
        xcb::Event::X(x::Event::ButtonPress(event)) => {
            let point = Point::new(f32::from(event.event_x()), f32::from(event.event_y()));
            if let Some(input) = PointerInput::from_x11_button(point, event.detail(), true) {
                should_redraw = route_pointer_input(window, bar, input)?;
            }
        }
        xcb::Event::X(x::Event::ButtonRelease(event)) => {
            let point = Point::new(f32::from(event.event_x()), f32::from(event.event_y()));
            if let Some(input) = PointerInput::from_x11_button(point, event.detail(), false) {
                should_redraw = route_pointer_input(window, bar, input)?;
            }
        }
        _ => {}
    }

    if should_redraw {
        gl_state.redraw(
            window,
            bar,
            u32::from(*current_width),
            u32::from(*current_height),
            1.0,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn drain_x_events(
    gl_state: &GlState,
    window: &WindowAdapter<'_>,
    current_width: &mut u16,
    current_height: &mut u16,
    bar: &mut CairoBar,
) -> Result<()> {
    loop {
        match window.conn.poll_for_event() {
            Ok(Some(event)) => {
                handle_x_event(event, gl_state, window, current_width, current_height, bar)?
            }
            Ok(None) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
}

fn sync_notifier(
    slot: &mut TransportNotifierSlot,
    runtime: &BarRuntime,
    epoll: &Epoll,
) -> Result<()> {
    if let NotifierChange::Replaced { fd, .. } = slot.sync(runtime)? {
        epoll.add(fd, SHARED_TOKEN)?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let shared_path = env::args().skip(1).last().unwrap_or_default();
    xbar_core::logging::init(BAR_NAME, &shared_path)?;
    let app_config = xbar_core::config::BarConfig::load_default()?;

    let runtime = if shared_path.is_empty() {
        BarRuntime::new(app_config.model_config())?
    } else {
        let recovery = TransportRecoveryConfig::new(shared_path.clone(), TRANSPORT_RETRY_INTERVAL)?;
        BarRuntime::with_managed_transport(app_config.model_config(), recovery)?
    };

    let (conn, screen_num) = xcb::Connection::connect(None)?;
    let setup = conn.get_setup();
    let screen = setup
        .roots()
        .nth(screen_num as usize)
        .ok_or_else(|| anyhow!("no X screen found"))?;

    // Prefer real translucency when a compositor can blend it; otherwise the
    // bar stays a plain opaque window. Checked once, here, because the visual
    // is a creation-time decision — and the selection only says compositing
    // is on, not that the compositor blurs behind the bar.
    let argb_visual = if compositor_active(&conn, screen_num) {
        find_argb_visual(screen)
    } else {
        None
    };
    let (window_depth, window_visual) = match &argb_visual {
        Some(visual) => (32, visual.visual_id()),
        None => (screen.root_depth(), screen.root_visual()),
    };
    let translucent = argb_visual.is_some();

    let mut presentation = app_config.presentation.clone();
    // The macOS-style template icons every bar renders from.
    presentation.apply_nerd_font_icons_if_stock();
    let bar_height = presentation
        .bar_height
        .round()
        .clamp(1.0, f32::from(u16::MAX)) as u16;
    let font = FontDescription::from_string(&app_config.font);
    let mut bar = CairoBar::new(runtime, presentation, font);
    // Under a compositor the background is a translucent wash the desktop
    // shows through, with an explicit config value still winning; without one
    // the palette background must paint fully opaque, so the configured
    // opacity is deliberately ignored rather than washed over nothing.
    if translucent {
        let opacity = app_config
            .background_opacity
            .unwrap_or(DEFAULT_BACKGROUND_OPACITY);
        bar.renderer_mut().set_background_opacity(Some(opacity));
    } else {
        bar.renderer_mut().set_background_opacity(None);
    }

    let win = conn.generate_id();
    let mut current_width = screen.width_in_pixels();
    let mut current_height = bar_height;
    let event_mask = x::EventMask::EXPOSURE
        | x::EventMask::STRUCTURE_NOTIFY
        | x::EventMask::BUTTON_PRESS
        | x::EventMask::BUTTON_RELEASE
        | x::EventMask::POINTER_MOTION
        | x::EventMask::ENTER_WINDOW
        | x::EventMask::LEAVE_WINDOW;
    if translucent {
        // A depth-32 window needs an explicit border pixel and colormap for
        // its non-default visual, or CreateWindow fails with BadMatch.
        let colormap = conn.generate_id();
        conn.send_and_check_request(&x::CreateColormap {
            alloc: x::ColormapAlloc::None,
            mid: colormap,
            window: screen.root(),
            visual: window_visual,
        })?;
        conn.send_and_check_request(&x::CreateWindow {
            depth: window_depth,
            wid: win,
            parent: screen.root(),
            x: 0,
            y: 0,
            width: current_width,
            height: current_height,
            border_width: 0,
            class: x::WindowClass::InputOutput,
            visual: window_visual,
            value_list: &[
                x::Cw::BackPixel(0),
                x::Cw::BorderPixel(0),
                x::Cw::EventMask(event_mask),
                x::Cw::Colormap(colormap),
            ],
        })?;
    } else {
        conn.send_and_check_request(&x::CreateWindow {
            depth: x::COPY_FROM_PARENT as u8,
            wid: win,
            parent: screen.root(),
            x: 0,
            y: 0,
            width: current_width,
            height: current_height,
            border_width: 0,
            class: x::WindowClass::InputOutput,
            visual: window_visual,
            value_list: &[
                x::Cw::BackPixmap(x::Pixmap::none()),
                x::Cw::EventMask(event_mask),
            ],
        })?;
    }

    let spec = dock_spec(0, 0, u32::from(current_width), current_height);
    write_dock_properties(&conn, win, &spec.properties())?;
    conn.send_and_check_request(&x::MapWindow { window: win })?;
    conn.flush()?;

    let window = WindowAdapter {
        conn: &conn,
        screen,
        win,
        bar_height: Cell::new(bar_height),
        effects: RefCell::new(EffectRouter::default()),
    };

    let raw_display =
        raw_window_handle::RawDisplayHandle::Xcb(raw_window_handle::XcbDisplayHandle::new(
            NonNull::new(conn.get_raw_conn().cast()),
            screen_num,
        ));
    let mut window_handle = raw_window_handle::XcbWindowHandle::new(
        NonZeroU32::new(win.resource_id()).expect("generated X11 window id is non-zero"),
    );
    window_handle.visual_id = NonZeroU32::new(window_visual);
    let raw_window = raw_window_handle::RawWindowHandle::Xcb(window_handle);
    let gl_state = GlState::new(
        raw_display,
        raw_window,
        u32::from(current_width),
        u32::from(current_height),
    )?;

    // Seed providers and consume any snapshot that was queued before startup.
    let mut initial_update = bar.tick();
    initial_update.merge(bar.poll_transport());
    window.apply_runtime_update(initial_update)?;
    gl_state.redraw(
        &window,
        &mut bar,
        u32::from(current_width),
        u32::from(current_height),
        1.0,
    )?;

    let timer = AlignedTimer::new(Duration::from_secs(1))?;
    let mut epoll = Epoll::new()?;
    // SAFETY: the connection outlives the epoll registration and owns its
    // descriptor for the whole program.
    let conn_fd = unsafe { BorrowedFd::borrow_raw(window.conn.as_raw_fd()) };
    epoll.add(conn_fd, X_TOKEN)?;
    epoll.add(timer.as_fd(), TIMER_TOKEN)?;
    let mut notifier_slot = TransportNotifierSlot::new(true);
    sync_notifier(&mut notifier_slot, bar.runtime(), &epoll)?;

    let mut ready_tokens = Vec::new();
    loop {
        ready_tokens.clear();
        let now = Instant::now();
        let dock_timeout = bar
            .next_dock_deadline(now)
            .map(|deadline| deadline.saturating_duration_since(now));
        ready_tokens.extend(epoll.wait_timeout(dock_timeout)?);
        if ready_tokens.is_empty() {
            // A Dock deadline is independent from the aligned one-second
            // provider timer. Service it promptly so a final magnified anchor
            // and a command-ring backpressure retry are not stranded until
            // the next clock tick.
            let update = bar.poll_transport();
            let needs_redraw = window.apply_runtime_update(update)?;
            sync_notifier(&mut notifier_slot, bar.runtime(), &epoll)?;
            if needs_redraw {
                gl_state.redraw(
                    &window,
                    &mut bar,
                    u32::from(current_width),
                    u32::from(current_height),
                    1.0,
                )?;
            }
            continue;
        }
        for token in &ready_tokens {
            match *token {
                X_TOKEN => drain_x_events(
                    &gl_state,
                    &window,
                    &mut current_width,
                    &mut current_height,
                    &mut bar,
                )?,
                TIMER_TOKEN => {
                    if timer.drain()? > 0 {
                        let mut update = bar.tick();
                        update.merge(bar.poll_transport());
                        let needs_redraw = window.apply_runtime_update(update)?;
                        sync_notifier(&mut notifier_slot, bar.runtime(), &epoll)?;
                        if needs_redraw {
                            gl_state.redraw(
                                &window,
                                &mut bar,
                                u32::from(current_width),
                                u32::from(current_height),
                                1.0,
                            )?;
                        }
                    }
                }
                SHARED_TOKEN => {
                    if let Some(notifier) = notifier_slot.notifier() {
                        notifier.drain()?;
                        let update = bar.poll_transport();
                        let needs_redraw = window.apply_runtime_update(update)?;
                        sync_notifier(&mut notifier_slot, bar.runtime(), &epoll)?;
                        if needs_redraw {
                            gl_state.redraw(
                                &window,
                                &mut bar,
                                u32::from(current_width),
                                u32::from(current_height),
                                1.0,
                            )?;
                        }
                    } else {
                        warn!("received shared token without an owned notifier");
                    }
                }
                token => debug!("unexpected epoll token: {token}"),
            }
        }
    }
}
