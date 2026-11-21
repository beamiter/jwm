//! Real authenticated 4K H.264 into an owned Intel Wayland compositor.
//! The static, asymmetric source is an import and ACK smoke test.
use super::*;
use crate::backend::wayland::state::JwmWaylandState;
use smithay::backend::{
    allocator::{Buffer as _, Fourcc},
    renderer::{
        Bind, Color32F, ExportMem, Offscreen, buffer_dimensions,
        damage::OutputDamageTracker,
        element::{
            Element, Kind as ElementKind,
            surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
        },
        gles::{GlesRenderbuffer, GlesRenderer},
    },
};
use smithay::output::{Mode, Output, PhysicalProperties, Scale as OutputScale, Subpixel};
use smithay::utils::{Buffer as BufferCoord, Clock, Monotonic, Rectangle, Transform};
use smithay::wayland::dmabuf::get_dmabuf;
use std::os::unix::fs::{FileExt, MetadataExt};

#[path = "host_wayland_gpu_4k_marker_integration.rs"]
mod marker;
#[path = "host_wayland_gpu_xwayland_integration.rs"]
mod xwayland;

const WIDTH: u32 = 3840;
const HEIGHT: u32 = 2160;
const ROI_ORIGIN: u32 = 16;
const ROI_SIZE: u32 = 96;
const QUADRANTS: [[bool; 3]; 4] = [
    [true, false, false],
    [false, true, false],
    [false, false, true],
    [true, true, false],
];

/// Drive a real configure and commit an asymmetric 96×96 patch near the top
/// left of a full-resolution source. The opposite corner and rows are gray.
fn install_source_pattern(server: &mut Server, app: &mut App) {
    let geometry = server.state.window_geometry.get_mut(&app.window).unwrap();
    (geometry.w, geometry.h) = (WIDTH, HEIGHT);
    let toplevel = &server.state.toplevels[&app.window];
    toplevel.with_pending_state(|state| state.size = Some((WIDTH as i32, HEIGHT as i32).into()));
    toplevel.send_configure();
    server.roundtrip();
    let serial = app
        .client
        .events()
        .into_iter()
        .filter(|event| event.sender == app.xdg && event.opcode == 0)
        .next_back()
        .expect("owned source 4K configure")
        .args[0];
    app.client.request(app.xdg, 4, &[serial]);

    let (buffer, file) = shm(server, &mut app.client, [112, 112, 112, 255], WIDTH, HEIGHT);
    let component = |high| if high { 240 } else { 16 };
    for (index, [red, green, blue]) in QUADRANTS.into_iter().enumerate() {
        let half = ROI_SIZE / 2;
        let left = ROI_ORIGIN + (index as u32 % 2) * half;
        let top = ROI_ORIGIN + (index as u32 / 2) * half;
        let bgra = [component(blue), component(green), component(red), 255];
        let row = bgra.repeat(half as usize);
        for y in top..top + half {
            file.write_all_at(&row, u64::from((y * WIDTH + left) * 4))
                .unwrap();
        }
    }
    app.client.request(app.surface, 1, &[buffer, 0, 0]);
    app.client.request(app.surface, 9, &[0, 0, WIDTH, HEIGHT]);
    app.client.request(app.surface, 6, &[]);
    server.roundtrip();
    assert!(
        app.client
            .events()
            .iter()
            .all(|event| !(event.sender == 1 && event.opcode == 0)),
        "source accepted the real 4K configure and SHM buffer"
    );
    let surface = server.state.toplevels[&app.window].wl_surface();
    assert_eq!(
        with_renderer_surface_state(surface, |state| state.surface_size()).flatten(),
        Some((WIDTH as i32, HEIGHT as i32).into())
    );
}

/// An owned 4K output. It draws the current root at scale 1 and reads only
/// the marker region. No client request dispatch intervenes in that proof.
struct Private4kOutput {
    output: Output,
    tracker: OutputDamageTracker,
    target: GlesRenderbuffer,
    clock: Clock<Monotonic>,
    sequence: u64,
}

impl Private4kOutput {
    fn new(server: &mut Server, renderer: &mut GlesRenderer) -> Self {
        let output = Output::new(
            "private-native-4k".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "JWM".into(),
                model: "owned-4k-test".into(),
                serial_number: "0".into(),
            },
        );
        let size = (WIDTH as i32, HEIGHT as i32);
        let mode = Mode {
            size: size.into(),
            refresh: 60_000,
        };
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(OutputScale::Integer(1)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        output.create_global::<JwmWaylandState>(&server.display.handle());
        Self {
            tracker: OutputDamageTracker::from_output(&output),
            target: Offscreen::create_buffer(renderer, Fourcc::Abgr8888, size.into())
                .expect("allocate one private full-resolution GLES target"),
            output,
            clock: Clock::new(),
            sequence: 0,
        }
    }

