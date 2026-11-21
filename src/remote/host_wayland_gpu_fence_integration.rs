//! Real native frames with one test-only GLES completion result overridden.
use super::*;
use crate::backend::wayland::state::JwmWaylandState;
use crate::remote::{
    client::{
        HardwareDecodeFailure, classify_failure, hardware_decode_failure_clean,
        run_client_with_video_codec,
    },
    gpu_present::fence_test::{Fault, with_fence_fault},
    h264, h264_wire,
    host::{AuthenticatedPeer, authenticate_peer},
    messages::{FrameCodec, ServerHello, StreamCapabilities, decode_source_frame_ack},
    protocol::{MessageKind, ProtocolError},
    source_session::{SourceId, SourceVersion},
    source_wire::{FramePurpose, SourceCatalog, SourceEntry, encode_frame_payload},
};
use std::{ffi::CString, io, net::TcpListener, os::unix::fs::MetadataExt};

const BITSTREAM: &[u8] = include_bytes!("native-video-probe.h264");
const SIZE: (u32, u32) = (256, 256);
const FIRST_SEQUENCE: u64 = 71;

fn drive_until<T>(
    server: &mut Server,
    phase: &str,
    mut observe: impl FnMut(&mut Server) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        server.roundtrip();
        if let Some(result) = observe(server) {
            return result;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {phase}");
        thread::sleep(Duration::from_millis(1));
    }
}

fn serve_frames(listener: TcpListener, key: [u8; 32], healthy_limit: usize) -> Vec<(u64, u64)> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("accept authenticated viewer: {error}"),
        }
    };
    let mut peer = authenticate_peer(stream, &key).unwrap();
    assert_eq!(peer.hello.stream.codec, FrameCodec::H264);
    assert!(!peer.hello.request_input && !peer.hello.request_clipboard);
    peer.reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(8)))
        .unwrap();
    let limits = peer.hello.stream.frame_limits;
    peer.writer
        .write_message(
            MessageKind::HelloAck,
            &ServerHello {
                stream: StreamCapabilities {
                    frame_limits: limits,
                    codec: FrameCodec::H264,
                    ..Default::default()
                },
                pointer_enabled: false,
                keyboard_enabled: false,
                clipboard_enabled: false,
                clipboard_files_enabled: false,
            }
            .encode(),
        )
        .unwrap();
    let source = SourceId(1);
    peer.writer
        .write_message(
            MessageKind::SourceCatalog,
            &SourceCatalog {
                current: SourceVersion { source, epoch: 1 },
                entries: vec![SourceEntry {
                    id: source,
                    label: "owned native fence source".into(),
                }],
            }
            .encode()
            .unwrap(),
        )
        .unwrap();

    let send_frame = |peer: &mut AuthenticatedPeer, sequence| {
        let mut bytes = Vec::new();
        h264_wire::encode_hardware_frame(
            &mut bytes,
            FrameCodec::H264,
            h264_wire::HardwareFrameHeader {
                sequence,
                source_width: SIZE.0 as u16,
                source_height: SIZE.1 as u16,
                encoded_width: SIZE.0 as u16,
                encoded_height: SIZE.1 as u16,
                geometry_epoch: 1,
            },
            BITSTREAM,
            limits,
        )
        .unwrap();
        let payload = encode_frame_payload(1, FramePurpose::Video, &bytes).unwrap();
        peer.writer
            .write_message(MessageKind::Frame, &payload)
            .unwrap();
        peer.writer.flush().unwrap();
    };
    let mut sent = FIRST_SEQUENCE;
    send_frame(&mut peer, sent);
    let mut acks = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "viewer did not close the authenticated connection"
        );
        match peer.reader.read_message() {
            Ok((MessageKind::FrameAck, payload)) => {
                let ack = decode_source_frame_ack(&payload).unwrap();
                assert_eq!(ack, (1, sent), "acknowledgement scope or order changed");
                acks.push(ack);
                assert!(
                    acks.len() <= healthy_limit,
                    "fault did not stop frame acknowledgements"
                );
                sent += 1;
                send_frame(&mut peer, sent);
            }
            Ok((MessageKind::Viewport | MessageKind::Heartbeat | MessageKind::Close, _)) => {}
            Ok((kind, _)) => panic!("unexpected authenticated record: {kind:?}"),
            Err(ProtocolError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                ) =>
            {
                break;
            }
            Err(error) => panic!("viewer stream did not close normally: {error:?}"),
        }
    }
    if healthy_limit != 0 {
        assert!(!acks.is_empty());
    }
    acks
}

