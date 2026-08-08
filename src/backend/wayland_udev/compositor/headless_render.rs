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
    #[cfg_attr(not(feature = "x11-backends"), allow(dead_code))]
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
    let frame = render_quad_frame(gl, prog, input, w, h, uniforms);
    let centre = ((h / 2) * w + w / 2) as usize * 4;
    [
        frame[centre],
        frame[centre + 1],
        frame[centre + 2],
        frame[centre + 3],
    ]
}

/// As [`render_quad`], but returns every pixel in OpenGL's bottom-left row
/// order for tests that need to probe a shape rather than a colour.
fn render_quad_frame(
    gl: &glow::Context,
    prog: glow::Program,
    input: [u8; 4],
    w: i32,
    h: i32,
    uniforms: impl FnOnce(&glow::Context),
) -> Vec<u8> {
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
        let mut frame = vec![0u8; (w * h * 4) as usize];
        gl.read_pixels(
            0,
            0,
            w,
            h,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(&mut frame)),
        );

        gl.bind_vertex_array(None);
        gl.delete_buffer(vbo);
        gl.delete_vertex_array(vao);
        gl.delete_framebuffer(fbo);
        gl.delete_texture(out_tex);
        gl.delete_texture(input_tex);
        frame
    }
}

/// Render the wobbly grid mesh over an opaque white texel into a WxH RGBA8 FBO
/// cleared to black, and return every pixel in OpenGL's bottom-left row order.
///
/// Unlike [`render_quad`] the mesh takes no vertex attributes: the wobbly
/// vertex shader derives each node from `gl_VertexID` over `(grid_n - 1)^2`
/// quads, exactly as both compositors draw it. A pixel is therefore "covered"
/// iff a mesh triangle landed on it, which is what makes the deformation
/// measurable from a readback.
fn render_mesh(
    gl: &glow::Context,
    prog: glow::Program,
    w: i32,
    h: i32,
    grid_n: i32,
    uniforms: impl FnOnce(&glow::Context),
) -> Vec<u8> {
    unsafe {
        let input_pixels = [255u8; 16];
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
        for filter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, filter, glow::NEAREST as i32);
        }
        for wrap in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, wrap, glow::CLAMP_TO_EDGE as i32);
        }

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
        for filter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, filter, glow::NEAREST as i32);
        }
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
            "mesh output FBO incomplete"
        );

        gl.viewport(0, 0, w, h);
        gl.disable(glow::BLEND);
        gl.use_program(Some(prog));
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(input_tex));
        uniforms(gl);

        // Core profiles still require a bound VAO for attribute-less draws.
        let vao = gl.create_vertex_array().unwrap();
        gl.bind_vertex_array(Some(vao));
        gl.clear_color(0.0, 0.0, 0.0, 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        let quads = (grid_n - 1).max(1);
        gl.draw_arrays(glow::TRIANGLES, 0, quads * quads * 6);
        gl.finish();

        let mut frame = vec![0u8; (w * h * 4) as usize];
        gl.read_pixels(
            0,
            0,
            w,
            h,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(&mut frame)),
        );

        gl.bind_vertex_array(None);
        gl.delete_vertex_array(vao);
        gl.delete_framebuffer(fbo);
        gl.delete_texture(out_tex);
        gl.delete_texture(input_tex);
        frame
    }
}

/// Render the real X11 WaterLily volume shader into an RGBA8 FBO and return
/// every pixel in OpenGL's bottom-left row order.  The volume deliberately
/// uses LINEAR filtering: the production texture uses the same sampler state,
/// and the empty-space probe must agree with the wider tricubic B-spline
/// reconstruction built on top of those hardware trilinear taps.
#[cfg(feature = "x11-backends")]
fn render_waterlily_volume_frame(
    gl: &glow::Context,
    fragment_shader: &str,
    voxels: &[u8],
    dimensions: [i32; 3],
    output_size: [i32; 2],
    box_half_extents: [f32; 3],
    scene_available: bool,
) -> Vec<u8> {
    use crate::backend::x11::compositor::shaders as s;

    let [volume_w, volume_h, volume_d] = dimensions;
    let [output_w, output_h] = output_size;
    assert!(volume_w >= 1 && volume_h >= 1 && volume_d >= 1);
    assert_eq!(
        voxels.len(),
        volume_w as usize * volume_h as usize * volume_d as usize * 4
    );

    unsafe {
        let program = link(gl, s::VERTEX_SHADER, fragment_shader)
            .expect("WaterLily volume shaders must link");

        let volume = gl.create_texture().unwrap();
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_3D, Some(volume));
        let unpack_alignment = gl.get_parameter_i32(glow::UNPACK_ALIGNMENT);
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        gl.tex_image_3d(
            glow::TEXTURE_3D,
            0,
            glow::RGBA as i32,
            volume_w,
            volume_h,
            volume_d,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(voxels)),
        );
        for filter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
            gl.tex_parameter_i32(glow::TEXTURE_3D, filter, glow::LINEAR as i32);
        }
        for wrap in [
            glow::TEXTURE_WRAP_S,
            glow::TEXTURE_WRAP_T,
            glow::TEXTURE_WRAP_R,
        ] {
            gl.tex_parameter_i32(glow::TEXTURE_3D, wrap, glow::CLAMP_TO_EDGE as i32);
        }

        let mut occupancy = vec![0_u8; (volume_w * volume_h * volume_d) as usize];
        let plane = (volume_w * volume_h) as usize;
        for z in 0..volume_d as usize {
            for y in 0..volume_h as usize {
                for x in 0..volume_w as usize {
                    let source = (z * plane + y * volume_w as usize + x) * 4;
                    if voxels[source + 3] == 0 {
                        continue;
                    }
                    for target_z in z.saturating_sub(1)..=(z + 1).min(volume_d as usize - 1) {
                        for target_y in y.saturating_sub(1)..=(y + 1).min(volume_h as usize - 1) {
                            for target_x in x.saturating_sub(1)..=(x + 1).min(volume_w as usize - 1)
                            {
                                occupancy
                                    [target_z * plane + target_y * volume_w as usize + target_x] =
                                    u8::MAX;
                            }
                        }
                    }
                }
            }
        }
        let occupancy_texture = gl.create_texture().unwrap();
        gl.active_texture(glow::TEXTURE2);
        gl.bind_texture(glow::TEXTURE_3D, Some(occupancy_texture));
        gl.tex_image_3d(
            glow::TEXTURE_3D,
            0,
            glow::R8 as i32,
            volume_w,
            volume_h,
            volume_d,
            0,
            glow::RED,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&occupancy)),
        );
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, unpack_alignment);
        gl.tex_parameter_i32(glow::TEXTURE_3D, glow::TEXTURE_SWIZZLE_A, glow::RED as i32);
        for filter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
            gl.tex_parameter_i32(glow::TEXTURE_3D, filter, glow::LINEAR as i32);
        }
        for wrap in [
            glow::TEXTURE_WRAP_S,
            glow::TEXTURE_WRAP_T,
            glow::TEXTURE_WRAP_R,
        ] {
            gl.tex_parameter_i32(glow::TEXTURE_3D, wrap, glow::CLAMP_TO_EDGE as i32);
        }

        // A low-frequency two-axis gradient makes refraction observable
        // without importing high-frequency checker noise into the firefly
        // metric. Bind a valid 1x1 fallback even when the scene path is off:
        // some drivers validate every active sampler across dynamic branches.
        let scene_side = if scene_available { 32 } else { 1 };
        let mut scene_pixels = Vec::with_capacity(scene_side * scene_side * 4);
        for y in 0..scene_side {
            for x in 0..scene_side {
                scene_pixels.extend_from_slice(&[
                    30 + (5 * x) as u8,
                    42 + (4 * y) as u8,
                    70 + (2 * (x + y)) as u8,
                    255,
                ]);
            }
        }
        let scene = gl.create_texture().unwrap();
        gl.active_texture(glow::TEXTURE1);
        gl.bind_texture(glow::TEXTURE_2D, Some(scene));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA as i32,
            scene_side as i32,
            scene_side as i32,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&scene_pixels)),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            if scene_available {
                glow::LINEAR_MIPMAP_LINEAR as i32
            } else {
                glow::NEAREST as i32
            },
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            if scene_available {
                glow::LINEAR as i32
            } else {
                glow::NEAREST as i32
            },
        );
        for wrap in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, wrap, glow::CLAMP_TO_EDGE as i32);
        }
        if scene_available {
            gl.generate_mipmap(glow::TEXTURE_2D);
        }

        let output = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_2D, Some(output));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA as i32,
            output_w,
            output_h,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        let framebuffer = gl.create_framebuffer().unwrap();
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(framebuffer));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(output),
            0,
        );
        assert_eq!(
            gl.check_framebuffer_status(glow::FRAMEBUFFER),
            glow::FRAMEBUFFER_COMPLETE,
            "WaterLily volume output FBO incomplete"
        );

        gl.viewport(0, 0, output_w, output_h);
        let blend_was_enabled = gl.is_enabled(glow::BLEND);
        let dither_was_enabled = gl.is_enabled(glow::DITHER);
        if blend_was_enabled {
            gl.disable(glow::BLEND);
        }
        if dither_was_enabled {
            gl.disable(glow::DITHER);
        }
        gl.use_program(Some(program));
        let uniform = |name: &str| gl.get_uniform_location(program, name);
        gl.uniform_4_f32(
            uniform("u_rect").as_ref(),
            0.0,
            0.0,
            output_w as f32,
            output_h as f32,
        );
        gl.uniform_matrix_4_f32_slice(
            uniform("u_projection").as_ref(),
            false,
            &ortho(output_w as f32, output_h as f32),
        );
        gl.uniform_1_i32(uniform("u_volume").as_ref(), 0);
        gl.uniform_1_i32(uniform("u_occupancy").as_ref(), 2);
        gl.uniform_1_i32(uniform("u_scene_texture").as_ref(), 1);
        gl.uniform_1_i32(
            uniform("u_scene_available").as_ref(),
            i32::from(scene_available),
        );
        gl.uniform_2_f32(
            uniform("u_screen_size").as_ref(),
            output_w as f32,
            output_h as f32,
        );
        gl.uniform_1_f32(uniform("u_opacity").as_ref(), 1.0);
        gl.uniform_3_f32(uniform("u_camera_position").as_ref(), 0.0, 0.0, -3.0);
        gl.uniform_3_f32(uniform("u_camera_right").as_ref(), 1.0, 0.0, 0.0);
        gl.uniform_3_f32(uniform("u_camera_up").as_ref(), 0.0, 1.0, 0.0);
        gl.uniform_3_f32(uniform("u_camera_forward").as_ref(), 0.0, 0.0, 1.0);
        gl.uniform_1_f32(uniform("u_tan_half_fov").as_ref(), 0.35);
        gl.uniform_3_f32(
            uniform("u_box_half_extents").as_ref(),
            box_half_extents[0],
            box_half_extents[1],
            box_half_extents[2],
        );
        gl.uniform_1_f32(uniform("u_time").as_ref(), 0.0);

        gl.active_texture(glow::TEXTURE1);
        gl.bind_texture(glow::TEXTURE_2D, Some(scene));
        gl.active_texture(glow::TEXTURE2);
        gl.bind_texture(glow::TEXTURE_3D, Some(occupancy_texture));
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_3D, Some(volume));
        let (vao, vbo) = create_quad_vao(gl);
        gl.bind_vertex_array(Some(vao));
        gl.clear_color(0.0, 0.0, 0.0, 0.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        gl.finish();

        let mut pixels = vec![0_u8; output_w as usize * output_h as usize * 4];
        gl.read_pixels(
            0,
            0,
            output_w,
            output_h,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(&mut pixels)),
        );

        gl.bind_vertex_array(None);
        gl.delete_buffer(vbo);
        gl.delete_vertex_array(vao);
        gl.delete_framebuffer(framebuffer);
        gl.delete_texture(output);
        gl.delete_texture(scene);
        gl.delete_texture(occupancy_texture);
        gl.delete_texture(volume);
        gl.delete_program(program);
        if blend_was_enabled {
            gl.enable(glow::BLEND);
        }
        if dither_was_enabled {
            gl.enable(glow::DITHER);
        }
        pixels
    }
}

