//! Headless GL test harness.
//!
//! Creates a surfaceless EGL context (OpenGL ES 3 for the Wayland backend's
//! `#version 300 es` shaders, or desktop GL 3.3 core for the X11 backend's
//! `#version 330 core` shaders) so the real shaders in both backends'
//! `compositor::shaders` modules can be compiled and exercised under
//! `cargo test`, without a display server or window. On a machine with no usable
//! EGL/GL the harness returns `None` and the tests skip, so this never breaks CI
//! on boxes that lack Mesa. Where Mesa is present (including the llvmpipe
//! software rasteriser) the tests run for real and catch shader-compile
//! regressions and pixel-math bugs that previously could only be found by
//! eyeballing a live compositor.

use glow::HasContext as _;
use std::os::raw::c_void;

/// Which client API / profile the headless context exposes.
#[derive(Clone, Copy)]
enum GlApi {
    /// OpenGL ES 3 — for the Wayland backend's `#version 300 es` shaders.
    Gles3,
    /// Desktop OpenGL 3.3 core — for the X11 backend's `#version 330 core` shaders.
    GlCore33,
}

struct HeadlessGl {
    gl: glow::Context,
    display: egl::EGLDisplay,
}

impl HeadlessGl {
    fn new(api: GlApi) -> Option<Self> {
        // EGL enums not surfaced by the egl 0.2.7 crate.
        const EGL_OPENGL_BIT: egl::EGLint = 0x0008;
        const EGL_CONTEXT_MINOR_VERSION: egl::EGLint = 0x30FB;
        const EGL_CONTEXT_OPENGL_PROFILE_MASK: egl::EGLint = 0x30FD;
        const EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT: egl::EGLint = 0x0001;

        let display = egl::get_display(egl::EGL_DEFAULT_DISPLAY)?;
        let (mut major, mut minor) = (0, 0);
        if !egl::initialize(display, &mut major, &mut minor) {
            return None;
        }

        let (egl_api, renderable, ctx_attrs): (egl::EGLenum, egl::EGLint, Vec<egl::EGLint>) =
            match api {
                GlApi::Gles3 => (
                    egl::EGL_OPENGL_ES_API,
                    // ES2-renderable configs also serve ES3 contexts on Mesa.
                    egl::EGL_OPENGL_ES2_BIT,
                    vec![egl::EGL_CONTEXT_CLIENT_VERSION, 3, egl::EGL_NONE],
                ),
                GlApi::GlCore33 => (
                    egl::EGL_OPENGL_API,
                    EGL_OPENGL_BIT,
                    vec![
                        // EGL_CONTEXT_CLIENT_VERSION aliases EGL_CONTEXT_MAJOR_VERSION.
                        egl::EGL_CONTEXT_CLIENT_VERSION,
                        3,
                        EGL_CONTEXT_MINOR_VERSION,
                        3,
                        EGL_CONTEXT_OPENGL_PROFILE_MASK,
                        EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT,
                        egl::EGL_NONE,
                    ],
                ),
            };

        if !egl::bind_api(egl_api) {
            return None;
        }
        let cfg_attrs = [
            egl::EGL_SURFACE_TYPE,
            egl::EGL_PBUFFER_BIT,
            egl::EGL_RENDERABLE_TYPE,
            renderable,
            egl::EGL_RED_SIZE,
            8,
            egl::EGL_GREEN_SIZE,
            8,
            egl::EGL_BLUE_SIZE,
            8,
            egl::EGL_ALPHA_SIZE,
            8,
            egl::EGL_NONE,
        ];
        let config = egl::choose_config(display, &cfg_attrs, 1)?;
        let context = egl::create_context(display, config, egl::EGL_NO_CONTEXT, &ctx_attrs)?;
        if !egl::make_current(display, egl::EGL_NO_SURFACE, egl::EGL_NO_SURFACE, context) {
            return None;
        }
        // Mesa advertises EGL_KHR_get_all_proc_addresses, so core GL/GLES entry
        // points resolve through eglGetProcAddress.
        let gl = unsafe {
            glow::Context::from_loader_function(|s| egl::get_proc_address(s) as *const c_void)
        };
        Some(Self { gl, display })
    }
}

