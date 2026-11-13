//! Real authenticated scroll-copy followed by static PNG repair and GL presentation.
use super::*;

fn scroll_pattern() -> Vec<u8> {
    let mut pixels = pattern(false);
    // Row-dependent color avoids the parent's periodic glyph scroll ambiguity.
    for y in 0..600usize {
        for x in 0..800usize {
            let at = (y * 800 + x) * 4;
            pixels[at] = ((x * 7 + y * 29 + y / 13) % 211 + 20) as u8;
            pixels[at + 1] = ((x / 7 + y * 17 + y / 19) % 211 + 20) as u8;
        }
    }
    pixels
}
fn scroll_pixels(previous: &[u8], dy: i16) -> Vec<u8> {
    let mut next = previous.to_vec();
    for y in 0..600usize {
        let source = y as isize + dy as isize;
        if (0..600).contains(&source) {
            next[y * 3200..(y + 1) * 3200]
                .copy_from_slice(&previous[source as usize * 3200..(source as usize + 1) * 3200]);
        } else {
            for x in 0..800usize {
                let at = (y * 800 + x) * 4;
                next[at..at + 4].copy_from_slice(&[31, 203, 89, 255]);
            }
        }
    }
    next
}

#[test]
fn actual_scroll_copy_preserves_exact_interior_and_repairs_exposed_edges() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut native = Server::new();
    let mut local = Server::new();
    native.state.image_capture_pending = Some(init_image_copy_capture(&native.display.handle()));
    local.state.image_capture_pending = Some(init_image_copy_capture(&local.display.handle()));
    register_test_presentation(&mut local);
    let mut target = App::with_color(&mut native, [0, 0, 255, 255]);
    let mut expected = scroll_pattern();
    repaint(&mut native, &mut target, &expected);
    let (server, stream) = UnixStream::pair().unwrap();
    native
        .display
        .handle()
        .insert_client(server, Arc::new(JwmClientState::default()))
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let _stop = Stop(stop.clone());
    let stopping = stop.clone();
    let (selected_tx, selected) = mpsc::channel();
    let (host_tx, host_done) = mpsc::channel();
    let host_thread = thread::spawn(move || {
        let result = (|| -> crate::remote::RemoteResult<()> {
            let capture = WaylandCapture::connect_test_stream(stream, 1, 0)?;
            selected_tx.send(())?;
            let deadline = Instant::now() + Duration::from_secs(8);
            let stream = loop {
                assert!(Instant::now() < deadline && !stopping.load(Ordering::Acquire));
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(e) => return Err(e.into()),
                }
            };
            let peer = host::authenticate_peer(stream, &KEY)?;
            host::serve_authenticated(
                peer,
                &{
                    let mut options = host_options(true);
                    options.scroll_copy = true;
                    options
                },
                &stopping,
                &Mutex::new(Some(host::GrantedCapture::Wayland(Box::new(capture)))),
            )
        })()
        .map_err(|e| e.to_string());
        let _ = host_tx.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(&mut native, &mut local, &mut gl);
        if selected.try_recv().is_ok() {
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    let (server, viewer_stream) = UnixStream::pair().unwrap();
    local
        .display
        .handle()
        .insert_client(server, Arc::new(JwmClientState::default()))
        .unwrap();
    let frames = Arc::new(Mutex::new((None, None)));
    let observer = frames.clone();
    let presented_times = Arc::new(Mutex::new(Vec::new()));
    let presented_observer = presented_times.clone();
    let codecs = Arc::new(Mutex::new(Vec::new()));
    let codec_observer = codecs.clone();
    let decoded = Arc::new(Mutex::new(Vec::new()));
    let decoded_observer = decoded.clone();
    let (client_tx, client_done) = mpsc::channel();
    let client_thread = thread::spawn(move || {
        let opts = ClientOptions {
            viewer_backend: ViewerBackend::Wayland,
            viewer_wayland_display: None,
            wayland_keyboard: false,
            wayland_text: None,
            static_lossless: true,
            wayland_pointer: false,
            wayland_control: false,
            wayland_scroll: false,
            wayland_horizontal_scroll: false,
            wayland_popup_pointer: false,
            resize_source: None,
            follow_source_size: false,
            window_size: None,
            window_position: None,
            frame_limits: Default::default(),
            address: address.to_string(),
            peer_label: "owned static lossless".into(),
            key: crate::remote::key::KeySource::Bytes(KEY.to_vec()),
            display: None,
            view_only: true,
            grab_input: false,
            seamless_input: false,
            clipboard: false,
            clipboard_mode: Default::default(),
            clipboard_direction: Default::default(),
            clipboard_file_limit: 0,
            escape_key: "Escape".into(),
            scaling: Default::default(),
        };
        let result = crate::remote::client::with_scroll_decode_observer(decoded_observer, || {
            with_static_lossless_frame_observer(codec_observer, || {
                with_wayland_presented_time_observer(presented_observer, || {
                    with_wayland_control_frame_observer(observer, || {
                        with_wayland_test_stream(viewer_stream, || run_client(opts))
                    })
                })
            })
        })
        .map_err(|error| {
            let diagnostic = crate::remote::client::classify_failure(error.as_ref());
            // Keep the actual causal message separate from ClientFailure's
            // user-facing stage and troubleshooting-hint formatting.
            let mut cause: &(dyn std::error::Error + 'static) = error.as_ref();
            for _ in 0..32 {
                match cause.source() {
                    Some(source) => cause = source,
                    None => return (diagnostic, cause.to_string()),
                }
            }
            panic!("unexpected cyclic/oversized client error chain");
        });
        let _ = client_tx.send(result);
    });

    let mut reader = None;
    let mut last = None;
    let mut baseline = None;
    let mut source_scope = None;
    for phase in 0..3 {
        let dy = match phase {
            1 => Some(7),
            2 => Some(-7),
            _ => None,
        };
        if let Some(dy) = dy {
            expected = scroll_pixels(&expected, dy);
            repaint(&mut native, &mut target, &expected);
        }
        let phase_start = Instant::now();
        let mut exact = None;
        let mut scroll_sequence = None;
        let mut repair_tiles = 0u64;
        let mut seen = 0usize;
        let mut total_tiles = None;
        let mut tile_log2 = None;
        loop {
            assert!(
                phase_start.elapsed() < Duration::from_secs(14),
                "scroll phase {phase} timed out: exact={exact:?} scroll={scroll_sequence:?} repair_tiles={repair_tiles}"
            );
            pump(&mut native, &mut local, &mut gl);
            assert!(
                host_done.try_recv().is_err(),
                "host ended before scroll proof"
            );
            assert!(
                client_done.try_recv().is_err(),
                "viewer ended before scroll proof"
            );
            {
                let records = decoded.lock().unwrap();
                for record in &records[seen..] {
                    if baseline.is_some_and(|seq| record.seq <= seq) {
                        continue;
                    }
                    let scope = (record.source_epoch, record.geometry_epoch);
                    if let Some(expected_scope) = source_scope {
                        assert_eq!(scope, expected_scope);
                    } else {
                        source_scope = Some(scope);
                    }
                    total_tiles = Some(record.total_tiles);
                    tile_log2 = Some(record.tile_log2);
                    if let Some(actual_dy) = record.scroll_dy {
                        assert_eq!(
                            Some(actual_dy),
                            dy,
                            "wire scroll direction must match committed source"
                        );
                        assert!(
                            scroll_sequence.replace(record.seq).is_none(),
                            "one source change must not replay scroll"
                        );
                    }
                    if record.lossless {
                        repair_tiles += u64::from(record.dirty_tiles);
                    }
                }
                seen = records.len();
            }
            if let Some(top) = local.state.toplevels.values().next() {
                let surface = top.wl_surface().clone();
                let feedback = take_feedback(&surface);
                if !feedback.is_empty() {
                    assert_eq!(feedback.len(), 1);
                    let seq = frames
                        .lock()
                        .unwrap()
                        .0
                        .expect("actual committed viewer sequence");
                    assert!(last.is_none_or(|previous| seq > previous));
                    let pixels = read_pixels(&mut local, &mut gl, &mut reader);
                    assert_eq!(frames.lock().unwrap().0, Some(seq));
                    let error = sse(&pixels, &expected);
                    eprintln!("scroll exact phase={phase} seq={seq} sse={error}");
                    if error == 0 && baseline.is_none_or(|previous| seq > previous) {
                        exact = Some(seq);
                    }
                    present(&mut local, feedback);
                    last = Some(seq);
                }
            }
            if let Some(seq) = exact {
                if presented_times
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(presented, _)| *presented == seq)
                {
                    let records = decoded.lock().unwrap();
                    let matching = records
                        .iter()
                        .find(|record| record.seq == seq)
                        .expect("presented seq decoded");
                    assert!(
                        matching.lossless,
                        "new exposed strip must be PNG-repaired before exact completion"
                    );
                    let total = total_tiles.unwrap();
                    if let Some(dy) = dy {
                        assert!(
                            scroll_sequence.is_some(),
                            "real network scroll-copy must occur"
                        );
                        let side = 1usize << tile_log2.unwrap();
                        let columns = 800usize.div_ceil(side);
                        let edge_rows = usize::from(dy.unsigned_abs()).div_ceil(side) + 1;
                        // Nonaligned scroll may invalidate the partial bottom row too.
                        let maximum = (columns * (edge_rows + 1)) as u64;
                        assert!(
                            repair_tiles > 0 && repair_tiles <= maximum,
                            "repair only exposed/partial edge tiles: {repair_tiles} > {maximum}"
                        );
                        assert!(repair_tiles < u64::from(total) / 2);
                    } else {
                        assert!(repair_tiles >= u64::from(total));
                    }
                    baseline = Some(seq);
                    break;
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
    stop.store(true, Ordering::Release);
    let until = Instant::now() + Duration::from_secs(8);
    let mut host_result = None;
    let mut client_result = None;
    while host_result.is_none() || client_result.is_none() {
        assert!(Instant::now() < until, "scroll fixture bounded cleanup");
        pump(&mut native, &mut local, &mut gl);
        if let Some(top) = local.state.toplevels.values().next() {
            let feedback = take_feedback(top.wl_surface());
            if !feedback.is_empty() {
                present(&mut local, feedback);
            }
        }
        if host_result.is_none() {
            host_result = host_done.try_recv().ok();
        }
        if client_result.is_none() {
            client_result = client_done.try_recv().ok();
        }
        thread::sleep(Duration::from_millis(1));
    }
    host_thread.join().unwrap();
    client_thread.join().unwrap();
    assert!(host_result.unwrap().is_ok());
    let (diagnostic, cause) = client_result.unwrap().unwrap_err();
    assert_eq!(diagnostic.stage, Some("session"));
    assert!(!diagnostic.reconnect_certified);
    if diagnostic.cleanup_sender_transport_only {
        assert_eq!(diagnostic.category, "cleanup_failed");
        assert_eq!(diagnostic.cleanup, "incomplete");
    } else {
        assert_eq!(diagnostic.category, "peer_closed");
        assert_eq!(diagnostic.cleanup, "unknown");
    }
    assert_eq!(
        diagnostic.peer_close_code,
        Some(crate::remote::close_wire::SessionCloseCode::HostStopping)
    );
    assert_eq!(
        cause,
        "host closed the remote session: the host stopped sharing its desktop"
    );
}

#[test]
fn scroll_oracle_copies_exact_rows_in_both_directions() {
    let initial = scroll_pattern();
    for dy in [7, -7] {
        let shifted = scroll_pixels(&initial, dy);
        assert_ne!(initial, shifted);
        for y in 0..600usize {
            let source = y as isize + dy as isize;
            let row = &shifted[y * 3200..(y + 1) * 3200];
            if (0..600).contains(&source) {
                assert_eq!(
                    row,
                    &initial[source as usize * 3200..(source as usize + 1) * 3200]
                );
            } else {
                assert!(row.chunks_exact(4).all(|pixel| pixel == [31, 203, 89, 255]));
            }
        }
    }
}