    fn present(
        &mut self,
        server: &mut Server,
        renderer: &mut GlesRenderer,
        surface: &WlSurface,
        inspect: impl FnOnce(&[u8]),
    ) -> Option<u64> {
        self.output.enter(surface);
        let feedback = take_feedback(surface);
        if feedback.is_empty() {
            return None;
        }
        // Smithay discards superseded feedback when another explicit commit
        // arrives. This feedback and renderer state belong to the current root.
        let size = (WIDTH as i32, HEIGHT as i32);
        with_renderer_surface_state(surface, |state| {
            let buffer = state.buffer().expect("current frame has a buffer");
            assert_eq!(state.buffer_scale(), 1);
            assert_eq!(state.buffer_transform(), Transform::Normal);
            assert_eq!(state.buffer_size(), Some(size.into()));
            assert_eq!(state.surface_size(), Some(size.into()));
            assert_eq!(buffer_dimensions(buffer), Some(size.into()));
            let dmabuf = get_dmabuf(buffer).expect("current 4K attachment is a native DMA-BUF");
            assert_eq!(dmabuf.format().code, Fourcc::Xrgb8888);
        })
        .expect("current surface has renderer state");

        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            render_elements_from_surface_tree(
                renderer,
                surface,
                (0, 0),
                1.0f64,
                1.0,
                ElementKind::Unspecified,
            );
        assert_eq!(elements.len(), 1, "one actual imported root element");
        let root = &elements[0];
        assert_eq!(root.geometry(1.0.into()), Rectangle::from_size(size.into()));
        assert_eq!(
            root.src(),
            Rectangle::from_size((f64::from(WIDTH), f64::from(HEIGHT)).into())
        );
        let mut target = renderer
            .bind(&mut self.target)
            .expect("bind owned 4K target");
        let result = self
            .tracker
            .render_output(renderer, &mut target, 0, &elements, Color32F::BLACK)
            .expect("compose actual committed 4K DMA-BUF");
        assert!(result.damage.is_some());
        assert_eq!(
            result
                .states
                .element_render_state(root.id().clone())
                .unwrap()
                .visible_area,
            (WIDTH * HEIGHT) as usize
        );

        // The normal offscreen projection maps logical y=0 to GL row 0.
        // The asymmetric source checks this against the actual mapped bytes.
        let roi: Rectangle<i32, BufferCoord> = Rectangle::new(
            (ROI_ORIGIN as i32, ROI_ORIGIN as i32).into(),
            (ROI_SIZE as i32, ROI_SIZE as i32).into(),
        );
        let mapping = renderer
            .copy_framebuffer(&target, roi, Fourcc::Abgr8888)
            .expect("read only the 96×96 region from the completed 4K target");
        let rgba = renderer
            .map_texture(&mapping)
            .expect("map only the region PBO");
        let completed = self.clock.now();
        assert_eq!(rgba.len(), (ROI_SIZE * ROI_SIZE * 4) as usize);
        inspect(rgba);

        self.sequence += 1;
        for callback in feedback {
            callback.presented(
                &self.output,
                completed,
                Refresh::Unknown,
                self.sequence,
                Kind::empty(),
            );
        }
        with_states(surface, |states| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(completed.as_millis());
            }
        });
        server
            .display
            .flush_clients()
            .expect("flush the real timed Wayland feedback");
        Some(completed.as_micros())
    }
}

fn check_source_pattern(rgba: &[u8]) -> [[u8; 4]; 4] {
    let samples = std::array::from_fn(|index| {
        let x = ROI_SIZE / 4 + (index as u32 % 2) * (ROI_SIZE / 2);
        let y = ROI_SIZE / 4 + (index as u32 / 2) * (ROI_SIZE / 2);
        let pos = ((y * ROI_SIZE + x) * 4) as usize;
        <[u8; 4]>::try_from(&rgba[pos..pos + 4]).unwrap()
    });
    for (sample, quadrants) in samples.into_iter().zip(QUADRANTS) {
        assert!(
            sample
                .into_iter()
                .zip(quadrants)
                .all(|(value, high)| { if high { value >= 190 } else { value <= 65 } }),
            "the rendered ROI is upright and unscaled after lossy H.264: {samples:?}"
        );
    }
    samples
}

