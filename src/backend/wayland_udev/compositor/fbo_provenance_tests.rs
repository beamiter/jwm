//! Actual production FBO rendering and native import/feedback ownership.
//! Offscreen GL is not a DRM pageflip or physical-presentation assertion.
use super::super::{CompositorOutputTextureOwnership, FboPresentationCandidate, WaylandCompositor};
use super::*;
use smithay::backend::renderer::gles::ffi;

fn plain(comp: &mut WaylandCompositor) {
    comp.border_enabled = false;
    comp.shadow_enabled = false;
    comp.blur_enabled = false;
    comp.temporal_blur_enabled = false;
    comp.window_tilt_enabled = false;
    comp.inactive_dim = 1.0;
    comp.inactive_desaturate = 0.0;
    comp.edge_glow_enabled = false;
    comp.motion_trail_enabled = false;
    comp.partial_damage_enabled = false;
}
fn candidate(texture: u32, token: u64) -> FboPresentationCandidate {
    FboPresentationCandidate {
        token,
        window: 7,
        texture,
        rect: (8, 6, 16, 12),
    }
}
fn draw(
    comp: &mut WaylandCompositor,
    gl: &ffi::Gles2,
    scene: &[(u64, i32, i32, u32, u32)],
) -> bool {
    comp.render_frame(gl, scene, Some(7), false, false, false, None, false)
}

#[test]
fn fbo_provenance_actual_red_blue_same_allocation_has_distinct_content_receipts() {
    let Some(_headless) = HeadlessGl::new(GlApi::Gles3) else {
        return;
    };
    let gl = ffi::Gles2::load_with(|symbol| egl::get_proc_address(symbol) as *const c_void);
    unsafe {
        let mut comp = WaylandCompositor::new(&gl, 64, 48, false).unwrap();
        plain(&mut comp);
        let texture = create_element_texture(&gl, 16, 12, &[255, 0, 0, 255].repeat(16 * 12));
        insert_opaque_test_window(&mut comp, 7, texture, 16, 12);
        comp.set_presentation_candidate(Some(candidate(texture, 1)));
        assert!(draw(&mut comp, &gl, &[(7, 8, 6, 16, 12)]));
        let first = comp
            .take_presentation_receipt()
            .expect("actual plain opaque quad must produce receipt");
        assert!(
            comp.take_presentation_receipt().is_none(),
            "receipt is taken only once"
        );
        let red = read_fbo_frame(&gl, comp.output_fbo, 64, 48);
        assert_eq!(frame_pixel(&red, 64, 48, 16, 12), [255, 0, 0, 255]);
        gl.BindTexture(ffi::TEXTURE_2D, texture);
        gl.TexSubImage2D(
            ffi::TEXTURE_2D,
            0,
            0,
            0,
            16,
            12,
            ffi::RGBA,
            ffi::UNSIGNED_BYTE,
            [0u8, 0, 255, 255].repeat(16 * 12).as_ptr().cast(),
        );
        gl.BindTexture(ffi::TEXTURE_2D, 0);
        comp.set_presentation_candidate(Some(candidate(texture, 2)));
        assert!(draw(&mut comp, &gl, &[(7, 8, 6, 16, 12)]));
        let second = comp.take_presentation_receipt().unwrap();
        let blue = read_fbo_frame(&gl, comp.output_fbo, 64, 48);
        assert_eq!(frame_pixel(&blue, 64, 48, 16, 12), [0, 0, 255, 255]);
        assert_eq!(first.output_texture, second.output_texture);
        assert_eq!(first.allocation_generation, second.allocation_generation);
        assert!(second.content_serial > first.content_serial);
        assert_eq!(comp.output_content_serial(), Some(second.content_serial));
        comp.release_gpu_resources(&gl, CompositorOutputTextureOwnership::RawCompositor);
        gl.DeleteTextures(1, &texture);
    }
}