/// Every shader used by the Wayland backend, tagged with its pipeline stage.
/// Most live in `shaders`; purpose-specific programs such as the minimized
/// thumbnail downsampler are listed explicitly as well. A missing entry simply
/// is not compile-checked.
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
            "GRADIENT_BORDER_FRAGMENT_SHADER",
            F,
            s::GRADIENT_BORDER_FRAGMENT_SHADER,
        ),
        (
            "POSTPROCESS_FRAGMENT_SHADER",
            F,
            s::POSTPROCESS_FRAGMENT_SHADER,
        ),
        ("GLASS_FRAGMENT_SHADER", F, s::GLASS_FRAGMENT_SHADER),
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
        (
            "MINIMIZED_THUMBNAIL_VERTEX_SHADER",
            V,
            super::minimized_thumbnail::THUMBNAIL_DOWNSAMPLE_VERTEX_SHADER,
        ),
        (
            "MINIMIZED_THUMBNAIL_FRAGMENT_SHADER",
            F,
            crate::backend::compositor_common::minimized_thumbnail::THUMBNAIL_DOWNSAMPLE_FRAGMENT_SHADER,
        ),
    ]
}

/// Every shader constant in the X11 backend's `shaders` module (desktop GL
/// `#version 330 core`). These have diverged from the Wayland set (different
/// GLSL dialect) and must be validated against a desktop-GL core context.
#[cfg(feature = "x11-backends")]
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
            "GRADIENT_BORDER_FRAGMENT_SHADER",
            F,
            s::GRADIENT_BORDER_FRAGMENT_SHADER,
        ),
        (
            "POSTPROCESS_FRAGMENT_SHADER",
            F,
            s::POSTPROCESS_FRAGMENT_SHADER,
        ),
        ("GLASS_FRAGMENT_SHADER", F, s::GLASS_FRAGMENT_SHADER),
        ("HUD_FRAGMENT_SHADER", F, s::HUD_FRAGMENT_SHADER),
        ("HUD_TEXT_FRAGMENT_SHADER", F, s::HUD_TEXT_FRAGMENT_SHADER),
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
        (
            "WATERLILY_VOLUME_FRAGMENT_SHADER",
            F,
            s::WATERLILY_VOLUME_FRAGMENT_SHADER,
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
        (
            "OVERVIEW_FACE_VERTEX_SHADER",
            V,
            s::OVERVIEW_FACE_VERTEX_SHADER,
        ),
        (
            "OVERVIEW_FACE_FRAGMENT_SHADER",
            F,
            s::OVERVIEW_FACE_FRAGMENT_SHADER,
        ),
        (
            "OVERVIEW_CAP_VERTEX_SHADER",
            V,
            s::OVERVIEW_CAP_VERTEX_SHADER,
        ),
        (
            "OVERVIEW_CAP_FRAGMENT_SHADER",
            F,
            s::OVERVIEW_CAP_FRAGMENT_SHADER,
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

fn assert_all_compile<N: AsRef<str>, S: AsRef<str>>(
    api: GlApi,
    what: &str,
    shaders: Vec<(N, Stage, S)>,
) {
    let Some(h) = HeadlessGl::new(api) else {
        eprintln!("headless GL unavailable - skipping {what}");
        return;
    };
    let gl = &h.gl;

    let mut failures = Vec::new();
    for (name, stage, src) in shaders {
        if let Err(log) = compile(gl, stage, src.as_ref()) {
            failures.push(format!("{}:\n{log}", name.as_ref()));
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
fn wayland_minimized_thumbnail_program_links() {
    let Some(h) = HeadlessGl::new(GlApi::Gles3) else {
        eprintln!("headless GL unavailable - skipping minimized thumbnail link test");
        return;
    };
    let program = link(
        &h.gl,
        super::minimized_thumbnail::THUMBNAIL_DOWNSAMPLE_VERTEX_SHADER,
        crate::backend::compositor_common::minimized_thumbnail::THUMBNAIL_DOWNSAMPLE_FRAGMENT_SHADER,
    )
    .unwrap_or_else(|error| panic!("minimized thumbnail shader failed to link: {error}"));
    unsafe { h.gl.delete_program(program) };
}

fn assert_constructor_probe_names_are_deleted(
    gl: &smithay::backend::renderer::gles::ffi::Gles2,
    probe: &super::CompositorConstructionProbe,
) {
    unsafe {
        for &program in &probe.programs {
            assert_eq!(
                gl.IsProgram(program),
                0,
                "constructor program {program} survived rollback"
            );
        }
        for &vertex_array in &probe.vertex_arrays {
            assert_eq!(
                gl.IsVertexArray(vertex_array),
                0,
                "constructor VAO {vertex_array} survived rollback"
            );
        }
        for &buffer in &probe.buffers {
            assert_eq!(
                gl.IsBuffer(buffer),
                0,
                "constructor buffer {buffer} survived rollback"
            );
        }
        for &framebuffer in &probe.framebuffers {
            assert_eq!(
                gl.IsFramebuffer(framebuffer),
                0,
                "constructor framebuffer {framebuffer} survived rollback"
            );
        }
        for &texture in &probe.textures {
            assert_eq!(
                gl.IsTexture(texture),
                0,
                "constructor texture {texture} survived rollback"
            );
        }

        for (binding, label) in [
            (
                smithay::backend::renderer::gles::ffi::CURRENT_PROGRAM,
                "program",
            ),
            (
                smithay::backend::renderer::gles::ffi::VERTEX_ARRAY_BINDING,
                "vertex array",
            ),
            (
                smithay::backend::renderer::gles::ffi::ARRAY_BUFFER_BINDING,
                "array buffer",
            ),
            (
                smithay::backend::renderer::gles::ffi::FRAMEBUFFER_BINDING,
                "framebuffer",
            ),
            (
                smithay::backend::renderer::gles::ffi::TEXTURE_BINDING_2D,
                "2D texture",
            ),
        ] {
            let mut value = -1;
            gl.GetIntegerv(binding, &mut value);
            assert_eq!(value, 0, "constructor rollback left {label} bound");
        }
    }
}

unsafe fn expect_constructor_error(
    gl: &smithay::backend::renderer::gles::ffi::Gles2,
    result: Result<super::WaylandCompositor, String>,
    message: &str,
) -> String {
    match result {
        Err(error) => error,
        Ok(mut compositor) => {
            unsafe {
                compositor.release_gpu_resources(
                    gl,
                    super::CompositorOutputTextureOwnership::RawCompositor,
                );
            }
            panic!("{message}");
        }
    }
}

#[test]
fn wayland_constructor_rolls_back_every_raw_gpu_name_on_failure() {
    let Some(_headless) = HeadlessGl::new(GlApi::Gles3) else {
        eprintln!("headless GL unavailable - skipping constructor rollback test");
        return;
    };
    let gl = smithay::backend::renderer::gles::ffi::Gles2::load_with(|symbol| {
        egl::get_proc_address(symbol) as *const c_void
    });

    unsafe {
        // A synthetic shader-stage failure exercises the same `?` exits as a
        // real compile/link error and includes the independently-owned
        // thumbnail program in the rollback inventory.
        let mut program_failure = super::CompositorConstructionProbe {
            fail_before_program_count: Some(12),
            ..Default::default()
        };
        let error = expect_constructor_error(
            &gl,
            super::WaylandCompositor::new_inner(&gl, 64, 48, false, Some(&mut program_failure)),
            "injected program construction failure must propagate",
        );
        assert!(error.contains("before program 12"));
        assert_eq!(program_failure.programs.len(), 12);
        assert!(program_failure.vertex_arrays.is_empty());
        assert_constructor_probe_names_are_deleted(&gl, &program_failure);

        // Fail after several complete framebuffer pairs.  The failing helper
        // owns and self-cleans its incomplete pair; the guard must clean every
        // earlier program, VAO/VBO, FBO and texture.
        let mut framebuffer_failure = super::CompositorConstructionProbe {
            fail_before_framebuffer_count: Some(4),
            ..Default::default()
        };
        let error = expect_constructor_error(
            &gl,
            super::WaylandCompositor::new_inner(&gl, 64, 48, false, Some(&mut framebuffer_failure)),
            "injected framebuffer construction failure must propagate",
        );
        assert!(error.contains("framebuffer"));
        assert_eq!(framebuffer_failure.programs.len(), 24);
        assert_eq!(framebuffer_failure.vertex_arrays.len(), 1);
        assert_eq!(framebuffer_failure.buffers.len(), 1);
        assert_eq!(framebuffer_failure.framebuffers.len(), 4);
        assert_eq!(framebuffer_failure.textures.len(), 4);
        assert_constructor_probe_names_are_deleted(&gl, &framebuffer_failure);

        // The final injection happens after the complete Self value exists,
        // proving the guard remains armed through particle resources and all
        // aggregate field initialization until the explicit commit.
        let mut commit_failure = super::CompositorConstructionProbe {
            fail_before_commit: true,
            ..Default::default()
        };
        let error = expect_constructor_error(
            &gl,
            super::WaylandCompositor::new_inner(&gl, 64, 48, false, Some(&mut commit_failure)),
            "injected pre-commit failure must propagate",
        );
        assert!(error.contains("before commit"));
        assert_eq!(commit_failure.programs.len(), 24);
        assert_eq!(commit_failure.vertex_arrays.len(), 2);
        assert_eq!(commit_failure.buffers.len(), 2);
        assert!(commit_failure.framebuffers.len() >= 10);
        assert_eq!(
            commit_failure.framebuffers.len(),
            commit_failure.textures.len()
        );
        assert_constructor_probe_names_are_deleted(&gl, &commit_failure);
    }
}

#[test]
fn wayland_runtime_gpu_release_is_complete_idempotent_and_recreatable() {
    let Some(_headless) = HeadlessGl::new(GlApi::Gles3) else {
        eprintln!("headless GL unavailable - skipping runtime GPU release test");
        return;
    };
    let gl = smithay::backend::renderer::gles::ffi::Gles2::load_with(|symbol| {
        egl::get_proc_address(symbol) as *const c_void
    });

    unsafe {
        let mut compositor = super::WaylandCompositor::new(&gl, 64, 48, false)
            .expect("headless Wayland compositor must initialize");
        let genie_program = compositor.genie_program;
        let line_program = compositor.line_program;
        let thumbnail_program = compositor
            .minimized_thumbnails
            .downsample_program_for_tests();
        let output_fbo = compositor.output_fbo;
        let output_texture = compositor.output_texture;
        let quad_vao = compositor.quad_vao;
        let quad_vbo = compositor.quad_vbo;

        let allocate_texture = || {
            let mut texture = 0;
            gl.GenTextures(1, &mut texture);
            gl.BindTexture(smithay::backend::renderer::gles::ffi::TEXTURE_2D, texture);
            gl.TexImage2D(
                smithay::backend::renderer::gles::ffi::TEXTURE_2D,
                0,
                smithay::backend::renderer::gles::ffi::RGBA8 as i32,
                1,
                1,
                0,
                smithay::backend::renderer::gles::ffi::RGBA,
                smithay::backend::renderer::gles::ffi::UNSIGNED_BYTE,
                [0u8; 4].as_ptr().cast(),
            );
            texture
        };
        let cached_textures = (0..12).map(|_| allocate_texture()).collect::<Vec<_>>();
        compositor.overview_title_textures = vec![cached_textures[0]];
        compositor.tab_title_textures = vec![vec![Some((cached_textures[1], 1, 1))]];
        compositor.annotation_label_textures = vec![Some((cached_textures[2], 1, 1))];
        compositor.screenshot_toolbar_icons = vec![Some((cached_textures[3], 1, 1))];
        compositor.hud_textures[0] = Some((cached_textures[4], 1, 1));
        compositor.sysui_textures[0] = Some((cached_textures[5], 1, 1));
        compositor
            .toast_textures
            .insert(1, [Some((cached_textures[6], 1, 1)), None]);
        compositor.osd_texture = Some(("test".into(), cached_textures[7], 1, 1));
        compositor.wallpaper_texture = Some(cached_textures[8]);
        compositor.old_wallpaper_texture = Some(cached_textures[9]);
        compositor.monitor_wallpapers.push(super::MonitorWallpaper {
            mon_x: 0,
            mon_y: 0,
            mon_w: 1,
            mon_h: 1,
            texture: Some(cached_textures[10]),
            mode: crate::backend::compositor_common::wallpaper::WallpaperMode::Fill,
            img_w: 1,
            img_h: 1,
            current_path: String::new(),
        });
        compositor
            .retired_wallpaper_textures
            .push(cached_textures[11]);

        let (previous_blur_fbo, previous_blur_texture) = super::create_fbo_texture(&gl, 2, 2);
        let (temporal_mix_fbo, temporal_mix_texture) = super::create_fbo_texture(&gl, 2, 2);
        compositor.prev_blur_fbo = Some((previous_blur_fbo, previous_blur_texture));
        compositor.temporal_mix_fbo = Some((temporal_mix_fbo, temporal_mix_texture));
        gl.GenFramebuffers(1, &mut compositor.blur_blit_src_fbo);
        let blur_blit_src_fbo = compositor.blur_blit_src_fbo;

        let pooled_texture = compositor.texture_pool.acquire(&gl, 2, 2);
        assert_eq!(compositor.texture_pool.in_use_count(), 1);
        assert!(compositor.pbo_uploader.upload_texture(
            &gl,
            output_texture,
            1,
            1,
            smithay::backend::renderer::gles::ffi::RGBA,
            &[0, 0, 0, 0],
        ));
        compositor.gpu_fence_sync_mgr.register_fence(&gl, 7);
        gl.BindFramebuffer(
            smithay::backend::renderer::gles::ffi::FRAMEBUFFER,
            output_fbo,
        );
        compositor.screenshot_readback.enqueue(
            &gl,
            std::path::PathBuf::from("/tmp/jwm-cancelled-headless-readback.png"),
            0,
            0,
            1,
            1,
        );
        compositor
            .recording
            .seed_inactive_gpu_resources_for_tests(&gl);
        let (recording_pbos, recording_fbo, recording_texture) =
            compositor.recording.gpu_resources_for_tests();

        assert_ne!(gl.IsProgram(genie_program), 0);
        assert_ne!(gl.IsProgram(line_program), 0);
        assert_ne!(gl.IsProgram(thumbnail_program), 0);
        assert_ne!(gl.IsFramebuffer(output_fbo), 0);
        assert_ne!(gl.IsTexture(output_texture), 0);
        assert_ne!(gl.IsVertexArray(quad_vao), 0);
        assert_ne!(gl.IsBuffer(quad_vbo), 0);

        assert!(compositor.release_gpu_resources(
            &gl,
            super::CompositorOutputTextureOwnership::RawCompositor,
        ));
        assert_eq!(gl.IsProgram(genie_program), 0);
        assert_eq!(gl.IsProgram(line_program), 0);
        assert_eq!(gl.IsProgram(thumbnail_program), 0);
        assert_eq!(gl.IsFramebuffer(output_fbo), 0);
        assert_eq!(gl.IsTexture(output_texture), 0);
        assert_eq!(gl.IsVertexArray(quad_vao), 0);
        assert_eq!(gl.IsBuffer(quad_vbo), 0);
        for texture in cached_textures.iter().copied().chain([
            pooled_texture,
            previous_blur_texture,
            temporal_mix_texture,
            recording_texture,
        ]) {
            assert_eq!(
                gl.IsTexture(texture),
                0,
                "texture {texture} survived teardown"
            );
        }
        for framebuffer in [
            previous_blur_fbo,
            temporal_mix_fbo,
            blur_blit_src_fbo,
            recording_fbo,
        ] {
            assert_eq!(
                gl.IsFramebuffer(framebuffer),
                0,
                "framebuffer {framebuffer} survived teardown"
            );
        }
        for buffer in recording_pbos {
            assert_eq!(
                gl.IsBuffer(buffer),
                0,
                "recording PBO {buffer} survived teardown"
            );
        }
        assert!(!compositor.screenshot_readback.has_pending());
        assert_eq!(compositor.gpu_fence_sync_mgr.stats().3, 0);
        assert_eq!(compositor.pbo_uploader.stats().2, 0);
        assert_eq!(compositor.texture_pool.available_count(), 0);
        assert_eq!(compositor.texture_pool.in_use_count(), 0);
        assert_eq!(
            compositor.recording.gpu_resources_for_tests(),
            ([0; 2], 0, 0)
        );
        assert!(!compositor.release_gpu_resources(
            &gl,
            super::CompositorOutputTextureOwnership::RawCompositor,
        ));

        // A renderer-owned output is the sole texture exception: the
        // compositor removes its FBO/raw alias, while Smithay performs the
        // one and only texture delete after its GlesTexture wrapper retires.
        let mut renderer_owned = super::WaylandCompositor::new(&gl, 32, 24, false)
            .expect("same-context compositor recreation must initialize");
        let renderer_owned_output = renderer_owned.output_texture;
        assert!(renderer_owned.release_gpu_resources(
            &gl,
            super::CompositorOutputTextureOwnership::SmithayRenderer,
        ));
        assert_ne!(gl.IsTexture(renderer_owned_output), 0);
        gl.DeleteTextures(1, &renderer_owned_output);
        assert_eq!(gl.IsTexture(renderer_owned_output), 0);

        let mut recreated = super::WaylandCompositor::new(&gl, 16, 16, false)
            .expect("release must leave the same EGL context reusable");
        assert!(recreated.release_gpu_resources(
            &gl,
            super::CompositorOutputTextureOwnership::RawCompositor,
        ));
    }
}

#[cfg(feature = "x11-backends")]
#[test]
fn x11_shaders_compile() {
    assert_all_compile(GlApi::GlCore33, "x11_shaders_compile", x11_shaders());
}

/// The X11 compositor does not always get a desktop-GL context: on an EGL
/// platform it reports `graphics API=egl/gles3` and `ShaderCache` rewrites the
/// `#version 330 core` sources into ESSL 3.00 on the fly. ESSL is the stricter
/// dialect (mandatory precision qualifiers, no implicit int/float conversions),
/// so `x11_shaders_compile` passing says nothing about that path — compile the
/// rewritten form the GLES path actually feeds the driver. A failure here takes
/// the whole compositor down, not just the offending effect, because
/// `Compositor::new` compiles every program up front.
#[cfg(feature = "x11-backends")]
#[test]
fn x11_shaders_compile_as_gles() {
    use crate::backend::x11::compositor::ShaderCache;

    let rewritten: Vec<(&'static str, Stage, String)> = x11_shaders()
        .into_iter()
        .map(|(name, stage, src)| {
            (
                name,
                stage,
                ShaderCache::prepare_source(src, true).into_owned(),
            )
        })
        .collect();
    assert_all_compile(GlApi::Gles3, "x11_shaders_compile_as_gles", rewritten);
}

/// When an effect's shader will not compile, `Compositor::optional_program`
/// keeps the compositor alive by binding a stand-in program in its place. Draw
/// sites are unaware of the substitution — they bind it and issue their draw
/// exactly as before — so the stand-in has to rasterise nothing at all, or a
/// disabled effect would paint garbage over the desktop instead of vanishing.
/// Check that on both context flavours the X11 compositor can be handed, since
/// the ESSL rewrite applies to the stand-in too.
#[cfg(feature = "x11-backends")]
#[test]
fn disabled_effect_stand_in_draws_nothing() {
    use crate::backend::x11::compositor::ShaderCache;
    use crate::backend::x11::compositor::init::{DISABLED_EFFECT_FRAGMENT, DISABLED_EFFECT_VERTEX};

    for (api, label) in [(GlApi::GlCore33, "gl33"), (GlApi::Gles3, "gles3")] {
        let Some(h) = HeadlessGl::new(api) else {
            eprintln!("headless GL unavailable - skipping disabled_effect_stand_in ({label})");
            continue;
        };
        let is_gles = matches!(api, GlApi::Gles3);
        let vs = ShaderCache::prepare_source(DISABLED_EFFECT_VERTEX, is_gles).into_owned();
        let fs = ShaderCache::prepare_source(DISABLED_EFFECT_FRAGMENT, is_gles).into_owned();
        let prog = link(&h.gl, &vs, &fs)
            .unwrap_or_else(|e| panic!("stand-in program failed to build on {label}: {e}"));

        // Opaque red input, so a stand-in that did rasterise would be obvious
        // against the black clear `render_quad_frame` starts from.
        let frame = render_quad_frame(&h.gl, prog, [255, 0, 0, 255], 8, 8, |_| {});
        let touched = frame
            .chunks_exact(4)
            .filter(|p| *p != [0, 0, 0, 255])
            .count();
        assert_eq!(
            touched, 0,
            "stand-in program shaded {touched} pixel(s) on {label}; it must draw nothing"
        );
    }
}

#[cfg(feature = "x11-backends")]
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
        // A translucent pixel is a water lens: on a flat alpha field the
        // refraction offset is zero, so the sharp scene shows through tinted
        // by the producer color at its alpha, and the output is opaque.
        assert_pixel(
            render([100, 120, 140, 60]),
            [54, 89, 125, 255],
            2,
            "WaterLily water lens",
        );

        gl.delete_texture(scene_tex);
        gl.delete_program(prog);
    }
}

/// The volumetric WaterLily ray-marcher must composit front to back: a red
/// front slice fully occludes a green back slice from the resting camera,
/// and orbiting to the opposite side reverses which slice wins. This pins
/// the protocol's front-to-back slice order, the camera basis convention,
/// and the emission/absorption accumulation in one observable behavior.
#[cfg(feature = "x11-backends")]
#[test]
fn waterlily_volume_shader_occludes_front_to_back() {
    use crate::backend::x11::compositor::shaders as s;

    let Some(h) = HeadlessGl::new(GlApi::GlCore33) else {
        eprintln!(
            "headless GL unavailable - skipping waterlily_volume_shader_occludes_front_to_back"
        );
        return;
    };
    let gl = &h.gl;
    const W: i32 = 16;
    const H: i32 = 16;

    unsafe {
        let prog = link(gl, s::VERTEX_SHADER, s::WATERLILY_VOLUME_FRAGMENT_SHADER)
            .expect("WaterLily volume shaders must link");

        // 1x1x4 volume: two opaque red front slices, two opaque green back
        // slices. Two slices per color keep the pinned occlusion behavior
        // independent of the reconstruction kernel's support so the first
        // material sample lands in a pure-color region.
        let voxels: [u8; 16] = [
            255, 0, 0, 255, 255, 0, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255,
        ];
        let volume = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_3D, Some(volume));
        gl.tex_image_3d(
            glow::TEXTURE_3D,
            0,
            glow::RGBA as i32,
            1,
            1,
            4,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&voxels)),
        );
        for filter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
            gl.tex_parameter_i32(glow::TEXTURE_3D, filter, glow::NEAREST as i32);
        }
        for wrap in [
            glow::TEXTURE_WRAP_S,
            glow::TEXTURE_WRAP_T,
            glow::TEXTURE_WRAP_R,
        ] {
            gl.tex_parameter_i32(glow::TEXTURE_3D, wrap, glow::CLAMP_TO_EDGE as i32);
        }

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
            "volume output FBO incomplete"
        );

        let (vao, vbo) = create_quad_vao(gl);
        let render_from = |position: [f32; 3], forward: [f32; 3], right: [f32; 3]| -> [u8; 4] {
            gl.viewport(0, 0, W, H);
            gl.disable(glow::BLEND);
            gl.use_program(Some(prog));
            let u = |name: &str| gl.get_uniform_location(prog, name);
            gl.uniform_4_f32(u("u_rect").as_ref(), 0.0, 0.0, W as f32, H as f32);
            gl.uniform_matrix_4_f32_slice(
                u("u_projection").as_ref(),
                false,
                &ortho(W as f32, H as f32),
            );
            gl.uniform_1_i32(u("u_volume").as_ref(), 0);
            // Unit 1 even when unused: sharing unit 0 with the sampler3D
            // would make the program invalid to draw with.
            gl.uniform_1_i32(u("u_scene_texture").as_ref(), 1);
            gl.uniform_1_i32(u("u_scene_available").as_ref(), 0);
            gl.uniform_2_f32(u("u_screen_size").as_ref(), W as f32, H as f32);
            gl.uniform_1_f32(u("u_opacity").as_ref(), 1.0);
            gl.uniform_3_f32(
                u("u_camera_position").as_ref(),
                position[0],
                position[1],
                position[2],
            );
            gl.uniform_3_f32(u("u_camera_right").as_ref(), right[0], right[1], right[2]);
            gl.uniform_3_f32(u("u_camera_up").as_ref(), 0.0, 1.0, 0.0);
            gl.uniform_3_f32(
                u("u_camera_forward").as_ref(),
                forward[0],
                forward[1],
                forward[2],
            );
            gl.uniform_1_f32(u("u_tan_half_fov").as_ref(), 0.35);
            gl.uniform_3_f32(u("u_box_half_extents").as_ref(), 0.5, 0.5, 0.5);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_3D, Some(volume));
            gl.bind_vertex_array(Some(vao));
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.finish();
            read_center(gl, W, H)
        };

        // Resting camera on the front side: the red front slice owns the
        // pixel; the green back slice is completely occluded.
        let front_view = render_from([0.0, 0.0, -3.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]);
        assert!(
            front_view[0] > 200
                && u16::from(front_view[0]) > u16::from(front_view[1]) + 40
                && u16::from(front_view[0]) > u16::from(front_view[2]) + 30
                && front_view[3] == 255,
            "front view must be red-dominant and opaque, got {front_view:?}"
        );
        let mut outside = [0_u8; 4];
        gl.read_pixels(
            0,
            0,
            1,
            1,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelPackData::Slice(Some(&mut outside)),
        );
        assert_eq!(
            outside,
            [0, 0, 0, 0],
            "a ray outside the projected aquarium must remain transparent"
        );

        // Orbited behind the tank: the same volume now leads with green.
        // The key light carries a front bias, so the back view is legally
        // dimmer than the front one; the pinned contract is the occlusion
        // order and the surviving hue, not matched brightness.
        let back_view = render_from([0.0, 0.0, 3.0], [0.0, 0.0, -1.0], [-1.0, 0.0, 0.0]);
        assert!(
            back_view[1] > 120
                && u16::from(back_view[1]) > u16::from(back_view[0]) + 40
                && u16::from(back_view[1]) > u16::from(back_view[2]) + 30
                && back_view[3] == 255,
            "back view must be green-dominant and opaque, got {back_view:?}"
        );

        gl.bind_vertex_array(None);
        gl.delete_buffer(vbo);
        gl.delete_vertex_array(vao);
        gl.delete_framebuffer(fbo);
        gl.delete_texture(out_tex);
        gl.delete_texture(volume);
        gl.delete_program(prog);
    }
}

