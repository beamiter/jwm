//! Standalone source-exact cache checks. Run test-procedural-wallpaper-cache.sh.
use glow::HasContext;
use std::ffi::{CString, c_char, c_void};
#[path = "../src/backend/x11/compositor/procedural_wallpaper_cache.rs"]
mod cache;
unsafe extern "C" {
    fn eglGetPlatformDisplay(
        platform: u32,
        native: *mut c_void,
        attrs: *const isize,
    ) -> *mut c_void;
    fn eglInitialize(d: *mut c_void, major: *mut i32, minor: *mut i32) -> u32;
    fn eglBindAPI(api: u32) -> u32;
    fn eglChooseConfig(
        d: *mut c_void,
        attrs: *const i32,
        cfg: *mut *mut c_void,
        n: i32,
        count: *mut i32,
    ) -> u32;
    fn eglCreateContext(
        d: *mut c_void,
        cfg: *mut c_void,
        share: *mut c_void,
        attrs: *const i32,
    ) -> *mut c_void;
    fn eglMakeCurrent(
        d: *mut c_void,
        draw: *mut c_void,
        read: *mut c_void,
        ctx: *mut c_void,
    ) -> u32;
    fn eglDestroyContext(d: *mut c_void, ctx: *mut c_void) -> u32;
    fn eglTerminate(d: *mut c_void) -> u32;
    fn eglGetProcAddress(name: *const c_char) -> *const c_void;
}
fn unavailable(reason: &str) {
    assert_ne!(
        std::env::var("JWM_REQUIRE_HEADLESS_GL").as_deref(),
        Ok("1"),
        "required headless GL unavailable: {reason}"
    );
    eprintln!("SKIP: {reason}");
}

/// Exercise the exact production mipmap adoption on both APIs. A high-frequency
/// source must converge to its area average; the conservative error fallback
/// must still expose the intact base image and restore the caller's binding.
unsafe fn equirectangular_mipmaps_preserve_state_and_base_fallback(gl: &glow::Context, es: bool) {
    unsafe {
        let source = gl.create_texture().unwrap();
        let sentinel = gl.create_texture().unwrap();
        let output = gl.create_texture().unwrap();
        let target = gl.create_framebuffer().unwrap();
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(output));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            8,
            4,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(output),
            0,
        );
        assert_eq!(
            gl.check_framebuffer_status(glow::FRAMEBUFFER),
            glow::FRAMEBUFFER_COMPLETE
        );
        gl.active_texture(glow::TEXTURE3);
        gl.bind_texture(glow::TEXTURE_2D, Some(source));
        let pixels: Vec<_> = (0..64 * 32)
            .flat_map(|index| {
                let value = if index % 2 == 0 { 0 } else { 255 };
                [value, value, value, 255]
            })
            .collect();
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            64,
            32,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&pixels)),
        );
        gl.bind_texture(glow::TEXTURE_2D, Some(sentinel));
        assert!(cache::prepare_equirectangular_texture(gl, source));
        assert_eq!(
            gl.get_parameter_i32(glow::ACTIVE_TEXTURE),
            glow::TEXTURE3 as i32
        );
        assert_eq!(
            gl.get_parameter_texture(glow::TEXTURE_BINDING_2D),
            Some(sentinel)
        );
        assert_eq!(
            gl.get_parameter_framebuffer(glow::DRAW_FRAMEBUFFER_BINDING),
            Some(target)
        );
        gl.bind_texture(glow::TEXTURE_2D, Some(source));
        for (parameter, expected) in [
            (glow::TEXTURE_WRAP_S, glow::REPEAT),
            (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            (glow::TEXTURE_MIN_FILTER, glow::LINEAR_MIPMAP_LINEAR),
        ] {
            assert_eq!(
                gl.get_tex_parameter_i32(glow::TEXTURE_2D, parameter),
                expected as i32
            );
        }

        let prefix = if es {
            "#version 300 es\nprecision highp float;\nprecision highp int;\nprecision highp sampler2D;\n"
        } else {
            "#version 330 core\n"
        };
        let vertex = gl.create_shader(glow::VERTEX_SHADER).unwrap();
        gl.shader_source(vertex, &format!("{prefix}void main(){{vec2 p=vec2((gl_VertexID<<1)&2,gl_VertexID&2);gl_Position=vec4(p*2.-1.,0,1);}}"));
        gl.compile_shader(vertex);
        assert!(
            gl.get_shader_compile_status(vertex),
            "{}",
            gl.get_shader_info_log(vertex)
        );
        let fragment = gl.create_shader(glow::FRAGMENT_SHADER).unwrap();
        gl.shader_source(fragment, &format!("{prefix}uniform sampler2D map;out vec4 color;void main(){{vec2 uv=vec2((floor(gl_FragCoord.x)*8.+.5)/64.+1.,-.5);color=textureLod(map,uv,2.);}}"));
        gl.compile_shader(fragment);
        assert!(
            gl.get_shader_compile_status(fragment),
            "{}",
            gl.get_shader_info_log(fragment)
        );
        let program = gl.create_program().unwrap();
        gl.attach_shader(program, vertex);
        gl.attach_shader(program, fragment);
        gl.link_program(program);
        assert!(
            gl.get_program_link_status(program),
            "{}",
            gl.get_program_info_log(program)
        );
        gl.use_program(Some(program));
        gl.uniform_1_i32(gl.get_uniform_location(program, "map").as_ref(), 3);
        let vao = gl.create_vertex_array().unwrap();
        gl.bind_vertex_array(Some(vao));
        gl.viewport(0, 0, 8, 4);
        gl.disable(glow::SCISSOR_TEST);
        gl.disable(glow::BLEND);
        for fallback in [false, true] {
            if fallback {
                // A reported GL error must conservatively keep the original
                // image usable. This injects an error, not a simulated OOM.
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, -1);
                gl.bind_texture(glow::TEXTURE_2D, Some(sentinel));
                assert!(!cache::prepare_equirectangular_texture(gl, source));
                assert_eq!(
                    gl.get_parameter_texture(glow::TEXTURE_BINDING_2D),
                    Some(sentinel)
                );
                gl.bind_texture(glow::TEXTURE_2D, Some(source));
                assert_eq!(
                    gl.get_tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER),
                    glow::LINEAR as i32
                );
                assert_eq!(
                    gl.get_tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAX_LEVEL),
                    0
                );
            }
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            let mut rendered = [0u8; 8 * 4 * 4];
            gl.read_pixels(
                0,
                0,
                8,
                4,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut rendered)),
            );
            for pixel in rendered.chunks_exact(4) {
                assert_eq!(pixel[3], 255);
                if fallback {
                    assert_eq!(
                        &pixel[..3],
                        &[0, 0, 0],
                        "base image survives error fallback"
                    );
                } else {
                    assert!(
                        pixel[..3].iter().all(|v| (127..=128).contains(v)),
                        "mipmaps must average alternating texels: {pixel:?}"
                    );
                }
            }
            assert_eq!(gl.get_error(), glow::NO_ERROR);
        }
        gl.use_program(None);
        gl.bind_vertex_array(None);
        gl.bind_texture(glow::TEXTURE_2D, None);
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, None);
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        gl.delete_vertex_array(vao);
        gl.delete_program(program);
        gl.delete_shader(vertex);
        gl.delete_shader(fragment);
        gl.delete_framebuffer(target);
        for texture in [source, sentinel, output] {
            gl.delete_texture(texture);
        }
        eprintln!("PASS: equirectangular mipmap pixels, state, repeat, and base fallback");
    }
}

