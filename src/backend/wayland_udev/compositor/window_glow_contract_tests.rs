use super::*;
use crate::backend::wayland_udev::compositor::{
    CompositorOutputTextureOwnership, WaylandCompositor,
};
use smithay::backend::renderer::gles::ffi;

struct TestConfig(std::path::PathBuf);
impl TestConfig {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "jwm-glow-{}-{}.toml",
            std::process::id(),
            rand::random::<u64>()
        ));
        crate::config::Config::default()
            .save_to_file(&path)
            .unwrap();
        Self(path)
    }

    fn publish(&self, changes: &[(&str, serde_json::Value)]) -> crate::config::Config {
        let mut table: toml::Value =
            toml::from_str(&std::fs::read_to_string(&self.0).unwrap()).unwrap();
        for (key, value) in changes {
            let (section, field) = key.split_once('.').unwrap();
            table[section][field] = toml::Value::try_from(value).unwrap();
        }
        std::fs::write(&self.0, toml::to_string(&table).unwrap()).unwrap();
        crate::config::Config::load_from_file(&self.0).unwrap()
    }
}
impl Drop for TestConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn glow_frames(api: GlApi, vertex: &str, fragment: &str) -> Option<Vec<Vec<u8>>> {
    let context = HeadlessGl::new(api)?;
    let gl = &context.gl;
    let program = link(gl, vertex, fragment).expect("border glow shaders link");
    let mut frames = Vec::new();
    for radius in [0.0, 8.0] {
        for spread in [4, 12] {
            let (w, h) = (48 + 2 * spread, 32 + 2 * spread);
            for alpha in [0.0, 0.4, 1.0] {
                let frame = render_quad_frame(gl, program, [0; 4], w, h, |gl| unsafe {
                    let u = |name: &str| gl.get_uniform_location(program, name);
                    gl.uniform_matrix_4_f32_slice(
                        u("u_projection").as_ref(),
                        false,
                        &ortho(w as f32, h as f32),
                    );
                    gl.uniform_4_f32(u("u_rect").as_ref(), 0.0, 0.0, w as f32, h as f32);
                    gl.uniform_2_f32(u("u_size").as_ref(), 48.0, 32.0);
                    gl.uniform_1_f32(u("u_radius").as_ref(), radius);
                    gl.uniform_1_f32(u("u_radius_top").as_ref(), radius);
                    gl.uniform_1_f32(u("u_border_width").as_ref(), -(spread as f32));
                    gl.uniform_4_f32(u("u_border_color").as_ref(), 0.25, 0.5, 1.0, alpha);
                    gl.uniform_1_i32(u("u_scene_linear").as_ref(), 0);
                });
                let pixel = |x, y| frame_pixel(&frame, w as usize, h as usize, x, y);
                assert_eq!(
                    pixel(w as usize / 2, h as usize / 2),
                    [0; 4],
                    "no interior fill"
                );
                let mut coverage = 0;
                for p in frame.chunks_exact(4) {
                    // A straight-alpha regression violates these relations
                    // throughout the halo, rather than only at one golden pixel.
                    assert!(p[0].abs_diff((f32::from(p[3]) * 0.25).round() as u8) <= 1);
                    assert!(p[1].abs_diff((f32::from(p[3]) * 0.5).round() as u8) <= 1);
                    assert!(p[2].abs_diff(p[3]) <= 1);
                    coverage += usize::from(p[3] > 0);
                }
                if alpha == 0.0 {
                    assert_eq!(coverage, 0);
                } else {
                    assert!(coverage > 0);
                    // At the middle of a straight side fwidth(SDF)=1;
                    // this independent falloff oracle avoids corner derivative
                    // differences between real GPU implementations.
                    let x = spread - 2;
                    let y = spread + 16;
                    let dist = 1.5_f32;
                    let normalized = dist / spread as f32;
                    let halo = (-4.0 * normalized * normalized).exp2()
                        * (1.0 - smoothstep_oracle(0.72, 1.0, normalized));
                    let core = 1.0 - smoothstep_oracle(0.0, 1.75, dist);
                    let uv = ((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
                    let gaussian = |cx: f32, cy: f32, sx: f32, sy: f32| {
                        (-((uv.0 - cx) / sx).powi(2) - ((uv.1 - cy) / sy).powi(2)).exp()
                    };
                    let energy = (0.42
                        + 0.36 * uv.0
                        + 0.22 * (1.0 - uv.1)
                        + 0.68 * gaussian(0.82, 0.08, 0.24, 0.18)
                        + 0.24 * gaussian(0.08, 0.82, 0.30, 0.26))
                    .clamp(0.25, 1.65);
                    let expected = (alpha * halo.max(core * 0.85) * energy * 255.0).round() as u8;
                    assert!(
                        pixel(x as usize, y as usize)[3].abs_diff(expected) <= 3,
                        "SDF falloff: spread={spread}, radius={radius}, alpha={alpha}"
                    );
                }
                frames.push(frame);
            }
        }
    }
    unsafe {
        gl.delete_program(program);
    }
    Some(frames)
}

#[test]
fn border_glow_sdf_and_premultiplied_alpha_match_gles_and_gl() {
    use crate::backend::wayland_udev::compositor::shaders as wayland;
    let Some(wayland_frames) = glow_frames(
        GlApi::Gles3,
        wayland::VERTEX_SHADER,
        wayland::BORDER_FRAGMENT_SHADER,
    ) else {
        return;
    };
    #[cfg(feature = "x11-backends")]
    {
        use crate::backend::x11::compositor::shaders as x11;
        let Some(x11_frames) = glow_frames(
            GlApi::GlCore33,
            x11::VERTEX_SHADER,
            x11::BORDER_FRAGMENT_SHADER,
        ) else {
            return;
        };
        for (wayland, x11) in wayland_frames.iter().zip(x11_frames) {
            assert_eq!(wayland.len(), x11.len());
            assert!(
                wayland.iter().zip(x11).all(|(a, b)| a.abs_diff(b) <= 2),
                "GL/GLES border glow pixels diverged"
            );
        }
    }
    #[cfg(not(feature = "x11-backends"))]
    let _ = wayland_frames;
}

#[test]
fn border_glow_partial_damage_focus_and_reload() {
    // CONFIG is process-global. Isolate intentional hot reloads from the
    // thousands of concurrent policy tests rather than racing their defaults.
    let module = module_path!().split_once("::").unwrap().1;
    let name = format!("{module}::isolated_border_glow_lifecycle");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--ignored", "--nocapture"])
        .output()
        .expect("spawn isolated GL contract");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
        "the isolated lifecycle test must actually execute: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
#[ignore = "run in an isolated process by border_glow_partial_damage_focus_and_reload"]
fn isolated_border_glow_lifecycle() {
    use crate::config::CONFIG;
    let Some(_context) = HeadlessGl::new(GlApi::Gles3) else {
        return;
    };
    let gl = ffi::Gles2::load_with(|s| egl::get_proc_address(s) as *const c_void);
    let file = TestConfig::new();
    let config = file.publish(&[
        ("behavior.border_glow_enabled", serde_json::json!(true)),
        ("behavior.border_glow_focused_only", serde_json::json!(true)),
        ("behavior.border_glow_radius", serde_json::json!(8)),
        (
            "behavior.border_glow_color",
            serde_json::json!([1.0, 0.3, 0.1, 0.7]),
        ),
        ("behavior.shadow_enabled", serde_json::json!(false)),
        ("behavior.border_enabled", serde_json::json!(false)),
        ("behavior.window_animation", serde_json::json!(false)),
        ("behavior.fading", serde_json::json!(false)),
        ("behavior.focus_highlight", serde_json::json!(false)),
        ("behavior.wallpaper", serde_json::json!("")),
        ("behavior.inactive_opacity", serde_json::json!(1.0)),
    ]);
    CONFIG.store(std::sync::Arc::new(config));
    const W: i32 = 256;
    const H: i32 = 128;
    unsafe {
        let mut compositor = WaylandCompositor::new(&gl, W as u32, H as u32, false).unwrap();
        compositor.apply_config();
        compositor.set_partial_damage(true);
        let texture = create_element_texture(&gl, 48, 32, &[180, 180, 180, 255].repeat(48 * 32));
        insert_opaque_test_window(&mut compositor, 1, texture, 48, 32);
        insert_opaque_test_window(&mut compositor, 2, texture, 48, 32);
        let mut scene = [(1, 32, 32, 48, 32), (2, 160, 40, 48, 32)];
        let render = |c: &mut WaylandCompositor, scene: &[_], focus| {
            c.needs_render = true;
            assert!(c.render_frame(&gl, scene, focus, false, false, false, None, false));
            read_fbo_frame(&gl, c.output_fbo, W, H)
        };
        compositor.force_full_redraw();
        let initial = render(&mut compositor, &scene, Some(1));
        assert!(
            !compositor.render_frame(&gl, &scene, Some(1), false, false, false, None, false),
            "a settled glow must not keep animating: needs={}, active={}, windows={:?}",
            compositor.needs_render,
            compositor.has_active_animations(),
            compositor
                .windows
                .values()
                .map(|w| (
                    w.fade_opacity,
                    w.anim_scale,
                    w.anim_scale_target,
                    w.ripple_active
                ))
                .collect::<Vec<_>>()
        );

        // A sentinel outside the moved window's damage proves this really
        // took the partial path. Compare every other pixel to a fresh full draw.
        gl.BindFramebuffer(ffi::FRAMEBUFFER, compositor.output_fbo);
        gl.Enable(ffi::SCISSOR_TEST);
        gl.Scissor(240, H - 112, 2, 2);
        gl.ClearColor(0.0, 1.0, 0.0, 1.0);
        gl.Clear(ffi::COLOR_BUFFER_BIT);
        gl.Disable(ffi::SCISSOR_TEST);
        scene[0].1 += 8;
        let partial = render(&mut compositor, &scene, Some(1));
        assert_eq!(
            frame_pixel(&partial, W as usize, H as usize, 240, 110),
            [0, 255, 0, 255]
        );
        let mut scissor = [0; 4];
        gl.GetIntegerv(ffi::SCISSOR_BOX, scissor.as_mut_ptr());
        let repaired_pixels = scissor[2] * scissor[3];
        assert!(
            repaired_pixels > 0 && repaired_pixels < W * H / 2,
            "damage={scissor:?}"
        );
        eprintln!(
            "border_glow moved-window repair: {repaired_pixels}/{} pixels, scissor={scissor:?}",
            W * H
        );
        compositor.force_full_redraw();
        let full = render(&mut compositor, &scene, Some(1));
        for y in 0..H as usize {
            for x in 0..W as usize {
                if (240..242).contains(&x) && (110..112).contains(&y) {
                    continue;
                }
                let a = frame_pixel(&partial, W as usize, H as usize, x, y);
                let b = frame_pixel(&full, W as usize, H as usize, x, y);
                assert!(
                    a.into_iter().zip(b).all(|(a, b)| a.abs_diff(b) <= 1),
                    "stale glow at {x},{y}"
                );
            }
        }
        let focused_other = render(&mut compositor, &scene, Some(2));
        assert_ne!(focused_other, full, "focus must move the glow");

        let config = file.publish(&[("behavior.border_glow_enabled", serde_json::json!(false))]);
        CONFIG.store(std::sync::Arc::new(config));
        compositor.apply_config();
        let disabled = render(&mut compositor, &scene, Some(2));
        assert_ne!(disabled, focused_other, "hot disable removes glow");
        compositor.force_full_redraw();
        assert_eq!(
            disabled,
            render(&mut compositor, &scene, Some(2)),
            "no stale hot-reload cache"
        );

        let config = file.publish(&[("behavior.border_glow_enabled", serde_json::json!(true))]);
        CONFIG.store(std::sync::Arc::new(config));
        compositor.windows.get_mut(&2).unwrap().is_fullscreen = true;
        compositor.apply_config();
        assert_eq!(
            disabled,
            render(&mut compositor, &scene, Some(2)),
            "fullscreen suppresses glow"
        );
        // Change the wallpaper under an actual frosted client while glow is
        // enabled, then hot-reload the glow color. Fresh full frames are the
        // oracle for retained blur/backdrop state, not synthetic shader output.
        compositor.windows.get_mut(&2).unwrap().is_fullscreen = false;
        compositor.windows.get_mut(&1).unwrap().is_frosted = true;
        compositor.blur_enabled = true;
        let wallpaper = create_element_texture(&gl, 1, 1, &[40, 80, 160, 255]);
        compositor.wallpaper_texture = Some(wallpaper);
        compositor.wallpaper_img_w = 1;
        compositor.wallpaper_img_h = 1;
        compositor.force_full_redraw();
        let blue = render(&mut compositor, &scene, Some(1));
        gl.BindTexture(ffi::TEXTURE_2D, wallpaper);
        gl.TexSubImage2D(
            ffi::TEXTURE_2D,
            0,
            0,
            0,
            1,
            1,
            ffi::RGBA,
            ffi::UNSIGNED_BYTE,
            [160u8, 80, 40, 255].as_ptr().cast(),
        );
        compositor.force_full_redraw();
        let amber = render(&mut compositor, &scene, Some(1));
        assert_ne!(
            blue, amber,
            "wallpaper updates reach the glass/glow composition"
        );
        let config = file.publish(&[(
            "behavior.border_glow_color",
            serde_json::json!([0.1, 1.0, 0.3, 0.7]),
        )]);
        CONFIG.store(std::sync::Arc::new(config));
        compositor.apply_config();
        let recolored = render(&mut compositor, &scene, Some(1));
        assert_ne!(amber, recolored, "reload invalidates the glow appearance");
        compositor.force_full_redraw();
        assert_eq!(
            recolored,
            render(&mut compositor, &scene, Some(1)),
            "reloaded caches match a full frame"
        );
        assert_ne!(initial, full, "the move changes real pixels");
        gl.DeleteTextures(1, &texture);
        assert!(
            compositor.release_gpu_resources(&gl, CompositorOutputTextureOwnership::RawCompositor)
        );
    }
}