impl Drop for HeadlessGl {
    fn drop(&mut self) {
        egl::make_current(
            self.display,
            egl::EGL_NO_SURFACE,
            egl::EGL_NO_SURFACE,
            egl::EGL_NO_CONTEXT,
        );
    }
}

#[derive(Clone, Copy)]
enum Stage {
    Vertex,
    Fragment,
}

fn compile(gl: &glow::Context, stage: Stage, src: &str) -> Result<glow::Shader, String> {
    let ty = match stage {
        Stage::Vertex => glow::VERTEX_SHADER,
        Stage::Fragment => glow::FRAGMENT_SHADER,
    };
    unsafe {
        let sh = gl.create_shader(ty)?;
        gl.shader_source(sh, src);
        gl.compile_shader(sh);
        if !gl.get_shader_compile_status(sh) {
            let log = gl.get_shader_info_log(sh);
            gl.delete_shader(sh);
            return Err(log);
        }
        Ok(sh)
    }
}

fn link(gl: &glow::Context, vs: &str, fs: &str) -> Result<glow::Program, String> {
    unsafe {
        let v = compile(gl, Stage::Vertex, vs)?;
        let f = compile(gl, Stage::Fragment, fs)?;
        let prog = gl.create_program()?;
        gl.attach_shader(prog, v);
        gl.attach_shader(prog, f);
        gl.link_program(prog);
        let ok = gl.get_program_link_status(prog);
        gl.delete_shader(v);
        gl.delete_shader(f);
        if !ok {
            let log = gl.get_program_info_log(prog);
            gl.delete_program(prog);
            return Err(log);
        }
        Ok(prog)
    }
}

/// Column-major orthographic projection mapping pixel coords [0,w]x[0,h] to NDC.
fn ortho(w: f32, h: f32) -> [f32; 16] {
    [
        2.0 / w,
        0.0,
        0.0,
        0.0, //
        0.0,
        2.0 / h,
        0.0,
        0.0, //
        0.0,
        0.0,
        -1.0,
        0.0, //
        -1.0,
        -1.0,
        0.0,
        1.0,
    ]
}

fn read_center(gl: &glow::Context, w: i32, h: i32) -> [u8; 4] {
    let mut buf = [0u8; 4];
    unsafe {
        gl.read_pixels(
            w / 2,
            h / 2,
            1,
            1,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(&mut buf)),
        );
    }
    buf
}

fn create_quad_vao(gl: &glow::Context) -> (glow::VertexArray, glow::Buffer) {
    let vertices: [f32; 8] = [
        0.0, 0.0, //
        1.0, 0.0, //
        0.0, 1.0, //
        1.0, 1.0,
    ];
    let bytes = unsafe {
        std::slice::from_raw_parts(
            vertices.as_ptr().cast::<u8>(),
            vertices.len() * std::mem::size_of::<f32>(),
        )
    };

    let (vao, vbo) = unsafe {
        let vao = gl.create_vertex_array().unwrap();
        let vbo = gl.create_buffer().unwrap();
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
        gl.bind_buffer(glow::ARRAY_BUFFER, None);
        gl.bind_vertex_array(None);
        (vao, vbo)
    };

    (vao, vbo)
}

#[track_caller]
fn assert_pixel(got: [u8; 4], want: [u8; 4], tol: i32, label: &str) {
    for i in 0..4 {
        let d = (got[i] as i32 - want[i] as i32).abs();
        assert!(
            d <= tol,
            "{label}: channel {i} got {} want {} (tol {tol}); full got {:?} want {:?}",
            got[i],
            want[i],
            got,
            want
        );
    }
}

