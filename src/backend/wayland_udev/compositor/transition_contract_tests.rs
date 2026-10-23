//! Exercise the real transition dispatcher, render targets and GL state at
//! exact frame times. Pixels outside the workarea are the chrome oracle.
use super::*;
use crate::backend::compositor_common::transitions::TransitionMode;
use crate::backend::wayland_udev::compositor::{
    CompositorOutputTextureOwnership, WaylandCompositor,
};
use smithay::backend::renderer::gles::ffi;
use std::time::{Duration, Instant};

#[test]
fn every_transition_frame_preserves_chrome_and_other_outputs() {
    let Some(_context) = HeadlessGl::new(GlApi::Gles3) else {
        return;
    };
    let gl = ffi::Gles2::load_with(|s| egl::get_proc_address(s) as *const c_void);
    const W: i32 = 96;
    const H: i32 = 72;
    let projection = ortho(W as f32, H as f32);
    let modes = [
        TransitionMode::Slide,
        TransitionMode::Fade,
        TransitionMode::Cube,
        TransitionMode::CoverFlow,
        TransitionMode::Flip,
        TransitionMode::Zoom,
        TransitionMode::Stack,
        TransitionMode::Blinds,
        TransitionMode::Helix,
        TransitionMode::Portal,
        TransitionMode::Book,
    ];
    // A cropped second output: top, bottom, left, right and hidden bar;
    // then a partially off-canvas output (negative desktop coordinates).
    let workareas = [
        (48, 12, 48, 60),
        (48, 0, 48, 60),
        (60, 0, 36, 72),
        (48, 0, 36, 72),
        (48, 0, 48, 72),
        (-12, -8, 48, 60),
    ];
    unsafe {
        let mut compositor = WaylandCompositor::new(&gl, W as u32, H as u32, false).unwrap();
        for mode in modes {
            compositor.transition_mode = mode;
            for (rect, direction) in workareas
                .into_iter()
                .flat_map(|rect| [-1, 1].map(|d| (rect, d)))
            {
                // The old scene is red; the incoming scene/chrome is green.
                gl.BindFramebuffer(ffi::FRAMEBUFFER, compositor.output_fbo);
                gl.Disable(ffi::SCISSOR_TEST);
                gl.ClearColor(1.0, 0.0, 0.0, 1.0);
                gl.Clear(ffi::COLOR_BUFFER_BIT);
                compositor.capture_transition_snapshot(&gl);
                compositor.notify_tag_switch(Duration::from_secs(1), direction, 0, rect);
                let start = Instant::now();
                compositor.transition_start = Some(start);
                let duration = compositor.transition_duration;
                let mut changed = false;
                for fraction in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 0.999, 1.0] {
                    gl.BindFramebuffer(ffi::FRAMEBUFFER, compositor.output_fbo);
                    gl.Disable(ffi::SCISSOR_TEST);
                    gl.Viewport(0, 0, W, H);
                    gl.Enable(ffi::BLEND);
                    gl.BlendFunc(ffi::ONE, ffi::ONE_MINUS_SRC_ALPHA);
                    gl.ClearColor(0.0, 1.0, 0.0, 1.0);
                    gl.Clear(ffi::COLOR_BUFFER_BIT);
                    compositor.render_transition_at(
                        &gl,
                        &projection,
                        false,
                        start + duration.mul_f64(fraction),
                    );
                    let frame = read_fbo_frame(&gl, compositor.output_fbo, W, H);
                    for y in 0..H {
                        for x in 0..W {
                            let inside = x >= rect.0
                                && x < rect.0 + rect.2 as i32
                                && y >= rect.1
                                && y < rect.1 + rect.3 as i32;
                            let pixel =
                                frame_pixel(&frame, W as usize, H as usize, x as usize, y as usize);
                            if !inside || fraction == 1.0 {
                                assert_eq!(
                                    pixel,
                                    [0, 255, 0, 255],
                                    "{mode:?}, rect={rect:?}, t={fraction}, pixel=({x},{y})"
                                );
                            } else {
                                changed |= pixel != [0, 255, 0, 255];
                            }
                        }
                    }
                    let mut viewport = [0; 4];
                    gl.GetIntegerv(ffi::VIEWPORT, viewport.as_mut_ptr());
                    assert_eq!(viewport, [0, 0, W, H]);
                    assert_eq!(gl.IsEnabled(ffi::SCISSOR_TEST), ffi::FALSE as u8);
                    assert_eq!(gl.GetError(), ffi::NO_ERROR, "{mode:?} t={fraction}");
                }
                assert!(changed, "{mode:?} must actually draw a transition");
                assert!(!compositor.transition_active, "completion retires {mode:?}");
            }
        }
        assert!(
            compositor.release_gpu_resources(&gl, CompositorOutputTextureOwnership::RawCompositor)
        );
    }
}
