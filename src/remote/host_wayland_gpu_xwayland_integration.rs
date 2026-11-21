//! An owned, render-node-only compositor for the rootful 4K Xwayland fixture.
use super::*;

const REFRESH_INTERVAL: Duration = Duration::from_nanos(16_666_667);

#[test]
#[ignore = "requires an owned socket directory and explicit Intel render node"]
fn serve_owned_intel_xwayland_output() {
    let socket = PathBuf::from(std::env::var_os("JWM_TEST_XWAYLAND_SOCKET").unwrap());
    let report = PathBuf::from(std::env::var_os("JWM_TEST_XWAYLAND_REPORT").unwrap());
    assert!(socket.is_absolute() && report.is_absolute() && !socket.exists() && !report.exists());
    assert_eq!(socket.parent(), report.parent());
    let node = PathBuf::from(std::env::var_os("JWM_TEST_XWAYLAND_RENDER_NODE").unwrap());
    let metadata = node.metadata().unwrap();
    let mut gl = HeadlessSmithayGles::on_render_node(&node).expect("explicit Intel hardware GLES");
    let listener = UnixListener::bind(socket).expect("owned Wayland listener");
    listener.set_nonblocking(true).unwrap();
    let mut server = Server::new();
    register_test_presentation(&mut server);
    server.state.ensure_dmabuf_global_with_feedback(
        &server.display.handle(),
        gl.renderer
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .copied(),
        [],
        metadata.rdev(),
    );
    let mut output = Private4kOutput::new(&mut server, &mut gl.renderer);
    let (stopped, stop) = mpsc::channel();
    thread::spawn(move || {
        let _ = std::io::stdin().read(&mut [0]);
        let _ = stopped.send(());
    });

    let mut next_tick = Instant::now();
    let watchdog = std::env::var("JWM_TEST_XWAYLAND_MAX_SECONDS").map_or(240, |value| {
        value.parse::<u64>().expect("numeric owned output lifetime")
    });
    assert!(
        (5..=3900).contains(&watchdog),
        "bounded owned output lifetime"
    );
    let expiry = next_tick + Duration::from_secs(watchdog);
    let mut imported = 0;
    let mut surface_entered = None;
    while stop.try_recv().is_err() {
        assert!(
            Instant::now() < expiry,
            "owned launcher did not stop the output"
        );
        accept(&listener, &mut server);
        server.dispatch_timers(Duration::ZERO);

        assert!(
            server.state.toplevels.len() <= 1,
            "owned rootful Xwayland role"
        );
        let top = server.state.toplevels.values().next().cloned();
        if let Some(top) = top {
            let surface = top.wl_surface();
            if surface_entered.as_ref() != Some(surface) {
                output.output.enter(surface);
                surface_entered = Some(surface.clone());
            }
            if !top.is_initial_configure_sent() {
                top.with_pending_state(|state| {
                    state.size = Some((WIDTH as i32, HEIGHT as i32).into());
                    state.states.set(TopState::Activated);
                    state.states.set(TopState::Fullscreen);
                });
                top.send_configure();
                server.display.flush_clients().unwrap();
            }
            if Instant::now() >= next_tick {
                next_tick = Instant::now() + REFRESH_INTERVAL;
                if draw_rootful_surface(&mut output, &mut gl.renderer, surface) {
                    imported += 1;
                    if imported == 1 {
                        replace_report(&report, metadata.rdev(), imported, watchdog);
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
    replace_report(&report, metadata.rdev(), imported, watchdog);
}

fn replace_report(path: &std::path::Path, device: libc::dev_t, imported: u64, watchdog: u64) {
    let pending = path.with_extension("pending");
    let record = json!({"render_node_device": device, "dmabuf_draws": imported, "size": [WIDTH, HEIGHT], "watchdog_seconds": watchdog});
    std::fs::write(&pending, record.to_string()).unwrap();
    std::fs::rename(pending, path).unwrap();
}

fn draw_rootful_surface(
    output: &mut Private4kOutput,
    renderer: &mut GlesRenderer,
    surface: &WlSurface,
) -> bool {
    let size = (WIDTH as i32, HEIGHT as i32);
    let Some((dimensions, is_dmabuf)) = with_renderer_surface_state(surface, |state| {
        state
            .buffer()
            .map(|buffer| (state.surface_size(), get_dmabuf(buffer).is_ok()))
    })
    .flatten() else {
        return false;
    };
    assert_eq!(
        dimensions,
        Some(size.into()),
        "owned Xwayland root geometry"
    );
    let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
        render_elements_from_surface_tree(
            renderer,
            surface,
            (0, 0),
            1.0f64,
            1.0,
            ElementKind::Unspecified,
        );
    assert!(
        !elements.is_empty(),
        "import owned rootful Xwayland surface"
    );
    let mut target = renderer.bind(&mut output.target).unwrap();
    let rendered = output
        .tracker
        .render_output(renderer, &mut target, 1, &elements, Color32F::BLACK)
        .expect("compose the actual committed rootful Xwayland buffer");
    rendered
        .sync
        .wait()
        .expect("finish the owned Xwayland output draw");
    let damaged = rendered.damage.is_some();
    let completed = output.clock.now();
    output.sequence += 1;
    for callback in take_feedback(surface) {
        callback.presented(
            &output.output,
            completed,
            Refresh::Unknown,
            output.sequence,
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
    damaged && is_dmabuf
}