fn fence_client_options(
    address: String,
    key: [u8; 32],
    viewer_backend: ViewerBackend,
    display: Option<String>,
) -> ClientOptions {
    ClientOptions {
        stats_json: false,
        control_duration: Default::default(),
        viewer_backend,
        viewer_wayland_display: None,
        wayland_keyboard: false,
        wayland_pointer: false,
        wayland_control: false,
        wayland_gestures: false,
        resumable_control: false,
        wayland_seamless_input: false,
        wayland_popup_pointer: false,
        static_lossless: false,
        wayland_text: None,
        wayland_scroll: false,
        wayland_horizontal_scroll: false,
        resize_source: None,
        follow_source_size: false,
        window_size: Some(WindowSize {
            width: SIZE.0 as u16,
            height: SIZE.1 as u16,
        }),
        window_position: None,
        x11_parent_window: None,
        frame_limits: Default::default(),
        address: address.clone(),
        peer_label: address,
        key: crate::remote::key::KeySource::Bytes(key.to_vec()),
        display,
        view_only: true,
        grab_input: false,
        seamless_input: None,
        clipboard: false,
        clipboard_mode: Default::default(),
        clipboard_direction: Default::default(),
        clipboard_file_limit: 0,
        escape_key: "Escape".into(),
        scaling: crate::remote::ScalingPolicy::Gpu,
    }
}

#[test]
#[ignore = "requires owned Intel Xwayland, working VA-API and JWM_REMOTE_X11_PREFETCH_TARGET=1"]
fn native_x11_first_fence_timeout_joins_prefetch_without_ack() {
    let display = std::env::var("DISPLAY").expect("select the owned Intel Xwayland display");
    let authority = std::env::var_os("XAUTHORITY").expect("select the owned X11 authority");
    assert!(std::path::Path::new(&authority).is_file());
    assert_eq!(
        std::env::var("JWM_REMOTE_X11_PREFETCH_TARGET").as_deref(),
        Ok("1")
    );

    for cleanup_fails in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let key = [0x83; 32];
        let host = thread::spawn(move || serve_frames(listener, key, 0));
        let (result, proof) = with_fence_fault(Fault::FirstDrawTimeout { cleanup_fails }, || {
            run_client_with_video_codec(
                fence_client_options(address, key, ViewerBackend::X11, Some(display.clone())),
                FrameCodec::H264,
            )
        });
        let acks = host.join().unwrap();
        assert!(acks.is_empty());
        assert_eq!(proof.real_signaled_draws, 1);
        assert_eq!(proof.injected_status, Some(glow::TIMEOUT_EXPIRED));
        assert!(proof.x11_prefetch_joined_after_request);
        assert!(proof.exact_frame_held_at_teardown && proof.same_egl_context_destroyed);

        let failure = result.unwrap_err();
        let diagnostic = classify_failure(failure.as_ref());
        assert_eq!(diagnostic.stage, Some("display"));
        let direct_retry = hardware_decode_failure_clean(failure.as_ref());
        let primary = failure.source().unwrap();
        let decoder = if cleanup_fails {
            assert!(!direct_retry);
            assert_eq!(diagnostic.cleanup, "incomplete");
            assert!(
                failure
                    .to_string()
                    .contains("injected X11 RGB preallocation cleanup failure")
            );
            primary.source().unwrap()
        } else {
            assert!(direct_retry);
            assert_ne!(diagnostic.cleanup, "incomplete");
            primary
        };
        assert!(decoder.is::<HardwareDecodeFailure>());
        let wrapper = decoder
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        let timeout = wrapper
            .get_ref()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert_eq!(timeout.kind(), io::ErrorKind::TimedOut);
        assert_eq!(timeout.to_string(), "GLES frame fence did not complete");

        eprintln!(
            "JWM_NATIVE_X11_FIRST_FENCE {}",
            serde_json::json!({
                "faulted_sequence": FIRST_SEQUENCE,
                "frame_acks": acks,
                "real_signaled_draws": proof.real_signaled_draws,
                "injected_status": "GL_TIMEOUT_EXPIRED",
                "prefetch_joined_after_request": proof.x11_prefetch_joined_after_request,
                "same_va_frame_held_at_egl_teardown": proof.exact_frame_held_at_teardown,
                "same_egl_context_destroyed": proof.same_egl_context_destroyed,
                "injected_cleanup_failure": cleanup_fails,
                "jpeg_retry_eligible": direct_retry,
            })
        );
    }
}