#[test]
fn fbo_provenance_hidden_clipped_overlay_and_cached_redraw_never_certify_old_candidate() {
    let Some(_headless) = HeadlessGl::new(GlApi::Gles3) else {
        return;
    };
    let gl = ffi::Gles2::load_with(|symbol| egl::get_proc_address(symbol) as *const c_void);
    unsafe {
        let mut comp = WaylandCompositor::new(&gl, 64, 48, false).unwrap();
        plain(&mut comp);
        let texture = create_element_texture(&gl, 16, 12, &[255, 0, 0, 255].repeat(16 * 12));
        insert_opaque_test_window(&mut comp, 7, texture, 16, 12);
        comp.set_presentation_candidate(Some(candidate(texture, 1)));
        assert!(draw(&mut comp, &gl, &[(7, 8, 6, 16, 12)]));
        assert!(comp.take_presentation_receipt().is_some());
        comp.set_presentation_candidate(Some(candidate(texture, 2)));
        draw(&mut comp, &gl, &[]);
        assert!(
            comp.take_presentation_receipt().is_none(),
            "hidden candidate has no quad"
        );
        let clipped = FboPresentationCandidate {
            rect: (-1, 6, 16, 12),
            ..candidate(texture, 3)
        };
        comp.set_presentation_candidate(Some(clipped));
        draw(&mut comp, &gl, &[(7, -1, 6, 16, 12)]);
        assert!(
            comp.take_presentation_receipt().is_none(),
            "partial visibility is not admitted"
        );
        comp.peek_active = true;
        comp.peek_opacity = 0.5;
        comp.set_presentation_candidate(Some(candidate(texture, 4)));
        draw(&mut comp, &gl, &[(7, 8, 6, 16, 12)]);
        assert!(
            comp.take_presentation_receipt().is_none(),
            "overlay cannot inherit proof"
        );
        comp.peek_active = false;
        comp.peek_opacity = 0.0;
        comp.companion_node = Some(std::sync::Arc::new(
            crate::backend::compositor_common::companion_scene::RasterNode {
                revision: 1,
                pixels: [0u8, 0, 255, 255].repeat(16 * 12),
                width: 16,
                height: 12,
                rect: [8.0, 6.0, 16.0, 12.0],
                accessible_label: "owned opaque proof negative control".into(),
                token: companion_protocol::Token {
                    companion_id: "test".into(),
                    host_id: "owned-test-host".into(),
                    incarnation: 1,
                    boot_id: "test-boot".into(),
                    epoch: 1,
                    surface_id: "opaque-node".into(),
                },
                target: "owned-test-window".into(),
            },
        ));
        comp.set_presentation_candidate(Some(candidate(texture, 5)));
        assert!(draw(&mut comp, &gl, &[(7, 8, 6, 16, 12)]));
        assert!(
            comp.companion_drawn_this_frame,
            "actual production companion draw must execute"
        );
        let covered = read_fbo_frame(&gl, comp.output_fbo, 64, 48);
        assert_eq!(
            frame_pixel(&covered, 64, 48, 16, 12),
            [0, 0, 255, 255],
            "opaque companion really covers red window"
        );
        assert!(
            comp.take_presentation_receipt().is_none(),
            "covered source has no proof via calm-frame guard"
        );
        comp.companion_node = None;
        comp.set_presentation_candidate(None);
        comp.force_full_redraw();
        draw(&mut comp, &gl, &[(7, 8, 6, 16, 12)]);
        assert!(
            comp.take_presentation_receipt().is_none(),
            "cached texture alone is not current import evidence"
        );
        comp.needs_render = false;
        assert!(!draw(&mut comp, &gl, &[(7, 8, 6, 16, 12)]));
        assert!(
            comp.take_presentation_receipt().is_none(),
            "skip clears receipt"
        );
        assert!(comp.output_content_serial().is_none());
        comp.release_gpu_resources(&gl, CompositorOutputTextureOwnership::RawCompositor);
        gl.DeleteTextures(1, &texture);
    }
}