/// Render a fullscreen quad with `prog` over a solid `input` texel into a WxH
/// RGBA8 FBO and return the center pixel. The input is a 2x2 solid texture with
/// NEAREST/CLAMP_TO_EDGE, so every neighbour fetch returns the same texel —
/// this is what makes blur passes a pure identity on a flat color. `uniforms`
/// runs after the program is bound and the input texture is live on unit 0.
/// Vertex shaders read the same location-0 quad attribute as the real
/// compositor fullscreen passes.
fn render_quad(
    gl: &glow::Context,
    prog: glow::Program,
    input: [u8; 4],
    w: i32,
    h: i32,
    uniforms: impl FnOnce(&glow::Context),
) -> [u8; 4] {
    unsafe {
        let input_pixels: Vec<u8> = input.iter().copied().cycle().take(4 * 4).collect();
        let input_tex = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_2D, Some(input_tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA as i32,
            2,
            2,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&input_pixels)),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::NEAREST as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::NEAREST as i32,
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

        let out_tex = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_2D, Some(out_tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA as i32,
            w,
            h,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::NEAREST as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::NEAREST as i32,
        );
        let fbo = gl.create_framebuffer().unwrap();
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(out_tex),
            0,
        );
        assert_eq!(
            gl.check_framebuffer_status(glow::FRAMEBUFFER),
            glow::FRAMEBUFFER_COMPLETE,
            "output FBO incomplete"
        );

        gl.viewport(0, 0, w, h);
        gl.disable(glow::BLEND);
        gl.use_program(Some(prog));
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(input_tex));
        uniforms(gl);

        let (vao, vbo) = create_quad_vao(gl);
        gl.bind_vertex_array(Some(vao));
        gl.clear_color(0.0, 0.0, 0.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        gl.finish();
        let px = read_center(gl, w, h);

        gl.bind_vertex_array(None);
        gl.delete_buffer(vbo);
        gl.delete_vertex_array(vao);
        gl.delete_framebuffer(fbo);
        gl.delete_texture(out_tex);
        gl.delete_texture(input_tex);
        px
    }
}

/// Every shader constant in the Wayland backend's `shaders` module, tagged with
/// its pipeline stage. Keep in sync with the `pub const *_SHADER`/`*_VERTEX`/
/// `*_FRAGMENT` declarations — a missing entry just isn't compile-checked.
fn wayland_shaders() -> Vec<(&'static str, Stage, &'static str)> {
    use super::shaders as s;
    use Stage::{Fragment as F, Vertex as V};
    vec![
        ("VERTEX_SHADER", V, s::VERTEX_SHADER),
        ("FRAGMENT_SHADER", F, s::FRAGMENT_SHADER),
        ("SHADOW_FRAGMENT_SHADER", F, s::SHADOW_FRAGMENT_SHADER),
        ("BLUR_DOWN_VERTEX", V, s::BLUR_DOWN_VERTEX),
        ("BLUR_DOWN_FRAGMENT", F, s::BLUR_DOWN_FRAGMENT),
        ("BLUR_UP_FRAGMENT", F, s::BLUR_UP_FRAGMENT),
        ("BOX_BLUR_FRAGMENT", F, s::BOX_BLUR_FRAGMENT),
        ("BORDER_FRAGMENT_SHADER", F, s::BORDER_FRAGMENT_SHADER),
        (
            "POSTPROCESS_FRAGMENT_SHADER",
            F,
            s::POSTPROCESS_FRAGMENT_SHADER,
        ),
        ("HUD_FRAGMENT_SHADER", F, s::HUD_FRAGMENT_SHADER),
        ("HUD_TEXT_FRAGMENT_SHADER", F, s::HUD_TEXT_FRAGMENT_SHADER),
        ("CUBE_VERTEX_SHADER", V, s::CUBE_VERTEX_SHADER),
        ("CUBE_FRAGMENT_SHADER", F, s::CUBE_FRAGMENT_SHADER),
        ("PORTAL_FRAGMENT_SHADER", F, s::PORTAL_FRAGMENT_SHADER),
        (
            "TRANSITION_FRAGMENT_SHADER",
            F,
            s::TRANSITION_FRAGMENT_SHADER,
        ),
        ("EDGE_GLOW_FRAGMENT_SHADER", F, s::EDGE_GLOW_FRAGMENT_SHADER),
        (
            "MAGNIFIER_POSTPROCESS_FRAGMENT_SHADER",
            F,
            s::MAGNIFIER_POSTPROCESS_FRAGMENT_SHADER,
        ),
        ("TILT_VERTEX_SHADER", V, s::TILT_VERTEX_SHADER),
        ("TILT_FRAGMENT_SHADER", F, s::TILT_FRAGMENT_SHADER),
        ("WOBBLY_VERTEX_SHADER", V, s::WOBBLY_VERTEX_SHADER),
        ("PARTICLE_VERTEX_SHADER", V, s::PARTICLE_VERTEX_SHADER),
        ("PARTICLE_FRAGMENT_SHADER", F, s::PARTICLE_FRAGMENT_SHADER),
        (
            "OVERVIEW_BG_FRAGMENT_SHADER",
            F,
            s::OVERVIEW_BG_FRAGMENT_SHADER,
        ),
        ("GENIE_VERTEX_SHADER", V, s::GENIE_VERTEX_SHADER),
        ("TEMPORAL_BLUR_MIX_VERTEX", V, s::TEMPORAL_BLUR_MIX_VERTEX),
        (
            "TEMPORAL_BLUR_MIX_FRAGMENT",
            F,
            s::TEMPORAL_BLUR_MIX_FRAGMENT,
        ),
        ("LINE_VERTEX_SHADER", V, s::LINE_VERTEX_SHADER),
        ("LINE_FRAGMENT_SHADER", F, s::LINE_FRAGMENT_SHADER),
    ]
}