#[test]
#[ignore = "requires a real Intel render node via JWM_REMOTE_GPU_PRESENT and working VA-API"]
fn native_wayland_fence_timeout_retires_real_surface_without_ack() {
    run_native_fence_fault(Fault::SecondDrawTimeout, 1);
}

#[test]
#[ignore = "requires real Intel VA-API and JWM_REMOTE_WAYLAND_REUSE_RGB=1"]
fn native_wayland_fence_wait_failure_retires_reused_target_without_ack() {
    assert_eq!(
        std::env::var("JWM_REMOTE_WAYLAND_REUSE_RGB").as_deref(),
        Ok("1")
    );
    run_native_fence_fault(Fault::SecondReusedDrawWaitFailed, 8);
}

fn run_native_fence_fault(fault: Fault, healthy_limit: usize) {
    let node = std::path::PathBuf::from(
        std::env::var_os("JWM_REMOTE_GPU_PRESENT").expect("select an owned Intel render node"),
    );
    let mut gl = HeadlessSmithayGles::on_render_node(&node).expect("explicit Intel hardware GLES");
    let device = node.metadata().unwrap().rdev();
    let mut server = Server::new();
    register_test_presentation(&mut server);
    server.state.image_capture_pending = Some(init_image_copy_capture(&server.display.handle()));
    server.state.ensure_dmabuf_global_with_feedback(
        &server.display.handle(),
        gl.renderer
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .copied(),
        [],
        device,
    );
    let (native, stream) = UnixStream::pair().unwrap();
    server
        .display
        .handle()
        .insert_client(native, Arc::new(JwmClientState::default()))
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let key = [0x83; 32];
    let host = thread::spawn(move || serve_frames(listener, key, healthy_limit));
    let viewer = thread::spawn(move || {
        with_fence_fault(fault, || {
            with_wayland_test_stream(stream, || {
                run_client_with_video_codec(
                    fence_client_options(address, key, ViewerBackend::Wayland, None),
                    FrameCodec::H264,
                )
            })
        })
    });

    let (surface, feedback) = drive_until(&mut server, "healthy native Wayland frame", |server| {
        let (&window, top) = server.state.toplevels.iter().next()?;
        if !top.is_initial_configure_sent() {
            let geometry = server.state.window_geometry.get_mut(&window).unwrap();
            (geometry.w, geometry.h) = SIZE;
            top.with_pending_state(|pending| {
                pending.size = Some((SIZE.0 as i32, SIZE.1 as i32).into());
                pending.states.set(TopState::Activated);
            });
            top.send_configure();
        }
        let surface = top.wl_surface().clone();
        let feedback = take_feedback(&surface);
        (!feedback.is_empty()).then_some((surface, feedback))
    });
    // Positive control uses the same four CPU-reference comparisons as the
    // X11 GPU probe, from the actual committed Wayland buffer before feedback.
    let cpu = h264::vaapi_probe_rgb(
        &CString::new(node.as_os_str().as_encoded_bytes()).unwrap(),
        BITSTREAM,
    )
    .unwrap();
    let mut readback = Readback::new(&mut server, SIZE);
    let mut healthy_frames = 0;
    let mut verify_and_present = |server: &mut Server, feedback: Vec<_>| {
        assert!(
            healthy_frames < healthy_limit,
            "fault did not prevent a commit"
        );
        assert_eq!(feedback.len(), 1);
        assert!(smithay::wayland::dmabuf::get_dmabuf(&committed_buffer(&surface)).is_ok());
        let pixels = readback.pixels(server, &mut gl);
        let mut references = Vec::new();
        for (x, y) in [(16, 16), (233, 16), (16, 231), (233, 231)] {
            let offset = y * SIZE.0 as usize + x;
            let actual = [
                pixels[offset * 4 + 2],
                pixels[offset * 4 + 1],
                pixels[offset * 4],
            ];
            let expected: [u8; 3] = cpu[offset * 3..offset * 3 + 3].try_into().unwrap();
            assert!(!references.contains(&expected));
            assert!(
                actual
                    .iter()
                    .zip(expected)
                    .all(|(gpu, cpu)| gpu.abs_diff(cpu) <= 4),
                "healthy frame color at ({x},{y}): {actual:?}, reference {expected:?}"
            );
            references.push(expected);
        }
        healthy_frames += 1;
        present(server, feedback);
    };
    // A per-surface commit hook also catches any attach+destroy that arrive
    // together before a polling observer sees that attachment.
    let later_commits = Arc::new(AtomicU64::new(0));
    let later = later_commits.clone();
    smithay::wayland::compositor::add_post_commit_hook::<JwmWaylandState, _>(
        &surface,
        move |_, _, _| {
            later.fetch_add(1, Ordering::Relaxed);
        },
    );
    verify_and_present(&mut server, feedback);
    drive_until(
        &mut server,
        "both native fence workers to terminate",
        |server| {
            if surface.is_alive() {
                let feedback = take_feedback(&surface);
                if !feedback.is_empty() {
                    verify_and_present(server, feedback);
                }
            }
            (host.is_finished() && viewer.is_finished()).then_some(())
        },
    );
    let acks = host.join().unwrap();
    let faulted_sequence = FIRST_SEQUENCE + healthy_frames as u64;
    assert_eq!(
        acks,
        (FIRST_SEQUENCE..faulted_sequence)
            .map(|sequence| (1, sequence))
            .collect::<Vec<_>>()
    );
    let (result, proof) = viewer.join().unwrap();
    server.roundtrip();
    assert_eq!(
        later_commits.load(Ordering::Relaxed),
        (healthy_frames - 1) as u64,
        "unresolved GPU draw reached the Wayland surface"
    );
    assert_eq!(proof.real_signaled_draws, healthy_frames + 1);
    assert!(proof.exact_frame_held_at_teardown && proof.same_egl_context_destroyed);
    let (expected_status, error_kind, error_message) = match fault {
        Fault::FirstDrawTimeout { .. } => unreachable!("Wayland requires a healthy frame first"),
        Fault::SecondDrawTimeout => {
            assert_eq!(healthy_frames, 1);
            (
                glow::TIMEOUT_EXPIRED,
                io::ErrorKind::TimedOut,
                "GLES frame fence did not complete",
            )
        }
        Fault::SecondReusedDrawWaitFailed => {
            // Lockstep ACK admission makes the first reused draw one of the
            // healthy frames already checked and presented by this compositor.
            assert_eq!(proof.real_signaled_reused_draws, 2);
            assert!(proof.faulted_reused_target_not_pooled);
            (
                glow::WAIT_FAILED,
                io::ErrorKind::Other,
                "GLES frame fence wait failed",
            )
        }
    };
    assert_eq!(proof.injected_status, Some(expected_status));

    let failure = result.unwrap_err();
    let diagnostic = classify_failure(failure.as_ref());
    assert_eq!(diagnostic.stage, Some("session"));
    assert_ne!(diagnostic.cleanup, "incomplete");
    assert!(!diagnostic.reconnect_certified);
    // Wayland's terminal retry check removes the HardwareDecodeFailure
    // certificate and preserves the original typed I/O cause.
    assert!(!hardware_decode_failure_clean(failure.as_ref()));
    let wrapper = failure
        .source()
        .unwrap()
        .downcast_ref::<io::Error>()
        .unwrap();
    let wait_failure = wrapper
        .get_ref()
        .unwrap()
        .downcast_ref::<io::Error>()
        .unwrap();
    assert_eq!(wait_failure.kind(), error_kind);
    assert_eq!(wait_failure.to_string(), error_message);
    if fault == Fault::SecondReusedDrawWaitFailed {
        eprintln!(
            "JWM_NATIVE_WAYLAND_REUSED_FENCE {}",
            serde_json::json!({
                "render_node_device": device,
                "healthy_presentations": healthy_frames,
                "healthy_acks": acks,
                "faulted_sequence": faulted_sequence,
                "signaled_reused_draws_including_fault": proof.real_signaled_reused_draws,
                "injected_status": "GL_WAIT_FAILED",
                "faulted_target_not_pooled": proof.faulted_reused_target_not_pooled,
                "same_va_frame_held_at_egl_teardown": proof.exact_frame_held_at_teardown,
                "same_egl_context_destroyed": proof.same_egl_context_destroyed,
                "reconnect_certified": diagnostic.reconnect_certified,
                "error": error_message,
            })
        );
    }
}