fn exact_h264(frame: &Value) -> bool {
    frame["codec"] == "H.264"
        && frame["source_dimensions"] == json!([WIDTH, HEIGHT])
        && frame["encoded_dimensions"] == json!([WIDTH, HEIGHT])
}

#[test]
#[ignore = "requires an explicit Intel render node, VA-API, and the real CLI"]
fn native_cli_wayland_gpu_viewer_presents_owned_4k_region() {
    let node = PathBuf::from(
        std::env::var_os("JWM_TEST_WAYLAND_GPU_CAPTURE_NODE").expect("Intel render node required"),
    );
    let mut gl = HeadlessSmithayGles::on_render_node(&node).expect("explicit hardware GBM/EGL");
    let device = node.metadata().unwrap().rdev();
    let formats = gl
        .renderer
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .copied()
        .collect::<Vec<_>>();
    let mut fixture =
        CliFixture::start_with_host(false, 2, None, None, Some(false), false, |host| {
            host.args(["--allow-h264", "--stats-json", "--fps", "10"])
                .env("JWM_REMOTE_GPU_CAPTURE", &node);
        });
    install_source_pattern(&mut fixture.native, &mut fixture.target);
    let mut output = Private4kOutput::new(&mut fixture.local, &mut gl.renderer);
    let mut session = Session::start_fixture_with_viewer(
        &mut gl,
        fixture,
        false,
        false,
        |fixture| {
            fixture.viewer_initial_size = (WIDTH, HEIGHT);
            fixture.hold_feedback = true;
            for server in [&mut fixture.native, &mut fixture.local] {
                server.state.ensure_dmabuf_global_with_feedback(
                    &server.display.handle(),
                    formats.iter().copied(),
                    [],
                    device,
                );
            }
        },
        |viewer| {
            viewer
                .args(["--scaling", "gpu"])
                .env("JWM_REMOTE_GPU_PRESENT", &node);
        },
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut samples = None;
    let mut completed_micros = None;
    let (host, viewer) = loop {
        let fixture = &mut session.fixture;
        fixture.tick(&mut gl);
        if let Some(surface) = fixture.surface.as_ref()
            && let Some(completed) =
                output.present(&mut fixture.local, &mut gl.renderer, surface, |rgba| {
                    samples = Some(check_source_pattern(rgba));
                })
        {
            fixture.last_dmabuf = Some(true);
            completed_micros = Some(completed);
        }
        let host = latest_host_stats(fixture);
        let events = records(&session.events);
        if let Some(host) = host.as_ref()
            && samples.is_some()
            && native_host_capture(host)
            && exact_h264(&host["last_sent_frame"])
            && let Some(viewer) = events.iter().rev().find_map(|row| {
                let metrics = &row["metrics"];
                let decoded = &metrics["last_decoded_frame"];
                let sent = &host["last_sent_frame"];
                (row["state"] == "viewing"
                    && row["catalog"]["source_id"] == 1
                    && metrics["session_id"] == host["session_id"]
                    && decoded["source_epoch"] == sent["source_epoch"]
                    && decoded["geometry_epoch"] == sent["geometry_epoch"]
                    && exact_h264(decoded)
                    && native_viewer_presentation(fixture, metrics))
                .then_some(metrics)
            })
        {
            break (host.clone(), viewer.clone());
        }
        assert!(
            Instant::now() < deadline,
            "4K DMA-BUF/H.264 completion timed out; presented={} host={} viewer={} last={:?}",
            output.sequence,
            fixture.host.text(),
            fixture.viewer.as_ref().unwrap().text(),
            events.last()
        );
        thread::sleep(Duration::from_millis(1));
    };
    eprintln!(
        "JWM_NATIVE_WAYLAND_4K {}",
        json!({
            "render_node_device": device,
            "roi": [ROI_ORIGIN, ROI_ORIGIN, ROI_SIZE, ROI_SIZE],
            "roi_bytes": ROI_SIZE * ROI_SIZE * 4,
            "samples": samples.unwrap(),
            "compositor_completions": output.sequence,
            "last_completion_monotonic_us": completed_micros.unwrap(),
            "host": host,
            "viewer": viewer,
        })
    );

    session.send(json!({"command":"disconnect","request":1}));
    session
        .fixture
        .wait(&mut gl, "4K smoke closes normally", CliFixture::exited);
    assert!(session.fixture.host.status.unwrap().success());
    assert!(
        session
            .fixture
            .viewer
            .as_ref()
            .unwrap()
            .status
            .unwrap()
            .success()
    );
}