/// Every shader constant in the X11 backend's `shaders` module (desktop GL
/// `#version 330 core`). These have diverged from the Wayland set (different
/// GLSL dialect) and must be validated against a desktop-GL core context.
fn x11_shaders() -> Vec<(&'static str, Stage, &'static str)> {
    use crate::backend::x11::compositor::shaders as s;
    use Stage::{Fragment as F, Vertex as V};
    vec![
        ("VERTEX_SHADER", V, s::VERTEX_SHADER),
        ("FRAGMENT_SHADER", F, s::FRAGMENT_SHADER),
        ("SHADOW_FRAGMENT_SHADER", F, s::SHADOW_FRAGMENT_SHADER),
        ("BLUR_DOWN_VERTEX", V, s::BLUR_DOWN_VERTEX),
        ("BLUR_DOWN_FRAGMENT", F, s::BLUR_DOWN_FRAGMENT),
        ("BLUR_UP_FRAGMENT", F, s::BLUR_UP_FRAGMENT),
        ("BOX_BLUR_FRAGMENT", F, s::BOX_BLUR_FRAGMENT),
        ("BORDER_FRAGMENT_SHADER", F, s::BORDER_FRAGMENT_SHADER),
        (
            "POSTPROCESS_FRAGMENT_SHADER",
            F,
            s::POSTPROCESS_FRAGMENT_SHADER,
        ),
        ("HUD_FRAGMENT_SHADER", F, s::HUD_FRAGMENT_SHADER),
        ("HUD_TEXT_FRAGMENT_SHADER", F, s::HUD_TEXT_FRAGMENT_SHADER),
        ("CUBE_VERTEX_SHADER", V, s::CUBE_VERTEX_SHADER),
        ("CUBE_FRAGMENT_SHADER", F, s::CUBE_FRAGMENT_SHADER),
        ("PORTAL_FRAGMENT_SHADER", F, s::PORTAL_FRAGMENT_SHADER),
        (
            "TRANSITION_FRAGMENT_SHADER",
            F,
            s::TRANSITION_FRAGMENT_SHADER,
        ),
        ("EDGE_GLOW_FRAGMENT_SHADER", F, s::EDGE_GLOW_FRAGMENT_SHADER),
        (
            "ADVANCED_POSTPROCESS_FRAGMENT_SHADER",
            F,
            s::ADVANCED_POSTPROCESS_FRAGMENT_SHADER,
        ),
        ("WATERLILY_FRAGMENT_SHADER", F, s::WATERLILY_FRAGMENT_SHADER),
        ("TILT_VERTEX_SHADER", V, s::TILT_VERTEX_SHADER),
        ("TILT_FRAGMENT_SHADER", F, s::TILT_FRAGMENT_SHADER),
        ("WOBBLY_VERTEX_SHADER", V, s::WOBBLY_VERTEX_SHADER),
        ("PARTICLE_VERTEX_SHADER", V, s::PARTICLE_VERTEX_SHADER),
        ("PARTICLE_FRAGMENT_SHADER", F, s::PARTICLE_FRAGMENT_SHADER),
        (
            "OVERVIEW_BG_FRAGMENT_SHADER",
            F,
            s::OVERVIEW_BG_FRAGMENT_SHADER,
        ),
        ("GENIE_VERTEX_SHADER", V, s::GENIE_VERTEX_SHADER),
        ("TEMPORAL_BLUR_MIX_VERTEX", V, s::TEMPORAL_BLUR_MIX_VERTEX),
        (
            "TEMPORAL_BLUR_MIX_FRAGMENT",
            F,
            s::TEMPORAL_BLUR_MIX_FRAGMENT,
        ),
    ]
}

