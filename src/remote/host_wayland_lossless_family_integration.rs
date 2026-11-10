//! Read-only popup family + passive resize + static PNG, at matching viewer pixels.
//! No scaled-source oracle: every accepted sample has a native-sized viewer buffer.
use super::*;

fn root_pixels(w: u32, h: u32, revision: u8) -> Vec<u8> {
    let mut bytes = vec![0; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let stroke = x % 11 == 0 || y % 17 == 0 || (x % 11 == 7 && y % 17 < 10);
            let rgb = if stroke {
                [31 + revision, 181, 53]
            } else {
                [207, 41 + revision, 157]
            };
            let i = ((y * w + x) * 4) as usize;
            bytes[i..i + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
        }
    }
    bytes
}
fn paint(server: &mut Server, app: &mut App, w: u32, h: u32, bytes: &[u8]) {
    let (buffer, mut file) = pixels(server, &mut app.client, w, h, [0, 0, 0, 255]);
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(bytes).unwrap();
    app.client.request(app.surface, 1, &[buffer, 0, 0]);
    app.client.request(app.surface, 9, &[0, 0, w, h]);
    app.client.request(app.surface, 6, &[]);
    server.roundtrip();
    server.roundtrip();
}
fn expected(w: u32, h: u32, revision: u8, popup: Option<(i32, i32)>) -> Vec<u8> {
    let mut bytes = root_pixels(w, h, revision);
    if let Some((px, py)) = popup {
        for y in py.max(0)..(py + 60).min(h as i32) {
            for x in px.max(0)..(px + 80).min(w as i32) {
                let i = ((y as u32 * w + x as u32) * 4) as usize;
                bytes[i..i + 4].copy_from_slice(&[21, 207, 231, 255]);
            }
        }
    }
    bytes
}
fn pump(h: &mut Harness, gl: &mut HeadlessSmithayGles, size: (u32, u32)) {
    h.native.roundtrip();
    fulfill_test_toplevel_capture(
        &mut gl.renderer,
        &mut None,
        &h.native.state,
        h.native.state.image_capture_pending.as_ref().unwrap(),
    );
    h.native.roundtrip();
    h.local.roundtrip();
    for (&win, top) in &h.local.state.toplevels {
        let geo = h.local.state.window_geometry.get_mut(&win).unwrap();
        if !top.is_initial_configure_sent() || (geo.w, geo.h) != size {
            geo.w = size.0;
            geo.h = size.1;
            top.with_pending_state(|state| {
                state.size = Some((size.0 as i32, size.1 as i32).into())
            });
            top.send_configure();
        }
    }
    h.local.roundtrip();
}
fn actual_pixels(
    h: &mut Harness,
    gl: &mut HeadlessSmithayGles,
    reader: &mut Capture,
    size: (u32, u32),
) -> Vec<u8> {
    let (buffer, mut file) = pixels(&mut h.local, &mut reader.client, size.0, size.1, [0x5a; 4]);
    let frame = reader.client.new_id();
    reader.client.request(reader.session, 0, &[frame]);
    reader.client.request(frame, 1, &[buffer]);
    reader.client.request(frame, 3, &[]);
    h.local.roundtrip();
    h.local.roundtrip();
    fulfill_test_toplevel_capture(
        &mut gl.renderer,
        &mut None,
        &h.local.state,
        h.local.state.image_capture_pending.as_ref().unwrap(),
    );
    h.local.roundtrip();
    h.local.roundtrip();
    let events = reader.client.events();
    assert!(!events.iter().any(|e| e.sender == 1 && e.opcode == 0));
    assert!(
        events.iter().any(|e| e.sender == frame && e.opcode == 3),
        "real viewer GL frame must complete"
    );
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes.len(), (size.0 * size.1 * 4) as usize);
    reader.client.request(frame, 0, &[]);
    h.local.roundtrip();
    bytes
}
fn wait_exact(
    h: &mut Harness,
    gl: &mut HeadlessSmithayGles,
    size: (u32, u32),
    expected: &[u8],
    after: Option<u64>,
    min_epoch: u64,
    context: (&str, &host::PendingLosslessProbe),
) -> (u64, u64) {
    let (phase, probe) = context;
    let until = Instant::now() + Duration::from_secs(12);
    let mut reader = None;
    let mut last = after;
    let mut exact = None;
    let mut saw_lossy = false;
    let mut last_error = None;
    let mut first_difference = None;
    loop {
        pump(h, gl, size);
        if let Some(top) = h.local.state.toplevels.values().next() {
            let surface = top.wl_surface().clone();
            let feedback = take_feedback(&surface);
            if !feedback.is_empty() {
                assert_eq!(feedback.len(), 1);
                let seq = h.frames.lock().unwrap().0.unwrap();
                assert!(
                    last.is_none_or(|old| seq > old),
                    "each committed feedback belongs to a fresh frame"
                );
                last = Some(seq);
                let dimensions = smithay::wayland::shm::with_buffer_contents(
                    &committed_buffer(&surface),
                    |_, _, data| (data.width as u32, data.height as u32),
                )
                .unwrap();
                if dimensions == size {
                    if reader.is_none() {
                        reader = Some(Capture::new_with_size(&mut h.local, size));
                    }
                    let bytes = actual_pixels(h, gl, reader.as_mut().unwrap(), size);
                    assert_eq!(h.frames.lock().unwrap().0, Some(seq));
                    // Root and popup contain no blue. This detects bystander
                    // capture on every inspected transition, not just final PNG.
                    assert!(
                        !bytes
                            .chunks_exact(4)
                            .any(|p| p[0] > 220 && p[1] < 30 && p[2] < 30),
                        "blue bystander pixels leaked"
                    );
                    let error: u64 = bytes
                        .iter()
                        .zip(expected)
                        .map(|(&a, &b)| {
                            let d = i64::from(a) - i64::from(b);
                            (d * d) as u64
                        })
                        .sum();
                    last_error = Some(error);
                    first_difference = bytes
                        .chunks_exact(4)
                        .zip(expected.chunks_exact(4))
                        .enumerate()
                        .find(|(_, (actual, expected))| actual != expected)
                        .map(|(index, (actual, expected))| {
                            (
                                index % size.0 as usize,
                                index / size.0 as usize,
                                actual.to_vec(),
                                expected.to_vec(),
                            )
                        });
                    let codecs = h.codecs.lock().unwrap();
                    let png = codecs
                        .iter()
                        .find(|(n, _)| *n == seq)
                        .expect("successful decoder codec evidence")
                        .1;
                    saw_lossy |= !png && error > 0;
                    // Join actual successful publication, never the latest
                    // capture (which can already belong to another epoch).
                    let publication_deadline = Instant::now() + Duration::from_secs(1);
                    let publication = loop {
                        let writes = probe.written();
                        let matched: Vec<_> = writes
                            .iter()
                            .filter(|record| record.0 == seq)
                            .copied()
                            .collect();
                        assert!(
                            matched.len() <= 1,
                            "successful wire sequence cannot belong to two scopes"
                        );
                        if let Some(record) = matched.first() {
                            assert_eq!(
                                record.1,
                                writes.first().unwrap().1,
                                "selected source identity must not change"
                            );
                            break *record;
                        }
                        assert!(
                            Instant::now() < publication_deadline,
                            "decoded sequence {seq} lacks successful publication evidence"
                        );
                        thread::sleep(Duration::from_millis(1));
                    };
                    assert_eq!(
                        publication.3, png,
                        "sender and actual decoder codec evidence disagree"
                    );
                    eprintln!(
                        "lossless family phase={phase} size={size:?} seq={seq} source={} epoch={} png={png} keyframe={} sse={error} first_difference={first_difference:?}",
                        publication.1, publication.2, publication.4
                    );
                    if error == 0 && publication.2 >= min_epoch {
                        assert!(
                            probe.written().iter().any(|record| after
                                .is_none_or(|old| record.0 > old)
                                && record.0 <= seq
                                && record.1 == publication.1
                                && record.2 == publication.2
                                && record.3),
                            "exact recovery requires PNG in this precise presented scope"
                        );
                        exact = Some((seq, publication.2));
                    }
                }
                present(&mut h.local, feedback);
            }
        }
        if let Some((seq, epoch)) = exact {
            if h.frames.lock().unwrap().1 == Some(seq) {
                // The first scene is deliberately high-frequency and must prove
                // this is a lossy-to-exact recovery, not an already-exact JPEG.
                if after.is_none() {
                    assert!(saw_lossy);
                }
                return (seq, epoch);
            }
        }
        if let Ok(result) = h.host_done.try_recv() {
            panic!("host ended before exact family pixels: {result:?}");
        }
        if let Ok(result) = h.client_done.try_recv() {
            panic!("viewer ended before exact family pixels: {result:?}");
        }
        assert!(
            Instant::now() < until,
            "exact family deadline phase={phase} size={size:?} min_epoch={min_epoch} after={after:?} last={last:?} last_sse={last_error:?} first_difference={first_difference:?} frames={:?}",
            *h.frames.lock().unwrap()
        );
        thread::sleep(Duration::from_millis(1));
    }
}
fn finish(mut h: Harness, gl: &mut HeadlessSmithayGles, size: (u32, u32), lock: bool) {
    if lock {
        h.native.state.session_lock_supported = true;
        let mut locker = h.native.connect();
        let manager = locker.bind("ext_session_lock_manager_v1", 1);
        let lease = locker.new_id();
        locker.request(manager, 1, &[lease]);
        h.native.roundtrip();
        h.native.roundtrip();
        assert!(h.native.state.session_locked);
        locker.request(lease, 2, &[]);
        h.native.roundtrip();
        h.native.roundtrip();
        assert!(!h.native.state.session_locked);
    } else {
        h.target.unmap(&mut h.native);
        h.target.remap(&mut h.native);
    }
    let until = Instant::now() + Duration::from_secs(8);
    let mut host = None;
    let mut client = None;
    while host.is_none() || client.is_none() {
        pump(&mut h, gl, size);
        if let Some(top) = h.local.state.toplevels.values().next() {
            let feedback = take_feedback(top.wl_surface());
            if !feedback.is_empty() {
                present(&mut h.local, feedback);
            }
        }
        if host.is_none() {
            host = h.host_done.try_recv().ok();
        }
        if client.is_none() {
            client = h.client_done.try_recv().ok();
        }
        assert!(
            Instant::now() < until,
            "revoked lossless family must terminate, not resume"
        );
        thread::sleep(Duration::from_millis(1));
    }
    assert!(host.unwrap().is_err());
    assert!(client.unwrap().is_err());
    let failure = h.client_failure.lock().unwrap();
    let (diagnostic, cause) = failure
        .as_ref()
        .expect("typed receiver failure required for revoked capture");
    assert_eq!(diagnostic.stage, Some("session"));
    if diagnostic.cleanup_sender_transport_only {
        // Only a typed closed-socket failure from the joined ACK sender may
        // accompany the exact authenticated capture revocation checked below.
        assert_eq!(diagnostic.category, "cleanup_failed");
        assert_eq!(diagnostic.cleanup, "incomplete");
        assert!(!diagnostic.reconnect_certified);
    } else {
        assert_eq!(diagnostic.category, "peer_closed");
        assert_eq!(diagnostic.cleanup, "unknown");
    }
    assert_eq!(
        diagnostic.peer_close_code,
        Some(crate::remote::close_wire::SessionCloseCode::CaptureUnavailable)
    );
    assert_eq!(
        cause, "host closed the remote session: host capture became unavailable",
        "unrelated transport/setup error is not a revocation result"
    );
    drop(failure);
    h.stop.store(true, Ordering::Release);
    h.host.take().unwrap().join().unwrap();
    h.client.take().unwrap().join().unwrap();
}
#[test]
fn actual_lossless_popup_resize_family_exact_pixels_epochs_and_revocation() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for lock in [false, true] {
        let probe = Arc::new(host::PendingLosslessProbe::observing_only());
        let mut h = Harness::with_lossless_probe(&mut gl, true, true, true, Some(probe.clone()));
        h.destroy_popup();
        paint(
            &mut h.native,
            &mut h._other,
            640,
            360,
            &[255, 0, 0, 255].repeat(640 * 360),
        );
        paint(
            &mut h.native,
            &mut h.target,
            320,
            240,
            &root_pixels(320, 240, 0),
        );
        let (mut seq, first_epoch) = wait_exact(
            &mut h,
            &mut gl,
            (320, 240),
            &expected(320, 240, 0, None),
            None,
            0,
            ("initial-root", &probe),
        );
        let xdg = h.target.xdg;
        h.popup = Some(Popup::new(
            &mut h.native,
            &mut h.target.client,
            xdg,
            (40, 40, 80, 60),
            [21, 207, 231, 255],
        ));
        seq = wait_exact(
            &mut h,
            &mut gl,
            (320, 240),
            &expected(320, 240, 0, Some((40, 40))),
            Some(seq),
            first_epoch,
            ("popup-created", &probe),
        )
        .0;
        h.resize(640, 360, false, None);
        paint(
            &mut h.native,
            &mut h.target,
            640,
            360,
            &root_pixels(640, 360, 1),
        );
        let (large_seq, large_epoch) = wait_exact(
            &mut h,
            &mut gl,
            (640, 360),
            &expected(640, 360, 1, Some((40, 40))),
            Some(seq),
            first_epoch + 1,
            ("resize-large", &probe),
        );
        h.popup
            .as_mut()
            .unwrap()
            .reposition(&mut h.native, &mut h.target.client, (600, 320));
        seq = wait_exact(
            &mut h,
            &mut gl,
            (640, 360),
            &expected(640, 360, 1, Some((600, 320))),
            Some(large_seq),
            large_epoch,
            ("popup-moved-clipped", &probe),
        )
        .0;
        h.popup
            .as_mut()
            .unwrap()
            .reposition(&mut h.native, &mut h.target.client, (40, 40));
        h.resize(320, 240, true, None);
        paint(
            &mut h.native,
            &mut h.target,
            320,
            240,
            &root_pixels(320, 240, 2),
        );
        let (returned, return_epoch) = wait_exact(
            &mut h,
            &mut gl,
            (320, 240),
            &expected(320, 240, 2, Some((40, 40))),
            Some(seq),
            large_epoch + 1,
            ("returned-new-revision", &probe),
        );
        assert!(return_epoch > large_epoch && large_epoch > first_epoch);
        h.destroy_popup();
        wait_exact(
            &mut h,
            &mut gl,
            (320, 240),
            &expected(320, 240, 2, None),
            Some(returned),
            return_epoch,
            ("popup-destroyed", &probe),
        );
        finish(h, &mut gl, (320, 240), lock);
    }
}
#[test]
fn family_oracle_clips_popup_and_distinguishes_returned_geometry() {
    let initial = expected(320, 240, 0, None);
    let returned = expected(320, 240, 2, None);
    assert_ne!(initial, returned);
    let clipped = expected(320, 240, 0, Some((300, 220)));
    let changed = initial
        .chunks_exact(4)
        .zip(clipped.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(changed, 20 * 20);
}

#[test]
fn actual_pending_png_resize_aba_discards_old_scope_before_write() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let probe = Arc::new(host::PendingLosslessProbe::default());
    let mut h = Harness::with_lossless_probe(&mut gl, true, true, true, Some(probe.clone()));
    h.destroy_popup();
    paint(
        &mut h.native,
        &mut h.target,
        320,
        240,
        &root_pixels(320, 240, 0),
    );
    let until = Instant::now() + Duration::from_secs(10);
    let mut reader = None;
    let mut first_jpeg = None;
    let blocked = loop {
        pump(&mut h, &mut gl, (320, 240));
        if let Some(top) = h.local.state.toplevels.values().next() {
            let surface = top.wl_surface().clone();
            let feedback = take_feedback(&surface);
            if !feedback.is_empty() {
                assert_eq!(feedback.len(), 1);
                let seq = h.frames.lock().unwrap().0.unwrap();
                let dims = smithay::wayland::shm::with_buffer_contents(
                    &committed_buffer(&surface),
                    |_, _, d| (d.width, d.height),
                )
                .unwrap();
                if dims == (320, 240) {
                    if reader.is_none() {
                        reader = Some(Capture::new_with_size(&mut h.local, (320, 240)));
                    }
                    let actual =
                        actual_pixels(&mut h, &mut gl, reader.as_mut().unwrap(), (320, 240));
                    assert_eq!(h.frames.lock().unwrap().0, Some(seq));
                    let codecs = h.codecs.lock().unwrap();
                    assert!(!codecs.iter().find(|(n, _)| *n == seq).unwrap().1);
                    assert_ne!(
                        actual,
                        root_pixels(320, 240, 0),
                        "initial JPEG must be lossy"
                    );
                    first_jpeg = Some(seq);
                }
                present(&mut h.local, feedback);
            }
        }
        if let Some(scope) = probe.blocked() {
            if first_jpeg.is_some_and(|seq| h.frames.lock().unwrap().1 == Some(seq)) {
                break scope;
            }
        }
        assert!(
            Instant::now() < until,
            "first PNG must reach deterministic pre-write barrier"
        );
        thread::sleep(Duration::from_millis(1));
    };
    drop(reader);
    let old_count = probe.written().len();
    eprintln!("pending PNG blocked seq/source/geometry={blocked:?}, already_written={old_count}");
    let transition_deadline = Instant::now() + Duration::from_secs(4);
    h.resize(640, 360, false, None);
    while probe.publication_epoch() <= blocked.2 {
        pump(&mut h, &mut gl, (640, 360));
        assert!(
            Instant::now() < transition_deadline,
            "host must publish G2 while sender is paused"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let large_epoch = probe.publication_epoch();
    h.resize(320, 240, true, None);
    paint(
        &mut h.native,
        &mut h.target,
        320,
        240,
        &root_pixels(320, 240, 3),
    );
    while probe.publication_epoch() <= large_epoch {
        pump(&mut h, &mut gl, (320, 240));
        assert!(
            Instant::now() < transition_deadline,
            "host must publish G3 before releasing old PNG"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let returned_epoch = probe.publication_epoch();
    eprintln!(
        "pending PNG before release old_geometry={} large_publication={large_epoch} returned_publication={returned_epoch} last_capture={:?}",
        blocked.2,
        h.captures.lock().unwrap().last()
    );
    probe.release();
    let (seq, epoch) = wait_exact(
        &mut h,
        &mut gl,
        (320, 240),
        &expected(320, 240, 3, None),
        first_jpeg,
        returned_epoch,
        ("pending-PNG-ABA-return", &probe),
    );
    assert_eq!(epoch, returned_epoch);
    let writes = probe.written();
    assert!(
        !writes
            .iter()
            .any(|&(s, source, geometry, png, _)| png && (s, source, geometry) == blocked),
        "old-scope PNG must not reach successful flush"
    );
    let first_new = *writes
        .get(old_count)
        .expect("new geometry must publish a frame");
    assert_eq!(first_new.2, returned_epoch);
    assert!(
        !first_new.3 && first_new.4,
        "new geometry begins with a JPEG keyframe, never stale exact debt"
    );
    assert!(
        writes
            .iter()
            .any(|&(s, _, g, png, _)| s == seq && g == returned_epoch && png),
        "exact presented recovery must join successful new-scope PNG publication"
    );
    eprintln!(
        "pending PNG discarded={blocked:?}; first_new={first_new:?}; exact_new=({seq},{epoch})"
    );
    finish(h, &mut gl, (320, 240), false);
}