/// A translucent low-alpha wake voxel must keep its authored palette hue on
/// screen. This guards the volumetric transfer/lighting redesign: the old
/// flat gray emission floor lifted every wake wisp to the same near-white,
/// which read as cotton-wool fog around the jellyfish; the floor is now
/// proportional to the voxel's own albedo, so a green vortex ring stays
/// visibly green and clearly translucent.
#[cfg(feature = "x11-backends")]
#[test]
fn waterlily_volume_shader_preserves_wake_hue() {
    use crate::backend::x11::compositor::shaders as s;

    let Some(h) = HeadlessGl::new(GlApi::GlCore33) else {
        eprintln!("headless GL unavailable - skipping waterlily_volume_shader_preserves_wake_hue");
        return;
    };
    let gl = &h.gl;
    const W: i32 = 16;
    const H: i32 = 16;

    unsafe {
        let prog = link(gl, s::VERTEX_SHADER, s::WATERLILY_VOLUME_FRAGMENT_SHADER)
            .expect("WaterLily volume shaders must link");

        // A uniform green wake medium in the producer's low-alpha band.
        let voxel: [u8; 4] = [60, 220, 80, 26];
        let voxels: Vec<u8> = voxel.iter().copied().cycle().take(4 * 4).collect();
        let volume = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_3D, Some(volume));
        gl.tex_image_3d(
            glow::TEXTURE_3D,
            0,
            glow::RGBA as i32,
            1,
            1,
            4,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&voxels)),
        );
        for filter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
            gl.tex_parameter_i32(glow::TEXTURE_3D, filter, glow::NEAREST as i32);
        }
        for wrap in [
            glow::TEXTURE_WRAP_S,
            glow::TEXTURE_WRAP_T,
            glow::TEXTURE_WRAP_R,
        ] {
            gl.tex_parameter_i32(glow::TEXTURE_3D, wrap, glow::CLAMP_TO_EDGE as i32);
        }

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
            "wake hue output FBO incomplete"
        );

        let (vao, vbo) = create_quad_vao(gl);
        gl.viewport(0, 0, W, H);
        gl.disable(glow::BLEND);
        gl.use_program(Some(prog));
        let u = |name: &str| gl.get_uniform_location(prog, name);
        gl.uniform_4_f32(u("u_rect").as_ref(), 0.0, 0.0, W as f32, H as f32);
        gl.uniform_matrix_4_f32_slice(
            u("u_projection").as_ref(),
            false,
            &ortho(W as f32, H as f32),
        );
        gl.uniform_1_i32(u("u_volume").as_ref(), 0);
        gl.uniform_1_i32(u("u_scene_texture").as_ref(), 1);
        gl.uniform_1_i32(u("u_scene_available").as_ref(), 0);
        gl.uniform_2_f32(u("u_screen_size").as_ref(), W as f32, H as f32);
        gl.uniform_1_f32(u("u_opacity").as_ref(), 1.0);
        gl.uniform_3_f32(u("u_camera_position").as_ref(), 0.0, 0.0, -3.0);
        gl.uniform_3_f32(u("u_camera_right").as_ref(), 1.0, 0.0, 0.0);
        gl.uniform_3_f32(u("u_camera_up").as_ref(), 0.0, 1.0, 0.0);
        gl.uniform_3_f32(u("u_camera_forward").as_ref(), 0.0, 0.0, 1.0);
        gl.uniform_1_f32(u("u_tan_half_fov").as_ref(), 0.35);
        gl.uniform_3_f32(u("u_box_half_extents").as_ref(), 0.5, 0.5, 0.5);
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_3D, Some(volume));
        gl.bind_vertex_array(Some(vao));
        gl.clear_color(0.0, 0.0, 0.0, 0.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        gl.finish();
        let wake = read_center(gl, W, H);

        assert!(
            wake[1] > 18,
            "the wake medium must remain visible, got {wake:?}"
        );
        assert!(
            wake[1] >= wake[0] + 8 && wake[1] >= wake[2] + 4,
            "the wake must keep its green hue instead of washing to gray, got {wake:?}"
        );
        assert!(
            wake[3] > 30 && wake[3] < 140,
            "a low-alpha wake column must stay clearly translucent, got {wake:?}"
        );

        gl.bind_vertex_array(None);
        gl.delete_buffer(vbo);
        gl.delete_vertex_array(vao);
        gl.delete_framebuffer(fbo);
        gl.delete_texture(out_tex);
        gl.delete_texture(volume);
        gl.delete_program(prog);
    }
}