fn assert_all_compile(api: GlApi, what: &str, shaders: Vec<(&'static str, Stage, &'static str)>) {
    let Some(h) = HeadlessGl::new(api) else {
        eprintln!("headless GL unavailable - skipping {what}");
        return;
    };
    let gl = &h.gl;

    let mut failures = Vec::new();
    for (name, stage, src) in shaders {
        if let Err(log) = compile(gl, stage, src) {
            failures.push(format!("{name}:\n{log}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{}: {} shader(s) failed to compile:\n\n{}",
        what,
        failures.len(),
        failures.join("\n---\n")
    );
}

#[test]
fn wayland_shaders_compile() {
    assert_all_compile(GlApi::Gles3, "wayland_shaders_compile", wayland_shaders());
}

#[test]
fn x11_shaders_compile() {
    assert_all_compile(GlApi::GlCore33, "x11_shaders_compile", x11_shaders());
}

#[test]
fn waterlily_shader_keys_white_to_translucent_scene_and_preserves_color() {
    use crate::backend::x11::compositor::shaders as s;

    let Some(h) = HeadlessGl::new(GlApi::GlCore33) else {
        eprintln!(
            "headless GL unavailable - skipping \
             waterlily_shader_keys_white_to_translucent_scene_and_preserves_color"
        );
        return;
    };
    let gl = &h.gl;
    const W: i32 = 16;
    const H: i32 = 16;

    unsafe {
        let prog = link(gl, s::VERTEX_SHADER, s::WATERLILY_FRAGMENT_SHADER)
            .expect("WaterLily shaders must link");
        let scene_pixels: Vec<u8> = [40u8, 80, 120, 255]
            .iter()
            .copied()
            .cycle()
            .take(4 * 4)
            .collect();
        let scene_tex = gl.create_texture().unwrap();
        gl.active_texture(glow::TEXTURE1);
        gl.bind_texture(glow::TEXTURE_2D, Some(scene_tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA as i32,
            2,
            2,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&scene_pixels)),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::NEAREST as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::NEAREST as i32,
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

        let render = |simulation| {
            render_quad(gl, prog, simulation, W, H, |gl| {
                let u = |name: &str| gl.get_uniform_location(prog, name);
                gl.uniform_4_f32(u("u_rect").as_ref(), 0.0, 0.0, W as f32, H as f32);
                gl.uniform_matrix_4_f32_slice(
                    u("u_projection").as_ref(),
                    false,
                    &ortho(W as f32, H as f32),
                );
                gl.uniform_1_i32(u("u_texture").as_ref(), 0);
                gl.uniform_1_i32(u("u_scene_texture").as_ref(), 1);
                gl.uniform_1_i32(u("u_scene_available").as_ref(), 1);
                gl.uniform_2_f32(u("u_screen_size").as_ref(), W as f32, H as f32);
                gl.uniform_1_f32(u("u_opacity").as_ref(), 1.0);
                gl.active_texture(glow::TEXTURE1);
                gl.bind_texture(glow::TEXTURE_2D, Some(scene_tex));
                gl.active_texture(glow::TEXTURE0);
            })
        };

        // Pure white becomes a 58%-opaque premultiplied sample of the blurred
        // scene, ready for the compositor's ONE/ONE_MINUS_SRC_ALPHA blend.
        assert_pixel(
            render([255, 255, 255, 255]),
            [23, 46, 70, 148],
            1,
            "WaterLily white backdrop",
        );
        // Saturated simulation details are not keyed or made translucent.
        assert_pixel(
            render([20, 80, 220, 255]),
            [20, 80, 220, 255],
            1,
            "WaterLily colored flow",
        );

        gl.delete_texture(scene_tex);
        gl.delete_program(prog);
    }
}

#[test]
fn main_window_shader_renders_opacity_and_dim() {
    let Some(h) = HeadlessGl::new(GlApi::Gles3) else {
        eprintln!("headless GL unavailable - skipping main_window_shader_renders_opacity_and_dim");
        return;
    };
    let gl = &h.gl;

    unsafe {
        let prog = link(
            gl,
            super::shaders::VERTEX_SHADER,
            super::shaders::FRAGMENT_SHADER,
        )
        .expect("main window shaders must link");

        const W: i32 = 16;
        const H: i32 = 16;

        // Input: solid 2x2 texture, RGBA (200,100,50,255).
        let texel = [200u8, 100, 50, 255];
        let mut input_pixels = Vec::with_capacity(4 * 4);
        for _ in 0..4 {
            input_pixels.extend_from_slice(&texel);
        }
        let input_tex = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_2D, Some(input_tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA as i32,
            2,
            2,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&input_pixels)),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::NEAREST as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::NEAREST as i32,
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

        // Output FBO (RGBA8, WxH).
        let out_tex = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_2D, Some(out_tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA as i32,
            W,
            H,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::NEAREST as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::NEAREST as i32,
        );
        let fbo = gl.create_framebuffer().unwrap();
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(out_tex),
            0,
        );
        assert_eq!(
            gl.check_framebuffer_status(glow::FRAMEBUFFER),
            glow::FRAMEBUFFER_COMPLETE,
            "output FBO incomplete"
        );

        gl.viewport(0, 0, W, H);
        gl.disable(glow::BLEND);
        gl.use_program(Some(prog));

        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(input_tex));

        let proj = ortho(W as f32, H as f32);
        let u = |n: &str| gl.get_uniform_location(prog, n);
        gl.uniform_4_f32(u("u_rect").as_ref(), 0.0, 0.0, W as f32, H as f32);
        gl.uniform_matrix_4_f32_slice(u("u_projection").as_ref(), false, &proj);
        gl.uniform_1_i32(u("u_texture").as_ref(), 0);
        gl.uniform_1_f32(u("u_radius").as_ref(), 0.0);
        gl.uniform_2_f32(u("u_size").as_ref(), W as f32, H as f32);
        gl.uniform_4_f32(u("u_uv_rect").as_ref(), 0.0, 0.0, 1.0, 1.0);
        gl.uniform_1_f32(u("u_ripple_progress").as_ref(), -1.0);
        gl.uniform_1_f32(u("u_ripple_amplitude").as_ref(), 0.0);

        let (vao, vbo) = create_quad_vao(gl);
        gl.bind_vertex_array(Some(vao));

        // Case 1: forced-opaque, no dim -> the texel passes through unchanged.
        gl.uniform_1_f32(u("u_opacity").as_ref(), 1.0);
        gl.uniform_1_f32(u("u_dim").as_ref(), 1.0);
        gl.clear_color(0.0, 0.0, 0.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        gl.finish();
        assert_pixel(
            read_center(gl, W, H),
            [200, 100, 50, 255],
            2,
            "opaque/no-dim",
        );

        // Case 2: dim 0.5 -> RGB halved, alpha stays opaque (u_opacity >= 0).
        gl.uniform_1_f32(u("u_dim").as_ref(), 0.5);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        gl.finish();
        assert_pixel(read_center(gl, W, H), [100, 50, 25, 255], 2, "dim-0.5");

        gl.bind_vertex_array(None);
        gl.delete_buffer(vbo);
        gl.delete_vertex_array(vao);
    }
}

/// When `u_color_managed = 1` with linear→linear EOTFs and an identity gamut
/// matrix, the per-surface color pipeline must be a no-op. This guards both
/// the "gate-on but no work to do" path and the GLSL helpers (decode_eotf /
/// encode_eotf / mat3 bind) against regressions that would tint pixels even
/// when the transform should be identity.
#[test]
fn main_window_shader_color_management_identity_is_passthrough() {
    let Some(h) = HeadlessGl::new(GlApi::Gles3) else {
        eprintln!("headless GL unavailable - skipping color_management_identity_is_passthrough");
        return;
    };
    let gl = &h.gl;

    unsafe {
        let prog = link(
            gl,
            super::shaders::VERTEX_SHADER,
            super::shaders::FRAGMENT_SHADER,
        )
        .expect("main window shaders must link");

        const W: i32 = 8;
        const H: i32 = 8;

        let texel = [180u8, 90, 30, 255];
        let mut input_pixels = Vec::with_capacity(4 * 4);
        for _ in 0..4 {
            input_pixels.extend_from_slice(&texel);
        }
        let input_tex = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_2D, Some(input_tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA as i32,
            2,
            2,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&input_pixels)),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::NEAREST as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::NEAREST as i32,
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

        let out_tex = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_2D, Some(out_tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA as i32,
            W,
            H,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::NEAREST as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::NEAREST as i32,
        );
        let fbo = gl.create_framebuffer().unwrap();
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(out_tex),
            0,
        );
        assert_eq!(
            gl.check_framebuffer_status(glow::FRAMEBUFFER),
            glow::FRAMEBUFFER_COMPLETE,
            "output FBO incomplete"
        );

        gl.viewport(0, 0, W, H);
        gl.disable(glow::BLEND);
        gl.use_program(Some(prog));
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(input_tex));

        let proj = ortho(W as f32, H as f32);
        let u = |n: &str| gl.get_uniform_location(prog, n);
        gl.uniform_4_f32(u("u_rect").as_ref(), 0.0, 0.0, W as f32, H as f32);
        gl.uniform_matrix_4_f32_slice(u("u_projection").as_ref(), false, &proj);
        gl.uniform_1_i32(u("u_texture").as_ref(), 0);
        gl.uniform_1_f32(u("u_radius").as_ref(), 0.0);
        gl.uniform_2_f32(u("u_size").as_ref(), W as f32, H as f32);
        gl.uniform_4_f32(u("u_uv_rect").as_ref(), 0.0, 0.0, 1.0, 1.0);
        gl.uniform_1_f32(u("u_ripple_progress").as_ref(), -1.0);
        gl.uniform_1_f32(u("u_ripple_amplitude").as_ref(), 0.0);
        gl.uniform_1_f32(u("u_opacity").as_ref(), 1.0);
        gl.uniform_1_f32(u("u_dim").as_ref(), 1.0);

        // Enable color management with an identity transform: linear→linear,
        // identity matrix. The fragment shader should leave the texel pixels
        // unchanged within rounding error.
        gl.uniform_1_i32(u("u_color_managed").as_ref(), 1);
        let identity = [1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        gl.uniform_matrix_3_f32_slice(u("u_color_matrix").as_ref(), false, &identity);
        gl.uniform_1_i32(u("u_decode_tf").as_ref(), 0); // Linear
        gl.uniform_1_f32(u("u_decode_gamma").as_ref(), 1.0);
        gl.uniform_1_i32(u("u_encode_tf").as_ref(), 0); // Linear
        gl.uniform_1_f32(u("u_encode_gamma").as_ref(), 1.0);

        let (vao, vbo) = create_quad_vao(gl);
        gl.bind_vertex_array(Some(vao));

        gl.clear_color(0.0, 0.0, 0.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        gl.finish();
        assert_pixel(read_center(gl, W, H), [180, 90, 30, 255], 2, "cm-identity");

        // Gate off: same shader, same texel, must still pass through.
        gl.uniform_1_i32(u("u_color_managed").as_ref(), 0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        gl.finish();
        assert_pixel(read_center(gl, W, H), [180, 90, 30, 255], 2, "cm-off");

        gl.bind_vertex_array(None);
        gl.delete_buffer(vbo);
        gl.delete_vertex_array(vao);
    }
}

/// Blurring a flat color must return that same color, whatever the kernel
/// weights are (Kawase down sums to 8, Kawase up to 12, box to 9). With a solid
/// input texture every neighbour tap is identical, so the weighted average
/// collapses to the input. This catches kernel-normalization regressions
/// (forgetting to divide by the weight total tints or darkens the result).
#[test]
fn blur_shaders_preserve_solid_color() {
    use super::shaders as s;
    let Some(h) = HeadlessGl::new(GlApi::Gles3) else {
        eprintln!("headless GL unavailable - skipping blur_shaders_preserve_solid_color");
        return;
    };
    let gl = &h.gl;

    const W: i32 = 16;
    const H: i32 = 16;
    let input = [173u8, 92, 211, 255];

    for (name, fs) in [
        ("BLUR_DOWN_FRAGMENT", s::BLUR_DOWN_FRAGMENT),
        ("BLUR_UP_FRAGMENT", s::BLUR_UP_FRAGMENT),
        ("BOX_BLUR_FRAGMENT", s::BOX_BLUR_FRAGMENT),
    ] {
        let prog = link(gl, s::BLUR_DOWN_VERTEX, fs)
            .unwrap_or_else(|log| panic!("{name} must link:\n{log}"));
        let got = render_quad(gl, prog, input, W, H, |gl| unsafe {
            let u = |n: &str| gl.get_uniform_location(prog, n);
            gl.uniform_4_f32(u("u_rect").as_ref(), 0.0, 0.0, W as f32, H as f32);
            gl.uniform_matrix_4_f32_slice(
                u("u_projection").as_ref(),
                false,
                &ortho(W as f32, H as f32),
            );
            gl.uniform_1_i32(u("u_texture").as_ref(), 0);
            gl.uniform_2_f32(u("u_halfpixel").as_ref(), 0.5 / W as f32, 0.5 / H as f32);
        });
        assert_pixel(got, input, 2, name);
        unsafe { gl.delete_program(prog) };
    }
}

/// The post-process shader must be a pass-through under neutral settings, and
/// must collapse to luminance when forced to grayscale. Guards the color-math
/// (saturation/brightness/contrast/temperature) against accidental drift.
#[test]
fn postprocess_identity_and_grayscale() {
    use super::shaders as s;
    let Some(h) = HeadlessGl::new(GlApi::Gles3) else {
        eprintln!("headless GL unavailable - skipping postprocess_identity_and_grayscale");
        return;
    };
    let gl = &h.gl;

    const W: i32 = 16;
    const H: i32 = 16;
    let input = [200u8, 100, 50, 255];

    let prog = link(gl, s::BLUR_DOWN_VERTEX, s::POSTPROCESS_FRAGMENT_SHADER)
        .unwrap_or_else(|log| panic!("postprocess must link:\n{log}"));

    let set_common = |gl: &glow::Context, grayscale: i32| unsafe {
        let u = |n: &str| gl.get_uniform_location(prog, n);
        // Fullscreen quad geometry (BLUR_DOWN_VERTEX); without these the quad
        // collapses to the origin and nothing covers the readback pixel.
        gl.uniform_4_f32(u("u_rect").as_ref(), 0.0, 0.0, W as f32, H as f32);
        gl.uniform_matrix_4_f32_slice(
            u("u_projection").as_ref(),
            false,
            &ortho(W as f32, H as f32),
        );
        gl.uniform_1_i32(u("u_texture").as_ref(), 0);
        gl.uniform_1_f32(u("u_color_temp").as_ref(), 0.0);
        gl.uniform_1_f32(u("u_saturation").as_ref(), 1.0);
        gl.uniform_1_f32(u("u_brightness").as_ref(), 1.0);
        gl.uniform_1_f32(u("u_contrast").as_ref(), 1.0);
        gl.uniform_1_i32(u("u_invert").as_ref(), 0);
        gl.uniform_1_i32(u("u_grayscale").as_ref(), grayscale);
    };

    // Identity: neutral params pass the texel through unchanged.
    let got = render_quad(gl, prog, input, W, H, |gl| set_common(gl, 0));
    assert_pixel(got, input, 2, "postprocess-identity");

    // Grayscale: rgb collapse to luminance dot(rgb, 0.2126/0.7152/0.0722).
    // For (200,100,50) that is ~118; alpha is untouched.
    let got = render_quad(gl, prog, input, W, H, |gl| set_common(gl, 1));
    assert_pixel(got, [118, 118, 118, 255], 2, "postprocess-grayscale");

    unsafe { gl.delete_program(prog) };
}