#[test]
fn surfaceless_frame_cache_state_pixels_precision_and_resize() {
    unsafe {
        for es in [false, true] {
            let null = std::ptr::null_mut();
            let d = eglGetPlatformDisplay(0x31DD, null, std::ptr::null());
            if d.is_null() || eglInitialize(d, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
                unavailable("surfaceless EGL unavailable");
                return;
            }
            if eglBindAPI(if es { 0x30A0 } else { 0x30A2 }) == 0 {
                eglTerminate(d);
                unavailable("requested GL API unavailable");
                return;
            }
            let mut cfg = null;
            let mut count = 0;
            if eglChooseConfig(
                d,
                [0x3033, 1, 0x3040, if es { 0x40 } else { 8 }, 0x3038].as_ptr(),
                &mut cfg,
                1,
                &mut count,
            ) == 0
                || count == 0
            {
                eglTerminate(d);
                unavailable("requested EGL config unavailable");
                return;
            }
            let ctx = eglCreateContext(d, cfg, null, [0x3098, 3, 0x3038].as_ptr());
            if ctx.is_null() || eglMakeCurrent(d, null, null, ctx) == 0 {
                if !ctx.is_null() {
                    eglDestroyContext(d, ctx);
                }
                eglTerminate(d);
                unavailable("surfaceless GL context unavailable");
                return;
            }
            let gl = glow::Context::from_loader_function(|name| {
                eglGetProcAddress(CString::new(name).unwrap().as_ptr())
            });
            eprintln!(
                "Renderer {} / {}",
                gl.get_parameter_string(glow::RENDERER),
                gl.get_parameter_string(glow::VERSION)
            );
            equirectangular_mipmaps_preserve_state_and_base_fallback(&gl, es);
            // Include SDR postprocessing: both HDR flags are false, but the
            // scene destination is RGB10_A2 and must retain all ten bits.
            for (hdr, output_ten_bit, postprocess_active) in [
                (false, false, false),
                (true, true, false),
                (false, false, true),
            ] {
                let ten_bit = cache::uses_ten_bit(hdr, output_ten_bit, postprocess_active);
                assert_eq!(ten_bit, hdr || output_ten_bit || postprocess_active);
                let dst = gl.create_framebuffer().unwrap();
                let tex = gl.create_texture().unwrap();
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    if ten_bit { glow::RGB10_A2 } else { glow::RGBA8 } as i32,
                    8,
                    8,
                    0,
                    glow::RGBA,
                    if ten_bit {
                        glow::UNSIGNED_INT_2_10_10_10_REV
                    } else {
                        glow::UNSIGNED_BYTE
                    },
                    glow::PixelUnpackData::Slice(None),
                );
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(dst));
                gl.framebuffer_texture_2d(
                    glow::FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    Some(tex),
                    0,
                );
                assert_eq!(
                    gl.check_framebuffer_status(glow::FRAMEBUFFER),
                    glow::FRAMEBUFFER_COMPLETE
                );
                gl.disable(glow::SCISSOR_TEST);
                gl.clear_color(0.0, 0.0, 1.0, 1.0);
                gl.clear(glow::COLOR_BUFFER_BIT);
                gl.viewport(1, 2, 3, 4);
                gl.scissor(2, 2, 2, 2);
                gl.enable(glow::SCISSOR_TEST);
                gl.enable(glow::BLEND);
                let mut c = cache::ProceduralFrameCache::default();
                assert!(c.ensure(&gl, 8, 8, ten_bit));
                assert!(!c.valid());
                assert_eq!(
                    gl.get_parameter_texture(glow::TEXTURE_BINDING_2D),
                    Some(tex)
                );
                assert_eq!(
                    gl.get_parameter_framebuffer(glow::DRAW_FRAMEBUFFER_BINDING),
                    Some(dst)
                );
                {
                    let _state = c.begin(&gl);
                    assert!(!gl.is_enabled(glow::SCISSOR_TEST));
                    assert!(!gl.is_enabled(glow::BLEND));
                    gl.clear_color(if ten_bit { 513.0 / 1023.0 } else { 1.0 }, 0.0, 0.0, 1.0);
                    gl.clear(glow::COLOR_BUFFER_BIT);
                }
                c.finish();
                assert!(c.valid());
                assert!(gl.is_enabled(glow::SCISSOR_TEST));
                assert!(gl.is_enabled(glow::BLEND));
                let mut v = [0; 4];
                gl.get_parameter_i32_slice(glow::VIEWPORT, &mut v);
                assert_eq!(v, [1, 2, 3, 4]);
                c.present(&gl, [(0, 0, 4, 8)]);
                assert_eq!(
                    gl.get_parameter_framebuffer(glow::READ_FRAMEBUFFER_BINDING),
                    Some(dst)
                );
                let mut pixels = [0u8; 8 * 8 * 4];
                gl.read_pixels(
                    0,
                    0,
                    8,
                    8,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelPackData::Slice(Some(&mut pixels)),
                );
                for y in 0..8 {
                    for x in 0..8 {
                        let i = (y * 8 + x) * 4;
                        assert_eq!(
                            &pixels[i..i + 4],
                            if (2..4).contains(&x) && (2..4).contains(&y) {
                                if ten_bit {
                                    &[128, 0, 0, 255]
                                } else {
                                    &[255, 0, 0, 255]
                                }
                            } else {
                                &[0, 0, 255, 255]
                            }
                        );
                    }
                }
                if ten_bit {
                    let mut packed = [0u8; 4];
                    gl.read_pixels(
                        2,
                        2,
                        1,
                        1,
                        glow::RGBA,
                        glow::UNSIGNED_INT_2_10_10_10_REV,
                        glow::PixelPackData::Slice(Some(&mut packed)),
                    );
                    assert_eq!(
                        u32::from_ne_bytes(packed) & 1023,
                        513,
                        "10-bit detail survives caching"
                    );
                }
                for _ in 0..120 {
                    assert!(c.ensure(&gl, 8, 8, ten_bit));
                    assert!(c.valid(), "unrelated repairs must not request regeneration");
                    c.present(&gl, [(0, 0, 4, 8)]);
                }
                assert!(c.ensure(&gl, 8, 8, ten_bit));
                assert!(c.valid());
                c.invalidate();
                assert!(!c.valid());
                assert!(c.ensure(&gl, 4, 4, ten_bit));
                assert!(!c.valid());
                assert!(!c.ensure(&gl, u32::MAX, 4, ten_bit));
                assert!(!c.valid());
                assert!(!c.ensure(&gl, u32::MAX, 4, ten_bit));
                assert!(c.ensure(&gl, 8, 8, ten_bit));
                c.cleanup(&gl);
                c.cleanup(&gl);
                assert!(!c.valid());
                assert_eq!(gl.get_error(), glow::NO_ERROR);
                gl.delete_framebuffer(dst);
                gl.delete_texture(tex);
            }
            eglMakeCurrent(d, null, null, null);
            eglDestroyContext(d, ctx);
            eglTerminate(d);
        }
    }
}