/// Build a test-only reference variant of the volume shader which always
/// evaluates the tricubic reconstruction. The production shader keeps its
/// empty-space optimization; comparing it with this oracle makes probe/tail
/// disagreement observable without pinning driver-specific absolute colors.
#[cfg(feature = "x11-backends")]
fn waterlily_volume_shader_without_empty_space_skip(source: &str) -> String {
    let reconstruction_start = source
        .find("            vec4 voxel = sample_volume_tricubic(tex);")
        .expect("WaterLily tricubic reconstruction must remain discoverable");
    let Some(probe_start) = source[..reconstruction_start].rfind("            if (") else {
        return source.to_owned();
    };
    // With no pre-reconstruction empty-space skip, the nearest preceding
    // branch is the loop's ordinary break guard. In that case the production
    // shader already is the reference shader.
    if !source[probe_start..reconstruction_start].contains("continue;") {
        return source.to_owned();
    }
    let mut reference = String::with_capacity(source.len());
    reference.push_str(&source[..probe_start]);
    reference.push_str(&source[reconstruction_start..]);
    reference
}

/// Build a test-only control which keeps the complete scene/backdrop path but
/// zeros the confidence shared by front-interface lighting and refraction.
/// Comparing it with production output proves the curved-shell regression is
/// actually exercising that path rather than merely rendering over a scene.
#[cfg(feature = "x11-backends")]
fn waterlily_volume_shader_without_front_interface(source: &str) -> String {
    const NEEDLE: &str = "float interface_confidence = smoothstep(";
    assert_eq!(
        source.matches(NEEDLE).count(),
        1,
        "front-interface confidence must remain uniquely discoverable"
    );
    source.replacen(NEEDLE, "float interface_confidence = 0.0 * smoothstep(", 1)
}

/// A low-alpha voxel column has a one-voxel-wide trilinear footprint but a
/// two-voxel-wide cubic B-spline footprint. The cheap center probe must not
/// punch black holes into that wider reconstructed tail. This is the exact
/// failure mode that made isolated dark dots trace the jelly wake's rings.
#[cfg(feature = "x11-backends")]
#[test]
fn waterlily_volume_probe_preserves_sparse_bspline_tail() {
    use crate::backend::x11::compositor::shaders as s;

    let Some(h) = HeadlessGl::new(GlApi::GlCore33) else {
        eprintln!(
            "headless GL unavailable - skipping \
             waterlily_volume_probe_preserves_sparse_bspline_tail"
        );
        return;
    };
    let gl = &h.gl;
    const N: usize = 12;
    const OUTPUT: i32 = 64;

    // Four colored cells out of 12^3 form a short view-aligned wake column.
    // Alpha 28/255 stays in the shader's low-alpha medium branch while being
    // strong enough for its B-spline tail to survive RGBA8 readback.
    let mut voxels = vec![0_u8; N * N * N * 4];
    for z in 4..8 {
        let base = ((z * N + 6) * N + 6) * 4;
        voxels[base..base + 4].copy_from_slice(&[48, 224, 72, 28]);
    }

    let reference_shader =
        waterlily_volume_shader_without_empty_space_skip(s::WATERLILY_VOLUME_FRAGMENT_SHADER);
    // Exercise both non-default branches of the helper's state restoration:
    // the focused render must not leak its blend/dither choices into the next
    // test pass that shares this context.
    unsafe {
        gl.enable(glow::BLEND);
        gl.disable(glow::DITHER);
    }
    let optimized = render_waterlily_volume_frame(
        gl,
        s::WATERLILY_VOLUME_FRAGMENT_SHADER,
        &voxels,
        [N as i32; 3],
        [OUTPUT; 2],
        [0.5; 3],
        false,
    );
    unsafe {
        assert!(gl.is_enabled(glow::BLEND));
        assert!(!gl.is_enabled(glow::DITHER));
    }
    let repeated = render_waterlily_volume_frame(
        gl,
        s::WATERLILY_VOLUME_FRAGMENT_SHADER,
        &voxels,
        [N as i32; 3],
        [OUTPUT; 2],
        [0.5; 3],
        false,
    );
    assert_eq!(
        optimized, repeated,
        "an unchanged volume and timestamp must render bit-identically"
    );

    let reference = render_waterlily_volume_frame(
        gl,
        &reference_shader,
        &voxels,
        [N as i32; 3],
        [OUTPUT; 2],
        [0.5; 3],
        false,
    );
    let empty = render_waterlily_volume_frame(
        gl,
        s::WATERLILY_VOLUME_FRAGMENT_SHADER,
        &vec![0_u8; N * N * N * 4],
        [N as i32; 3],
        [OUTPUT; 2],
        [0.5; 3],
        false,
    );

    let mut visible_reference_pixels = 0_usize;
    let mut black_holes = 0_usize;
    let mut max_alpha_loss = 0_i16;
    let mut max_green_loss = 0_i16;
    for ((got, want), clear) in optimized
        .chunks_exact(4)
        .zip(reference.chunks_exact(4))
        .zip(empty.chunks_exact(4))
    {
        let reference_alpha_gain = i16::from(want[3]) - i16::from(clear[3]);
        if reference_alpha_gain >= 2 {
            visible_reference_pixels += 1;
            let alpha_loss = i16::from(want[3]) - i16::from(got[3]);
            let green_loss = i16::from(want[1]) - i16::from(got[1]);
            max_alpha_loss = max_alpha_loss.max(alpha_loss);
            max_green_loss = max_green_loss.max(green_loss);
            if alpha_loss >= 2 || green_loss >= 3 {
                black_holes += 1;
            }
        }
        assert!(
            got[0] <= got[3].saturating_add(1)
                && got[1] <= got[3].saturating_add(1)
                && got[2] <= got[3].saturating_add(1),
            "volume shader output must remain premultiplied, got {got:?}"
        );
    }

    eprintln!(
        "sparse B-spline tail: visible={visible_reference_pixels} \
         black_holes={black_holes} max_alpha_loss={max_alpha_loss} \
         max_green_loss={max_green_loss}"
    );
    assert!(
        visible_reference_pixels >= 8,
        "the synthetic sparse column must expose a measurable B-spline tail"
    );
    assert_eq!(
        black_holes, 0,
        "the center probe must not discard visibly reconstructed tail pixels \
         (max alpha loss {max_alpha_loss}, max green loss {max_green_loss})"
    );
}