#[cfg(feature = "backend-wayland-udev")]
mod native {
    use super::*;
    use crate::backend::wayland_udev::backend::fbo_presentation;
    use crate::backend::wayland_udev::image_copy_capture::{
        authority_tests::App, wire_test_client::Server,
    };
    use crate::sync_ext::MutexExt;
    use smithay::backend::renderer::buffer_has_alpha;
    use smithay::backend::renderer::utils::RendererSurfaceStateUserData;
    use smithay::wayland::{
        compositor::with_states,
        presentation::{PresentationFeedbackCachedState, PresentationState},
    };
    use std::io::Write;
    use std::os::fd::AsRawFd;

    fn commit(server: &mut Server, app: &mut App, color: [u8; 4], format: u32) -> u32 {
        let before_epoch = server
            .state
            .surface_commit_epoch(app.window)
            .expect("mapped native commit epoch");
        let presentation = app.client.bind("wp_presentation", 1);
        let shm = app.client.bind("wl_shm", 1);
        let fd = nix::sys::memfd::memfd_create(
            c"fbo-proof-test",
            nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
        )
        .unwrap();
        nix::unistd::ftruncate(&fd, 16 * 12 * 4).unwrap();
        let mut file = std::fs::File::from(fd);
        file.write_all(&color.repeat(16 * 12)).unwrap();
        let pool = app.client.new_id();
        app.client
            .request_with_fd_args(shm, 0, &[pool, 16 * 12 * 4], file.as_raw_fd());
        let buffer = app.client.new_id();
        app.client
            .request(pool, 0, &[buffer, 0, 16, 12, 16 * 4, format]);
        let feedback = app.client.new_id();
        app.client
            .request(presentation, 1, &[app.surface, feedback]);
        app.client.request(app.surface, 1, &[buffer, 0, 0]);
        app.client.request(app.surface, 2, &[0, 0, 16, 12]);
        app.client.request(app.surface, 6, &[]);
        // Receiving SCM_RIGHTS can end one dispatch before the later buffer,
        // feedback and attach/commit requests. A real sync after the commit is
        // the fence; neither one dispatch nor a fixed sleep proves it applied.
        let sync = app.client.new_id();
        app.client.request(1, 0, &[sync]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            server.roundtrip();
            let events = app.client.events();
            assert!(
                events
                    .iter()
                    .all(|event| !(event.sender == 1 && event.opcode == 0)),
                "native commit protocol error before sync"
            );
            if events
                .iter()
                .any(|event| event.sender == sync && event.opcode == 0)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "native commit wl_display.sync not completed"
            );
            std::thread::yield_now();
        }
        assert!(
            server
                .state
                .surface_commit_epoch(app.window)
                .is_some_and(|epoch| epoch > before_epoch),
            "sync must fence a new actual commit"
        );
        let surface = server.state.toplevels[&app.window].wl_surface();
        with_states(surface, |states| {
            assert_eq!(
                states
                    .cached_state
                    .get::<PresentationFeedbackCachedState>()
                    .current()
                    .callbacks
                    .len(),
                1,
                "new feedback must be committed before prepare"
            );
            let data = states
                .data_map
                .get::<RendererSurfaceStateUserData>()
                .expect("real on_commit buffer data")
                .lock_safe();
            assert_eq!(
                buffer_has_alpha(&**data.buffer().expect("new committed SHM buffer")),
                Some(format == 0),
                "prepare must inspect the new buffer format, not cached initial ARGB"
            );
        });
        feedback
    }
    fn setup() -> (Server, App) {
        let mut server = Server::new();
        server.state.presentation_state = PresentationState::new::<
            crate::backend::wayland::state::JwmWaylandState,
        >(
            &server.display.handle(), libc::CLOCK_MONOTONIC as u32
        );
        let app = App::with_size_color(&mut server, [0, 0, 255, 255], 16, 12);
        (server, app)
    }
    #[test]
    fn fbo_provenance_real_import_late_commit_discards_a_without_taking_b() {
        let Some(mut gl) = HeadlessSmithayGles::new() else {
            return;
        };
        let (mut server, mut app) = setup();
        let a = commit(&mut server, &mut app, [0, 0, 255, 255], 1); // XRGB red
        let scene = [(app.window.raw(), 8, 6, 16, 12)];
        let prepared = fbo_presentation::prepare(&server.state, &mut gl.renderer, &scene)
            .expect("real opaque native import");
        let c = prepared.candidate();
        let (mut comp, receipt) = gl
            .renderer
            .with_context(|raw| {
                let mut comp = unsafe { WaylandCompositor::new(raw, 64, 48, false) }.unwrap();
                plain(&mut comp);
                insert_opaque_test_window(&mut comp, c.window, c.texture, 16, 12);
                comp.set_presentation_candidate(Some(c));
                assert!(comp.render_frame(
                    raw,
                    &scene,
                    Some(c.window),
                    false,
                    false,
                    false,
                    None,
                    false
                ));
                let pixels = unsafe { read_fbo_frame(raw, comp.output_fbo, 64, 48) };
                assert_eq!(frame_pixel(&pixels, 64, 48, 16, 12), [255, 0, 0, 255]);
                let receipt = comp.take_presentation_receipt().unwrap();
                (comp, receipt)
            })
            .unwrap();
        let b = commit(&mut server, &mut app, [255, 0, 0, 255], 1); // late blue
        assert!(prepared.finish(receipt, &server.state).is_none());
        server.roundtrip();
        let events = app.client.events();
        assert!(
            events
                .iter()
                .any(|event| event.sender == a && event.opcode == 2),
            "late commit must conservatively discard A"
        );
        assert!(
            events.iter().all(|event| event.sender != b),
            "A receipt must neither present nor consume B feedback"
        );
        let surface = server.state.toplevels[&app.window].wl_surface();
        assert_eq!(
            with_states(surface, |states| states
                .cached_state
                .get::<PresentationFeedbackCachedState>()
                .current()
                .callbacks
                .len()),
            1
        );
        let b_prepared =
            fbo_presentation::prepare(&server.state, &mut gl.renderer, &scene).unwrap();
        drop(b_prepared);
        server.roundtrip();
        assert!(
            app.client
                .events()
                .iter()
                .any(|event| event.sender == b && event.opcode == 2),
            "unused prepared ownership drops to discard"
        );
        gl.renderer
            .with_context(|raw| unsafe {
                comp.release_gpu_resources(raw, CompositorOutputTextureOwnership::RawCompositor)
            })
            .unwrap();
    }
    #[test]
    fn fbo_provenance_unsupported_alpha_does_not_reuse_previous_import_as_proof() {
        let Some(mut gl) = HeadlessSmithayGles::new() else {
            return;
        };
        let (mut server, mut app) = setup();
        commit(&mut server, &mut app, [0, 0, 255, 255], 1);
        let scene = [(app.window.raw(), 8, 6, 16, 12)];
        drop(fbo_presentation::prepare(&server.state, &mut gl.renderer, &scene).unwrap());
        let rejected = commit(&mut server, &mut app, [255, 0, 0, 255], 0); // ARGB, even if pixels opaque
        assert!(fbo_presentation::prepare(&server.state, &mut gl.renderer, &scene).is_none());
        server.roundtrip();
        assert!(
            app.client
                .events()
                .iter()
                .all(|event| event.sender != rejected),
            "unsupported cached path cannot claim presentation"
        );
    }
    #[test]
    fn fbo_provenance_real_feedback_proof_consumes_once_or_discards_on_stale_content_and_drop() {
        let Some(mut gl) = HeadlessSmithayGles::new() else {
            return;
        };
        for mode in [
            "consume",
            "stale-content",
            "drop",
            "manager-aba",
            "lock-aba",
        ] {
            let (mut server, mut app) = setup();
            let feedback = commit(&mut server, &mut app, [0, 0, 255, 255], 1);
            let scene = [(app.window.raw(), 8, 6, 16, 12)];
            let prepared =
                fbo_presentation::prepare(&server.state, &mut gl.renderer, &scene).unwrap();
            let c = prepared.candidate();
            let (mut comp, receipt) = gl
                .renderer
                .with_context(|raw| {
                    let mut comp = unsafe { WaylandCompositor::new(raw, 64, 48, false) }.unwrap();
                    plain(&mut comp);
                    insert_opaque_test_window(&mut comp, c.window, c.texture, 16, 12);
                    comp.set_presentation_candidate(Some(c));
                    assert!(comp.render_frame(
                        raw,
                        &scene,
                        Some(c.window),
                        false,
                        false,
                        false,
                        None,
                        false
                    ));
                    let pixels = unsafe { read_fbo_frame(raw, comp.output_fbo, 64, 48) };
                    assert_eq!(frame_pixel(&pixels, 64, 48, 16, 12), [255, 0, 0, 255]);
                    let receipt = comp.take_presentation_receipt().unwrap();
                    assert!(comp.take_presentation_receipt().is_none());
                    (comp, receipt)
                })
                .unwrap();
            let proof = prepared.finish(receipt, &server.state).unwrap();
            let epoch = server.state.surface_commit_epoch(app.window);
            let generation = server.state.presentation_generation;
            if mode == "manager-aba" {
                server.state.set_manager_window_mapped(app.window, false);
                server.state.set_manager_window_mapped(app.window, true);
                assert!(server.state.mapped_windows.contains(&app.window));
                assert_eq!(server.state.surface_commit_epoch(app.window), epoch);
                assert_ne!(server.state.presentation_generation, generation);
            }
            if mode == "lock-aba" {
                server.state.session_lock_supported = true;
                let mut locker = server.connect();
                let manager = locker.bind("ext_session_lock_manager_v1", 1);
                let lock = locker.new_id();
                locker.request(manager, 1, &[lock]);
                server.roundtrip();
                server.roundtrip();
                assert!(server.state.session_locked);
                assert!(
                    locker
                        .events()
                        .iter()
                        .any(|event| event.sender == lock && event.opcode == 0)
                );
                locker.request(lock, 2, &[]);
                server.roundtrip();
                assert!(!server.state.session_locked);
                assert_eq!(server.state.surface_commit_epoch(app.window), epoch);
                assert_ne!(server.state.presentation_generation, generation);
            }
            if mode == "drop" {
                drop(proof);
            } else {
                if mode == "stale-content" {
                    gl.renderer
                        .with_context(|raw| {
                            comp.set_presentation_candidate(None);
                            comp.force_full_redraw();
                            assert!(comp.render_frame(
                                raw,
                                &scene,
                                Some(c.window),
                                false,
                                false,
                                false,
                                None,
                                false
                            ));
                        })
                        .unwrap();
                }
                let callbacks = proof.consume(
                    &server.state,
                    comp.output_texture,
                    comp.output_texture_generation(),
                    comp.output_content_serial(),
                );
                assert_eq!(callbacks.len(), usize::from(mode == "consume"));
                // A sampling proof is not a KMS completion. The owned test
                // discards transferred feedback instead of inventing presented.
                for callback in callbacks {
                    callback.discarded();
                }
            }
            server.roundtrip();
            let events = app.client.events();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.sender == feedback && event.opcode == 2)
                    .count(),
                1
            );
            assert!(
                events
                    .iter()
                    .all(|event| !(event.sender == feedback && event.opcode == 1)),
                "no fake physical-presentation event"
            );
            gl.renderer
                .with_context(|raw| unsafe {
                    comp.release_gpu_resources(raw, CompositorOutputTextureOwnership::RawCompositor)
                })
                .unwrap();
        }
    }
}