/// A ray chord ending exactly around a half-step boundary must not gain or
/// lose one complete opacity sample. The two boxes differ by only 0.00064 of
/// a voxel in optical length, so RGBA8 output should remain within one code;
/// the former midpoint cutoff jumped by roughly six alpha codes here.
#[cfg(feature = "x11-backends")]
#[test]
fn waterlily_volume_fractional_tail_is_continuous() {
    use crate::backend::x11::compositor::shaders as s;

    let Some(h) = HeadlessGl::new(GlApi::GlCore33) else {
        eprintln!(
            "headless GL unavailable - skipping \
             waterlily_volume_fractional_tail_is_continuous"
        );
        return;
    };
    const N: usize = 8;
    const HALF_STEP_BOUNDARY: f32 = 0.390_625;
    const EPSILON: f32 = 0.000_02;
    let voxels = [190_u8, 140, 220, 48]
        .into_iter()
        .cycle()
        .take(N * N * N * 4)
        .collect::<Vec<_>>();
    let render = |half_depth| {
        render_waterlily_volume_frame(
            &h.gl,
            s::WATERLILY_VOLUME_FRAGMENT_SHADER,
            &voxels,
            [N as i32; 3],
            [1, 1],
            [0.5, 0.5, half_depth],
            false,
        )
    };
    let below = render(HALF_STEP_BOUNDARY - EPSILON);
    let above = render(HALF_STEP_BOUNDARY + EPSILON);

    assert!(below[3] > 20, "uniform volume must be visibly integrated");
    for channel in 0..4 {
        assert!(
            below[channel].abs_diff(above[channel]) <= 1,
            "fractional tail discontinuity in channel {channel}: \
             below={below:?}, above={above:?}"
        );
    }
}

/// A producer-shaped bell membrane exercises a broad curved surface instead
/// of one specially aligned voxel column.  The source follows Jelly's
/// antialiased spherical-shell coverage, tissue opacity band, and
/// apex-to-rim violet palette.  Rendering the complete frame through the
/// production LINEAR volume and occupancy samplers makes both isolated
/// probe holes and concentric transfer/shadow bands observable as spatial
/// discontinuities without relying on driver-specific golden pixels.
#[cfg(feature = "x11-backends")]
#[test]
fn waterlily_volume_curved_shell_is_spatially_coherent() {
    use crate::backend::x11::compositor::shaders as s;

    let Some(h) = HeadlessGl::new(GlApi::GlCore33) else {
        eprintln!(
            "headless GL unavailable - skipping \
             waterlily_volume_curved_shell_is_spatially_coherent"
        );
        return;
    };
    let gl = &h.gl;
    const N: usize = 32;
    const OUTPUT: usize = 96;

    // Reproduce the worker's analytic bell material at cell centres: an
    // approximately two-cell spherical shell, smoothly cut at its mouth and
    // feathered across about 1.6 cells. Quiescent volume RGB is the violet
    // palette's white midpoint even where alpha is zero, just like the
    // producer; this prevents straight-alpha reconstruction from inventing
    // an artificial black fringe around the membrane.
    let mut voxels = vec![0_u8; N * N * N * 4];
    let center = (N as f32 - 1.0) * 0.5;
    let radius = N as f32 * 0.31;
    for voxel in voxels.chunks_exact_mut(4) {
        voxel[..3].copy_from_slice(&[0xfa, 0xfa, 0xfd]);
    }
    for z in 0..N {
        let depth = z as f32 - center;
        for row in 0..N {
            // Published volume rows are top-to-bottom while world +Y is up.
            let height = center - row as f32;
            for x in 0..N {
                let lateral = x as f32 - center;
                let shell = ((lateral * lateral + height * height + depth * depth).sqrt() - radius)
                    .abs()
                    - 1.0;
                let mouth = -height;
                let lip_blend = (0.5 + 0.5 * (shell - mouth) / 1.2).clamp(0.0, 1.0);
                let lip = mouth + (shell - mouth) * lip_blend + 1.2 * lip_blend * (1.0 - lip_blend);
                let surface = (0.5 - 0.62 * lip).clamp(0.0, 1.0);
                if surface <= 0.0 {
                    continue;
                }

                let polar = (height / radius).clamp(-1.0, 1.0);
                let polar_t = ((polar + 0.35) / 1.20).clamp(0.0, 1.0);
                let apex_mix = polar_t * polar_t * (3.0 - 2.0 * polar_t);
                let rim = [216.0_f32, 212.0_f32, 255.0_f32];
                let apex = [184.0_f32, 156.0_f32, 246.0_f32];
                let membrane_mix = 0.95 * surface;
                let base = ((z * N + row) * N + x) * 4;
                for channel in 0..3 {
                    let membrane = rim[channel] + (apex[channel] - rim[channel]) * apex_mix;
                    voxels[base + channel] =
                        (250.0 + (membrane - 250.0) * membrane_mix).round() as u8;
                }
                // Producer tissue density is 0.44 * coverage and is encoded
                // with its 190/255 per-cell opacity range (max alpha ~= 0.33).
                voxels[base + 3] = (190.0 * 0.44 * surface).round() as u8;
            }
        }
    }

    let frame = render_waterlily_volume_frame(
        gl,
        s::WATERLILY_VOLUME_FRAGMENT_SHADER,
        &voxels,
        [N as i32; 3],
        [OUTPUT as i32; 2],
        [0.5; 3],
        true,
    );
    let repeated = render_waterlily_volume_frame(
        gl,
        s::WATERLILY_VOLUME_FRAGMENT_SHADER,
        &voxels,
        [N as i32; 3],
        [OUTPUT as i32; 2],
        [0.5; 3],
        true,
    );
    assert_eq!(
        frame, repeated,
        "a fixed curved volume and timestamp must render bit-identically"
    );

    let no_interface_shader =
        waterlily_volume_shader_without_front_interface(s::WATERLILY_VOLUME_FRAGMENT_SHADER);
    let without_interface = render_waterlily_volume_frame(
        gl,
        &no_interface_shader,
        &voxels,
        [N as i32; 3],
        [OUTPUT as i32; 2],
        [0.5; 3],
        true,
    );

    let mut clear_voxels = voxels.clone();
    for voxel in clear_voxels.chunks_exact_mut(4) {
        voxel[3] = 0;
    }
    let clear = render_waterlily_volume_frame(
        gl,
        s::WATERLILY_VOLUME_FRAGMENT_SHADER,
        &clear_voxels,
        [N as i32; 3],
        [OUTPUT as i32; 2],
        [0.5; 3],
        true,
    );

    let luma = |pixel: &[u8]| -> i32 {
        // Integer Rec.709 weights are sufficient for spatial comparisons and
        // keep the metric deterministic across host floating-point modes.
        (54 * i32::from(pixel[0]) + 183 * i32::from(pixel[1]) + 19 * i32::from(pixel[2]) + 128)
            / 256
    };
    let mut shell_signal = vec![0_i32; OUTPUT * OUTPUT];
    let mut alpha_gain = vec![0_i32; OUTPUT * OUTPUT];
    let mut visible_pixels = 0_usize;
    let mut bounds = [OUTPUT, OUTPUT, 0_usize, 0_usize];
    for (index, (got, background)) in frame.chunks_exact(4).zip(clear.chunks_exact(4)).enumerate() {
        assert!(
            got[0] <= got[3].saturating_add(1)
                && got[1] <= got[3].saturating_add(1)
                && got[2] <= got[3].saturating_add(1),
            "curved-shell output must remain premultiplied, got {got:?}"
        );
        shell_signal[index] = luma(got) - luma(background);
        alpha_gain[index] = i32::from(got[3]) - i32::from(background[3]);
        if alpha_gain[index] >= 4 {
            visible_pixels += 1;
            let x = index % OUTPUT;
            let y = index / OUTPUT;
            bounds[0] = bounds[0].min(x);
            bounds[1] = bounds[1].min(y);
            bounds[2] = bounds[2].max(x);
            bounds[3] = bounds[3].max(y);
        }
    }
    assert!(
        visible_pixels >= 240,
        "the synthetic bell must cover enough pixels for spatial analysis; \
         got {visible_pixels}"
    );

    let mut interface_changed_pixels = 0_usize;
    let mut max_interface_delta = 0_u8;
    for (index, (got, control)) in frame
        .chunks_exact(4)
        .zip(without_interface.chunks_exact(4))
        .enumerate()
    {
        if alpha_gain[index] < 4 {
            continue;
        }
        let pixel_delta = (0..3)
            .map(|channel| got[channel].abs_diff(control[channel]))
            .max()
            .unwrap_or(0);
        max_interface_delta = max_interface_delta.max(pixel_delta);
        if pixel_delta >= 1 {
            interface_changed_pixels += 1;
        }
    }
    eprintln!(
        "front interface: changed={interface_changed_pixels} \
         max_delta={max_interface_delta}"
    );
    assert!(
        interface_changed_pixels >= 12 && max_interface_delta >= 1,
        "the scene-enabled curved shell must measurably exercise its \
         confidence-gated interface lighting/refraction; changed \
         {interface_changed_pixels} pixels, max delta {max_interface_delta}"
    );

    // A skipped or invalid material sample becomes a dark singleton amid
    // eight well-covered neighbours, while an unstable normal/specular term
    // becomes an isolated bright firefly. Measure absolute output luma so
    // both sides of that spatial discontinuity remain observable.
    let mut interior_candidates = 0_usize;
    let mut isolated_dark_holes = 0_usize;
    let mut isolated_bright_fireflies = 0_usize;
    let mut worst_local_deficit = 0_i32;
    let mut worst_local_surplus = 0_i32;
    for y in 1..OUTPUT - 1 {
        for x in 1..OUTPUT - 1 {
            let index = y * OUTPUT + x;
            let mut neighbour_min = i32::MAX;
            let mut neighbour_max = i32::MIN;
            let mut surrounded = true;
            for dy in -1_isize..=1 {
                for dx in -1_isize..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let neighbour =
                        (y.wrapping_add_signed(dy)) * OUTPUT + x.wrapping_add_signed(dx);
                    surrounded &= alpha_gain[neighbour] >= 4;
                    let neighbour_luma = luma(&frame[neighbour * 4..neighbour * 4 + 4]);
                    neighbour_min = neighbour_min.min(neighbour_luma);
                    neighbour_max = neighbour_max.max(neighbour_luma);
                }
            }
            if surrounded {
                interior_candidates += 1;
                let center_luma = luma(&frame[index * 4..index * 4 + 4]);
                let deficit = neighbour_min - center_luma;
                let surplus = center_luma - neighbour_max;
                worst_local_deficit = worst_local_deficit.max(deficit);
                worst_local_surplus = worst_local_surplus.max(surplus);
                if deficit >= 10 {
                    isolated_dark_holes += 1;
                }
                if surplus >= 10 {
                    isolated_bright_fireflies += 1;
                }
            }
        }
    }
    assert!(
        interior_candidates >= 120,
        "the shell must expose a substantial covered interior; got \
         {interior_candidates} candidates within bounds {bounds:?}"
    );
    assert_eq!(
        isolated_dark_holes, 0,
        "the curved tissue interior must not contain isolated dark holes \
         (worst neighbour deficit {worst_local_deficit})"
    );
    assert_eq!(
        isolated_bright_fireflies, 0,
        "the curved tissue interior must not contain isolated bright fireflies \
         (worst neighbour surplus {worst_local_surplus})"
    );

    // Average concentric half-annuli through the dome's interior. Real
    // lighting and shell chord length may turn once or twice, but alternating
    // opacity/shadow isocontours produce repeated significant slope reversals.
    const RADIAL_BINS: usize = 7;
    let center_x = (OUTPUT as f32 - 1.0) * 0.5;
    let center_y = (OUTPUT as f32 - 1.0) * 0.5;
    let projected_radius =
        ((bounds[2] as f32 - center_x).max(center_x - bounds[0] as f32)).max(1.0);
    let mut radial_sum = [0_i64; RADIAL_BINS];
    let mut radial_count = [0_usize; RADIAL_BINS];
    for y in 0..OUTPUT {
        for x in 0..OUTPUT {
            // With OpenGL's bottom-left readback order, producer-world +Y
            // (the retained dome) occupies the lower framebuffer half.
            if y as f32 > center_y {
                continue;
            }
            let dx = x as f32 - center_x;
            let dy = center_y - y as f32;
            let radius_fraction = (dx * dx + dy * dy).sqrt() / projected_radius;
            if !(0.12..0.82).contains(&radius_fraction) {
                continue;
            }
            let bin = (((radius_fraction - 0.12) / 0.70) * RADIAL_BINS as f32).floor() as usize;
            let index = y * OUTPUT + x;
            radial_sum[bin] += i64::from(shell_signal[index]);
            radial_count[bin] += 1;
        }
    }
    assert!(
        radial_count.iter().all(|count| *count >= 6),
        "every radial band must be sampled, got counts {radial_count:?}"
    );
    let radial_luma: Vec<f32> = radial_sum
        .iter()
        .zip(radial_count)
        .map(|(sum, count)| *sum as f32 / count as f32)
        .collect();
    let mut slope_reversals = 0_usize;
    let mut previous_direction = 0_i32;
    let mut total_variation = 0.0_f32;
    for pair in radial_luma.windows(2) {
        let delta = pair[1] - pair[0];
        total_variation += delta.abs();
        let direction = if delta > 1.25 {
            1
        } else if delta < -1.25 {
            -1
        } else {
            0
        };
        if direction != 0 {
            if previous_direction != 0 && direction != previous_direction {
                slope_reversals += 1;
            }
            previous_direction = direction;
        }
    }
    let radial_min = radial_luma.iter().copied().fold(f32::INFINITY, f32::min);
    let radial_max = radial_luma
        .iter()
        .copied()
        .fold(f32::NEG_INFINITY, f32::max);
    let radial_span = radial_max - radial_min;
    eprintln!(
        "curved shell: visible={visible_pixels} bounds={bounds:?} \
         interior={interior_candidates} worst_hole={worst_local_deficit} \
         worst_firefly={worst_local_surplus} \
         radial_luma={radial_luma:?} reversals={slope_reversals} \
         variation={total_variation:.2} span={radial_span:.2}"
    );
    assert!(
        slope_reversals <= 2,
        "the bell must not develop concentric luma oscillations; radial \
         profile {radial_luma:?} has {slope_reversals} significant reversals"
    );
    assert!(
        total_variation <= 2.75 * radial_span + 6.0,
        "radial luma variation is excessive for one smooth shell: profile \
         {radial_luma:?}, variation {total_variation}, span {radial_span}"
    );
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

        // Input: solid premultiplied-style 2x2 texture with partial alpha.
        let texel = [100u8, 50, 25, 128];
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
        gl.uniform_1_f32(u("u_radius_top").as_ref(), 0.0);
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
            [100, 50, 25, 255],
            2,
            "opaque/no-dim",
        );

        // Case 2: dim 0.5 -> RGB halved, alpha stays opaque (u_opacity >= 0).
        gl.uniform_1_f32(u("u_dim").as_ref(), 0.5);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        gl.finish();
        assert_pixel(read_center(gl, W, H), [50, 25, 13, 255], 2, "dim-0.5");

        // Case 3: negative opacity selects texture alpha; its magnitude is the
        // layer fade and must scale premultiplied RGB and alpha exactly once.
        gl.uniform_1_f32(u("u_opacity").as_ref(), -0.5);
        gl.uniform_1_f32(u("u_dim").as_ref(), 1.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        gl.finish();
        assert_pixel(
            read_center(gl, W, H),
            [50, 25, 13, 64],
            2,
            "texture-alpha/layer-0.5",
        );

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
        gl.uniform_1_f32(u("u_radius_top").as_ref(), 0.0);
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

/// The shadow shader must produce a gaussian penumbra: full coverage well
/// inside the window rect, half coverage exactly at the rect edge, a smooth
/// decay outward, and an exact zero before the expanded quad edge (where the
/// geometry clips). Probes the falloff along the horizontal midline by
/// sliding the quad so each probe point lands on the readback pixel.
fn assert_shadow_gaussian_falloff(api: GlApi, what: &str, vs: &'static str, fs: &'static str) {
    const W: i32 = 16;
    const H: i32 = 16;
    // Window 64x64, spread 24, sharp corners. In expanded-quad pixel
    // coordinates the quad spans [0, 112], the window [24, 88].
    const SIZE: f32 = 64.0;
    const SPREAD: f32 = 24.0;
    const EXPANDED: f32 = SIZE + 2.0 * SPREAD;

    let Some(h) = HeadlessGl::new(api) else {
        eprintln!("headless GL unavailable - skipping {what}");
        return;
    };
    let gl = &h.gl;

    let prog = link(gl, vs, fs).unwrap_or_else(|log| panic!("{what}: shadow must link:\n{log}"));

    // Alpha of the shadow field at horizontal quad coordinate qx (midline).
    let sample = |qx: f32| -> [u8; 4] {
        render_quad(gl, prog, [0, 0, 0, 0], W, H, |gl| unsafe {
            let u = |n: &str| gl.get_uniform_location(prog, n);
            // Land quad point (qx, EXPANDED/2) on the center pixel's center.
            let cx = W as f32 / 2.0 + 0.5;
            let cy = H as f32 / 2.0 + 0.5;
            gl.uniform_4_f32(
                u("u_rect").as_ref(),
                cx - qx,
                cy - EXPANDED * 0.5,
                EXPANDED,
                EXPANDED,
            );
            gl.uniform_matrix_4_f32_slice(
                u("u_projection").as_ref(),
                false,
                &ortho(W as f32, H as f32),
            );
            gl.uniform_4_f32(u("u_shadow_color").as_ref(), 0.0, 0.0, 0.0, 0.8);
            gl.uniform_2_f32(u("u_size").as_ref(), SIZE, SIZE);
            gl.uniform_1_f32(u("u_radius").as_ref(), 0.0);
            gl.uniform_1_f32(u("u_radius_top").as_ref(), 0.0);
            gl.uniform_1_f32(u("u_spread").as_ref(), SPREAD);
        })
    };

    // Deep inside the window rect: full shadow color (0.8 * 255 = 204).
    assert_pixel(sample(56.0), [0, 0, 0, 204], 3, "shadow deep inside");
    // Exactly at the window edge (dist = 0): half coverage, the signature of
    // a blurred edge rather than a hard outline (0.5 * 0.8 * 255 = 102).
    assert_pixel(sample(88.0), [0, 0, 0, 102], 4, "shadow at window edge");
    // Mid penumbra (dist = spread/2 = 1.5 sigma): logistic decay ~0.072
    // (0.072 * 0.8 * 255 = 15).
    assert_pixel(sample(100.0), [0, 0, 0, 15], 4, "shadow mid penumbra");
    // One pixel before the quad edge: forced to (near) zero so the clipped
    // quad never shows a seam.
    assert_pixel(sample(111.0), [0, 0, 0, 0], 2, "shadow near quad edge");

    unsafe { gl.delete_program(prog) };
}

#[test]
fn wayland_shadow_shader_has_gaussian_penumbra() {
    use super::shaders as s;
    assert_shadow_gaussian_falloff(
        GlApi::Gles3,
        "wayland_shadow_shader_has_gaussian_penumbra",
        s::VERTEX_SHADER,
        s::SHADOW_FRAGMENT_SHADER,
    );
}

#[cfg(feature = "x11-backends")]
#[test]
fn x11_shadow_shader_has_gaussian_penumbra() {
    use crate::backend::x11::compositor::shaders as s;
    assert_shadow_gaussian_falloff(
        GlApi::GlCore33,
        "x11_shadow_shader_has_gaussian_penumbra",
        s::VERTEX_SHADER,
        s::SHADOW_FRAGMENT_SHADER,
    );
}

/// The gradient border shader must interpolate color A → color B along the
/// gradient direction, and must mask out the ring's interior. Uses a filled
/// quad (border width == quad size) to probe interior gradient values, and a
/// thin ring to check the center stays transparent.
fn assert_gradient_border_interpolates(api: GlApi, what: &str, vs: &'static str, fs: &'static str) {
    const W: i32 = 16;
    const H: i32 = 16;
    const SIZE: f32 = 100.0; // quad is SIZE x SIZE

    let Some(h) = HeadlessGl::new(api) else {
        eprintln!("headless GL unavailable - skipping {what}");
        return;
    };
    let gl = &h.gl;

    let prog = link(gl, vs, fs).unwrap_or_else(|log| panic!("{what}: gradient must link:\n{log}"));

    // Renders the quad so its local point (qx, qy) lands on the readback
    // pixel; angle in degrees, bw = ring thickness (SIZE = filled quad).
    let sample = |qx: f32, qy: f32, angle_deg: f32, bw: f32| -> [u8; 4] {
        render_quad(gl, prog, [0, 0, 0, 0], W, H, |gl| unsafe {
            let u = |n: &str| gl.get_uniform_location(prog, n);
            let cx = W as f32 / 2.0 + 0.5;
            let cy = H as f32 / 2.0 + 0.5;
            gl.uniform_4_f32(u("u_rect").as_ref(), cx - qx, cy - qy, SIZE, SIZE);
            gl.uniform_matrix_4_f32_slice(
                u("u_projection").as_ref(),
                false,
                &ortho(W as f32, H as f32),
            );
            gl.uniform_4_f32(u("u_color_a").as_ref(), 1.0, 0.0, 0.0, 1.0);
            gl.uniform_4_f32(u("u_color_b").as_ref(), 0.0, 0.0, 1.0, 1.0);
            gl.uniform_1_f32(u("u_gradient_angle").as_ref(), angle_deg.to_radians());
            gl.uniform_2_f32(u("u_size").as_ref(), SIZE, SIZE);
            gl.uniform_1_f32(u("u_radius").as_ref(), 0.0);
            gl.uniform_1_f32(u("u_radius_top").as_ref(), 0.0);
            gl.uniform_1_f32(u("u_border_width").as_ref(), bw);
            // Wayland variant only; ignored (None location) on the X11 one.
            gl.uniform_1_i32(u("u_scene_linear").as_ref(), 0);
        })
    };

    // Horizontal gradient (angle 0): t == v_uv.x, red → blue.
    assert_pixel(
        sample(10.0, 50.0, 0.0, SIZE),
        [230, 0, 26, 255],
        3,
        "gradient left",
    );
    assert_pixel(
        sample(50.0, 50.0, 0.0, SIZE),
        [128, 0, 128, 255],
        3,
        "gradient middle",
    );
    assert_pixel(
        sample(90.0, 50.0, 0.0, SIZE),
        [26, 0, 230, 255],
        3,
        "gradient right",
    );
    // Vertical gradient (angle 90): t == v_uv.y.
    assert_pixel(
        sample(50.0, 10.0, 90.0, SIZE),
        [230, 0, 26, 255],
        3,
        "gradient top",
    );
    assert_pixel(
        sample(50.0, 90.0, 90.0, SIZE),
        [26, 0, 230, 255],
        3,
        "gradient bottom",
    );
    // Thin ring: the interior is masked out. Blending is disabled in
    // render_quad, so the fully transparent fragment lands as-is.
    assert_pixel(
        sample(50.0, 50.0, 0.0, 4.0),
        [0, 0, 0, 0],
        2,
        "ring interior",
    );

    unsafe { gl.delete_program(prog) };
}

/// The frosted-glass surface must behave like glass, not like a colored rect:
/// with no tint it hands the backdrop through untouched, the tint covers it in
/// proportion to its alpha, the rim lights the whole perimeter, and the mask is
/// a squircle rather than a circular rounded rect.
fn assert_glass_surface_frosts_its_backdrop(
    api: GlApi,
    what: &str,
    vs: &'static str,
    fs: &'static str,
) {
    const W: i32 = 16;
    const H: i32 = 16;
    const SIZE: f32 = 100.0; // sheet is SIZE x SIZE

    let Some(h) = HeadlessGl::new(api) else {
        eprintln!("headless GL unavailable - skipping {what}");
        return;
    };
    let gl = &h.gl;

    let prog = link(gl, vs, fs).unwrap_or_else(|log| panic!("{what}: glass must link:\n{log}"));

    // A flat backdrop, so any sampling position gives the same reading and the
    // assertions describe the shader's own arithmetic rather than the blur's.
    // It also makes the refraction a no-op, which is what lets the tint and
    // rim be measured in isolation.
    let backdrop = [40u8, 120, 200, 255];

    struct Glass {
        tint: [f32; 4],
        radius: f32,
        corner_exp: f32,
        rim: f32,
        bevel: f32,
    }
    const PLAIN: Glass = Glass {
        tint: [0.0, 0.0, 0.0, 0.0],
        radius: 0.0,
        corner_exp: 2.0,
        rim: 0.0,
        bevel: 0.0,
    };

    // Renders the sheet so its local point (qx, qy) lands on the readback pixel.
    let sample = |qx: f32, qy: f32, g: &Glass| -> [u8; 4] {
        render_quad(gl, prog, backdrop, W, H, |gl| unsafe {
            let u = |n: &str| gl.get_uniform_location(prog, n);
            let cx = W as f32 / 2.0 + 0.5;
            let cy = H as f32 / 2.0 + 0.5;
            gl.uniform_4_f32(u("u_rect").as_ref(), cx - qx, cy - qy, SIZE, SIZE);
            gl.uniform_matrix_4_f32_slice(
                u("u_projection").as_ref(),
                false,
                &ortho(W as f32, H as f32),
            );
            gl.uniform_1_i32(u("u_backdrop").as_ref(), 0);
            gl.uniform_2_f32(u("u_screen_size").as_ref(), W as f32, H as f32);
            gl.uniform_4_f32(
                u("u_tint").as_ref(),
                g.tint[0],
                g.tint[1],
                g.tint[2],
                g.tint[3],
            );
            gl.uniform_2_f32(u("u_size").as_ref(), SIZE, SIZE);
            gl.uniform_1_f32(u("u_radius").as_ref(), g.radius);
            gl.uniform_1_f32(u("u_radius_top").as_ref(), g.radius);
            gl.uniform_1_f32(u("u_corner_exp").as_ref(), g.corner_exp);
            gl.uniform_1_f32(u("u_saturation").as_ref(), 1.0);
            gl.uniform_1_f32(u("u_luminance").as_ref(), 1.0);
            gl.uniform_1_f32(u("u_bevel_width").as_ref(), g.bevel);
            gl.uniform_1_f32(u("u_refraction").as_ref(), 0.0);
            gl.uniform_1_f32(u("u_rim_width").as_ref(), 6.0);
            gl.uniform_1_f32(u("u_rim_intensity").as_ref(), g.rim);
            gl.uniform_3_f32(u("u_rim_tint").as_ref(), 1.0, 1.0, 1.0);
            gl.uniform_1_f32(u("u_sheen").as_ref(), 0.0);
            gl.uniform_1_f32(u("u_edge_shade").as_ref(), 0.0);
            gl.uniform_1_f32(u("u_grain").as_ref(), 0.0);
            gl.uniform_1_f32(u("u_alpha").as_ref(), 1.0);
            // Wayland variant only; ignored (None location) on the X11 one.
            gl.uniform_1_i32(u("u_scene_linear").as_ref(), 0);
        })
    };

    // No tint, no edge lighting: the sheet is a window onto its backdrop.
    assert_pixel(
        sample(50.0, 50.0, &PLAIN),
        backdrop,
        2,
        "untinted glass passes the backdrop through",
    );
    // Full coverage: the tint wins outright.
    assert_pixel(
        sample(
            50.0,
            50.0,
            &Glass {
                tint: [1.0, 0.0, 0.0, 1.0],
                ..PLAIN
            },
        ),
        [255, 0, 0, 255],
        2,
        "opaque tint hides the backdrop",
    );
    // Half coverage: the midpoint between backdrop and tint.
    assert_pixel(
        sample(
            50.0,
            50.0,
            &Glass {
                tint: [1.0, 0.0, 0.0, 0.5],
                ..PLAIN
            },
        ),
        [148, 60, 100, 255],
        3,
        "half-covered glass blends toward the tint",
    );

    // The rim lights the *whole* perimeter, not just one edge — that is what
    // separates a pane of glass from a card with a top highlight.
    let lit = Glass {
        rim: 0.5,
        bevel: 8.0,
        ..PLAIN
    };
    let middle = sample(50.0, 50.0, &lit);
    for (name, (qx, qy)) in [
        ("top", (50.0, 1.0)),
        ("bottom", (50.0, SIZE - 1.0)),
        ("left", (1.0, 50.0)),
        ("right", (SIZE - 1.0, 50.0)),
    ] {
        let edge = sample(qx, qy, &lit);
        assert!(
            edge[0] > middle[0] + 15,
            "{what}: the {name} edge must catch the rim light, got {edge:?} vs {middle:?}"
        );
    }
    assert_pixel(
        middle,
        backdrop,
        2,
        "rim lighting does not reach the middle",
    );

    // Continuous corners: a point that a circular radius clips away is still
    // inside the squircle, because the superellipse bulges toward the corner.
    // On the 45° diagonal of a 32px corner the circle turns back at 9.4px from
    // the corner and the n=4.2 squircle at 4.9px, so 7px falls between them.
    let probe = 7.0;
    let circular = sample(
        probe,
        probe,
        &Glass {
            tint: [1.0, 0.0, 0.0, 1.0],
            radius: 32.0,
            corner_exp: 2.0,
            ..PLAIN
        },
    );
    let squircle = sample(
        probe,
        probe,
        &Glass {
            tint: [1.0, 0.0, 0.0, 1.0],
            radius: 32.0,
            corner_exp: 4.2,
            ..PLAIN
        },
    );
    assert_pixel(
        circular,
        [0, 0, 0, 255],
        2,
        "circular corner clips the probe",
    );
    assert_pixel(
        squircle,
        [255, 0, 0, 255],
        2,
        "the squircle corner still covers the probe",
    );

    unsafe { gl.delete_program(prog) };
}

#[test]
fn wayland_glass_surface_frosts_its_backdrop() {
    use super::shaders as s;
    assert_glass_surface_frosts_its_backdrop(
        GlApi::Gles3,
        "wayland_glass_surface_frosts_its_backdrop",
        s::VERTEX_SHADER,
        s::GLASS_FRAGMENT_SHADER,
    );
}

#[cfg(feature = "x11-backends")]
#[test]
fn x11_glass_surface_frosts_its_backdrop() {
    use crate::backend::x11::compositor::shaders as s;
    assert_glass_surface_frosts_its_backdrop(
        GlApi::GlCore33,
        "x11_glass_surface_frosts_its_backdrop",
        s::VERTEX_SHADER,
        s::GLASS_FRAGMENT_SHADER,
    );
}

#[test]
fn wayland_gradient_border_shader_interpolates() {
    use super::shaders as s;
    assert_gradient_border_interpolates(
        GlApi::Gles3,
        "wayland_gradient_border_shader_interpolates",
        s::VERTEX_SHADER,
        s::GRADIENT_BORDER_FRAGMENT_SHADER,
    );
}

#[cfg(feature = "x11-backends")]
#[test]
fn x11_gradient_border_shader_interpolates() {
    use crate::backend::x11::compositor::shaders as s;
    assert_gradient_border_interpolates(
        GlApi::GlCore33,
        "x11_gradient_border_shader_interpolates",
        s::VERTEX_SHADER,
        s::GRADIENT_BORDER_FRAGMENT_SHADER,
    );
}

/// The main window shader's inactive desaturation must be a pure luminance
/// mix: off at 0, full grayscale at 1, and a linear blend in between.
fn assert_window_shader_desaturates(api: GlApi, what: &str, vs: &'static str, fs: &'static str) {
    const W: i32 = 16;
    const H: i32 = 16;

    let Some(h) = HeadlessGl::new(api) else {
        eprintln!("headless GL unavailable - skipping {what}");
        return;
    };
    let gl = &h.gl;

    let prog = link(gl, vs, fs).unwrap_or_else(|log| panic!("{what}: window must link:\n{log}"));

    let input = [200u8, 100, 50, 255];
    let sample = |desat: f32| -> [u8; 4] {
        render_quad(gl, prog, input, W, H, |gl| unsafe {
            let u = |n: &str| gl.get_uniform_location(prog, n);
            gl.uniform_4_f32(u("u_rect").as_ref(), 0.0, 0.0, W as f32, H as f32);
            gl.uniform_matrix_4_f32_slice(
                u("u_projection").as_ref(),
                false,
                &ortho(W as f32, H as f32),
            );
            gl.uniform_1_i32(u("u_texture").as_ref(), 0);
            gl.uniform_1_f32(u("u_opacity").as_ref(), 1.0);
            gl.uniform_1_f32(u("u_radius").as_ref(), 0.0);
            gl.uniform_1_f32(u("u_radius_top").as_ref(), 0.0);
            gl.uniform_2_f32(u("u_size").as_ref(), W as f32, H as f32);
            gl.uniform_1_f32(u("u_dim").as_ref(), 1.0);
            gl.uniform_1_f32(u("u_desat").as_ref(), desat);
            gl.uniform_4_f32(u("u_uv_rect").as_ref(), 0.0, 0.0, 1.0, 1.0);
            gl.uniform_1_f32(u("u_ripple_amplitude").as_ref(), 0.0);
            // Wayland-only color-management uniforms; None (no-op) on X11.
            gl.uniform_1_i32(u("u_color_managed").as_ref(), 0);
            gl.uniform_1_i32(u("u_scene_linear").as_ref(), 0);
        })
    };

    // Off: exact passthrough.
    assert_pixel(sample(0.0), input, 2, "desat off");
    // Full: rgb collapse to luminance dot(rgb, 0.2126/0.7152/0.0722) ≈ 118.
    assert_pixel(sample(1.0), [118, 118, 118, 255], 2, "desat full");
    // Half: linear midpoint between passthrough and grayscale.
    assert_pixel(sample(0.5), [159, 109, 84, 255], 2, "desat half");

    unsafe { gl.delete_program(prog) };
}

#[test]
fn wayland_window_shader_desaturates_inactive() {
    use super::shaders as s;
    assert_window_shader_desaturates(
        GlApi::Gles3,
        "wayland_window_shader_desaturates_inactive",
        s::VERTEX_SHADER,
        s::FRAGMENT_SHADER,
    );
}

#[cfg(feature = "x11-backends")]
#[test]
fn x11_window_shader_desaturates_inactive() {
    use crate::backend::x11::compositor::shaders as s;
    assert_window_shader_desaturates(
        GlApi::GlCore33,
        "x11_window_shader_desaturates_inactive",
        s::VERTEX_SHADER,
        s::FRAGMENT_SHADER,
    );
}

/// Routing an SDR desktop through the HDR pass must not change its brightness.
///
/// The pass scales SDR content to absolute nits, tone maps, then encodes back.
/// Every step of that round trip has to cancel: with no tone curve and an SDR
/// output EOTF it is by definition an identity. It was not — the encode divided
/// by the display peak instead of the SDR reference white it had multiplied by,
/// so turning HDR on dimmed the entire screen, wallpaper included, to
/// `(80/peak) ^ (1/2.2)` — about 47% at the default 400-nit peak — and no
/// brightness control could bring it back.
#[cfg(feature = "x11-backends")]
#[test]
fn x11_hdr_pass_does_not_dim_an_sdr_desktop() {
    use crate::backend::x11::compositor::shaders as s;

    const W: i32 = 8;
    const H: i32 = 8;
    let what = "x11_hdr_pass_does_not_dim_an_sdr_desktop";

    let Some(h) = HeadlessGl::new(GlApi::GlCore33) else {
        eprintln!("headless GL unavailable - skipping {what}");
        return;
    };
    let gl = &h.gl;
    let prog = link(
        gl,
        s::BLUR_DOWN_VERTEX,
        s::ADVANCED_POSTPROCESS_FRAGMENT_SHADER,
    )
    .unwrap_or_else(|log| panic!("{what}: postprocess must link:\n{log}"));

    let sample = |input: [u8; 4], hdr: bool, tone_mapping: i32| -> [u8; 4] {
        render_quad(gl, prog, input, W, H, |gl| unsafe {
            let u = |n: &str| gl.get_uniform_location(prog, n);
            gl.uniform_matrix_4_f32_slice(
                u("u_projection").as_ref(),
                false,
                &ortho(W as f32, H as f32),
            );
            gl.uniform_4_f32(u("u_rect").as_ref(), 0.0, 0.0, W as f32, H as f32);
            gl.uniform_1_i32(u("u_texture").as_ref(), 0);
            gl.uniform_1_f32(u("u_color_temp").as_ref(), 0.0);
            gl.uniform_1_f32(u("u_saturation").as_ref(), 1.0);
            gl.uniform_1_f32(u("u_brightness").as_ref(), 1.0);
            gl.uniform_1_f32(u("u_contrast").as_ref(), 1.0);
            gl.uniform_1_i32(u("u_invert").as_ref(), 0);
            gl.uniform_1_i32(u("u_grayscale").as_ref(), 0);
            gl.uniform_1_i32(u("u_magnifier_enabled").as_ref(), 0);
            gl.uniform_2_f32(u("u_magnifier_center").as_ref(), 0.5, 0.5);
            gl.uniform_1_f32(u("u_magnifier_radius").as_ref(), 0.0);
            gl.uniform_1_f32(u("u_magnifier_zoom").as_ref(), 1.0);
            gl.uniform_1_i32(u("u_colorblind_mode").as_ref(), 0);
            gl.uniform_1_i32(u("u_hdr_enabled").as_ref(), i32::from(hdr));
            gl.uniform_1_f32(u("u_hdr_peak_nits").as_ref(), 400.0);
            gl.uniform_1_i32(u("u_tone_mapping_method").as_ref(), tone_mapping);
            // SDR display: sRGB gamma out, BT.709 primaries — what the EDID
            // check falls back to when the panel advertises no HDR metadata.
            gl.uniform_1_i32(u("u_eotf_mode").as_ref(), 0);
            gl.uniform_1_i32(u("u_output_colorspace").as_ref(), 0);
        })
    };

    for input in [
        [64u8, 64, 64, 255],
        [128, 128, 128, 255],
        [255, 255, 255, 255],
    ] {
        let off = sample(input, false, 0);
        assert_pixel(off, input, 2, "hdr off is a passthrough");
        // Tolerance covers the pow(2.2)/pow(1/2.2) round trip at 8 bits.
        assert_pixel(sample(input, true, 0), off, 3, "hdr on, no tone curve");
    }

    // With a tone curve the midtones may move, but white must stay white:
    // an SDR desktop's peak is the display's peak.
    let white = sample([255, 255, 255, 255], true, 2);
    assert!(
        white[0] >= 250,
        "{what}: ACES crushed white to {white:?}, the screen would read as grey"
    );
}

/// The wobbly mesh must sit exactly on the window rect at rest and follow the
/// spring grid's per-node offsets once a drag deforms it.
///
/// This is the only test that runs the vertex shader against offsets produced
/// by the real physics, so it pins the contract between `WobblyState`'s node
/// layout (`row * grid_n + col`, drag lag weighted by distance from the grabbed
/// node) and the `u_grid_offsets` lookup the shader performs.
fn assert_wobbly_mesh_follows_grid_offsets(
    api: GlApi,
    what: &str,
    vs: &'static str,
    fs: &'static str,
) {
    use crate::backend::compositor_common::effects::wobbly_node_count;
    use crate::backend::compositor_common::wobbly::WobblyState;

    const W: i32 = 64;
    const H: i32 = 64;
    // Centred rect, so lag has room to move the mesh without leaving the FBO.
    const RECT: [f32; 4] = [16.0, 16.0, 32.0, 32.0];
    const DRAG: f32 = 16.0;

    let Some(h) = HeadlessGl::new(api) else {
        eprintln!("headless GL unavailable - skipping {what}");
        return;
    };
    let gl = &h.gl;
    let prog = link(gl, vs, fs).unwrap_or_else(|log| panic!("{what}: wobbly must link:\n{log}"));

    let grid_n = wobbly_node_count(8);
    let mut wobbly = WobblyState::new(grid_n, grid_n / 2, grid_n / 2, RECT[2], RECT[3]);

    let render = |wobbly: &WobblyState| -> Vec<u8> {
        let mut flat = Vec::with_capacity(wobbly.offsets.len() * 2);
        for offset in &wobbly.offsets {
            flat.push(offset[0]);
            flat.push(offset[1]);
        }
        render_mesh(gl, prog, W, H, wobbly.grid_n as i32, |gl| unsafe {
            let u = |n: &str| gl.get_uniform_location(prog, n);
            gl.uniform_matrix_4_f32_slice(
                u("u_projection").as_ref(),
                false,
                &ortho(W as f32, H as f32),
            );
            gl.uniform_4_f32(u("u_rect").as_ref(), RECT[0], RECT[1], RECT[2], RECT[3]);
            gl.uniform_1_i32(u("u_texture").as_ref(), 0);
            gl.uniform_1_f32(u("u_opacity").as_ref(), 1.0);
            gl.uniform_1_f32(u("u_radius").as_ref(), 0.0);
            gl.uniform_1_f32(u("u_radius_top").as_ref(), 0.0);
            gl.uniform_2_f32(u("u_size").as_ref(), RECT[2], RECT[3]);
            gl.uniform_1_f32(u("u_dim").as_ref(), 1.0);
            gl.uniform_1_f32(u("u_desat").as_ref(), 0.0);
            gl.uniform_4_f32(u("u_uv_rect").as_ref(), 0.0, 0.0, 1.0, 1.0);
            gl.uniform_1_f32(u("u_ripple_amplitude").as_ref(), 0.0);
            gl.uniform_1_i32(u("u_color_managed").as_ref(), 0);
            gl.uniform_1_i32(u("u_scene_linear").as_ref(), 0);
            gl.uniform_2_f32_slice(u("u_grid_offsets").as_ref(), &flat);
            gl.uniform_1_i32(u("u_grid_n").as_ref(), wobbly.grid_n as i32);
        })
    };

    // Both probes sit on the rect's vertical middle and clear of every edge the
    // drag moves, so a covered/uncovered flip can only come from the offsets.
    let covered = |frame: &[u8], x: i32, y: i32| -> bool { frame[((y * W + x) * 4) as usize] > 8 };
    let inside_right = (44, 32);
    let outside_left = (10, 32);

    let at_rest = render(&wobbly);
    assert!(
        covered(&at_rest, inside_right.0, inside_right.1),
        "{what}: undeformed mesh must fill its rect"
    );
    assert!(
        !covered(&at_rest, outside_left.0, outside_left.1),
        "{what}: undeformed mesh must not spill outside its rect"
    );

    // Drag the window right: every node lags left, the grabbed centre least.
    wobbly.apply_window_move_delta(DRAG, 0.0);
    let dragged = render(&wobbly);
    assert!(
        !covered(&dragged, inside_right.0, inside_right.1),
        "{what}: trailing edge did not lag behind the drag"
    );
    assert!(
        covered(&dragged, outside_left.0, outside_left.1),
        "{what}: leading edge did not stretch out of the rect"
    );

    unsafe { gl.delete_program(prog) };
}

#[test]
fn wayland_wobbly_mesh_follows_grid_offsets() {
    use super::shaders as s;
    assert_wobbly_mesh_follows_grid_offsets(
        GlApi::Gles3,
        "wayland_wobbly_mesh_follows_grid_offsets",
        s::WOBBLY_VERTEX_SHADER,
        s::FRAGMENT_SHADER,
    );
}

#[cfg(feature = "x11-backends")]
#[test]
fn x11_wobbly_mesh_follows_grid_offsets() {
    use crate::backend::x11::compositor::shaders as s;
    assert_wobbly_mesh_follows_grid_offsets(
        GlApi::GlCore33,
        "x11_wobbly_mesh_follows_grid_offsets",
        s::WOBBLY_VERTEX_SHADER,
        s::FRAGMENT_SHADER,
    );
}

/// A docked panel's top corners must be square and its bottom corners round.
///
/// This is the whole shape of the Dynamic-Island effect: the panel merges with
/// the bar above it because the two meet along a straight edge, and reads as a
/// separate object below because it curves away. A single-radius mask cannot
/// express that, so the SDF picks its radius per half — and the halves have to
/// meet without a seam, which is what the mid-edge probes check.
fn assert_island_corners_are_asymmetric(
    api: GlApi,
    what: &str,
    vs: &'static str,
    fs: &'static str,
) {
    const W: i32 = 64;
    const H: i32 = 64;
    const RADIUS: f32 = 20.0;

    let Some(h) = HeadlessGl::new(api) else {
        eprintln!("headless GL unavailable - skipping {what}");
        return;
    };
    let gl = &h.gl;
    let prog = link(gl, vs, fs).unwrap_or_else(|log| panic!("{what}: border must link:\n{log}"));

    let frame = render_quad_frame(gl, prog, [255, 255, 255, 255], W, H, |gl| unsafe {
        let u = |n: &str| gl.get_uniform_location(prog, n);
        gl.uniform_matrix_4_f32_slice(
            u("u_projection").as_ref(),
            false,
            &ortho(W as f32, H as f32),
        );
        gl.uniform_4_f32(u("u_rect").as_ref(), 0.0, 0.0, W as f32, H as f32);
        gl.uniform_2_f32(u("u_size").as_ref(), W as f32, H as f32);
        gl.uniform_4_f32(u("u_border_color").as_ref(), 1.0, 1.0, 1.0, 1.0);
        // A border wider than the rect fills it, which is how every JWM panel
        // is drawn through this program.
        gl.uniform_1_f32(u("u_border_width").as_ref(), W as f32);
        gl.uniform_1_f32(u("u_radius").as_ref(), RADIUS);
        gl.uniform_1_f32(u("u_radius_top").as_ref(), 0.0);
        gl.uniform_1_i32(u("u_scene_linear").as_ref(), 0);
    });

    // The shader's own space is what the split is expressed in: `p.y < 0` — the
    // half nearer y = 0 — takes the top radius. `ortho` here maps y = 0 to NDC
    // -1 and read_pixels starts at NDC -1, so a readback row *is* a shader row.
    // (Production projects top-left-origin instead, which is why that half is
    // the screen's top there.)
    let covered =
        |x: i32, shader_row: i32| -> bool { frame[((shader_row * W + x) * 4) as usize] > 128 };

    // Two pixels into the top-left corner: square, so it is filled.
    assert!(covered(2, 2), "{what}: top-left corner was rounded off");
    assert!(
        covered(W - 3, 2),
        "{what}: top-right corner was rounded off"
    );
    // The same inset at the bottom sits outside a 20 px radius: cut away.
    assert!(
        !covered(2, H - 3),
        "{what}: bottom-left corner stayed square"
    );
    assert!(
        !covered(W - 3, H - 3),
        "{what}: bottom-right corner stayed square"
    );

    // The straight edges must not step where the two radii meet. Inside the
    // rect the radius cancels out of the SDF, so a seam here would mean the
    // split was done wrong.
    for row in [H / 2 - 1, H / 2, H / 2 + 1] {
        assert!(covered(1, row), "{what}: left edge broke at row {row}");
        assert!(covered(W - 2, row), "{what}: right edge broke at row {row}");
    }

    unsafe { gl.delete_program(prog) };
}

#[test]
fn wayland_island_corners_are_asymmetric() {
    use super::shaders as s;
    assert_island_corners_are_asymmetric(
        GlApi::Gles3,
        "wayland_island_corners_are_asymmetric",
        s::VERTEX_SHADER,
        s::BORDER_FRAGMENT_SHADER,
    );
}

#[cfg(feature = "x11-backends")]
#[test]
fn x11_island_corners_are_asymmetric() {
    use crate::backend::x11::compositor::shaders as s;
    assert_island_corners_are_asymmetric(
        GlApi::GlCore33,
        "x11_island_corners_are_asymmetric",
        s::VERTEX_SHADER,
        s::BORDER_FRAGMENT_SHADER,
    );
}
