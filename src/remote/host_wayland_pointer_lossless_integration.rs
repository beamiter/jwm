//! Owned protocol displays + authenticated production client/host + real GL.
//! Presentation feedback is controlled, not a physical DRM scanout measurement.
use super::*;
use crate::remote::source_wire::FramePurpose;

fn pattern() -> (Vec<u8>, Vec<u8>) {
    sized_pattern((SW, SH), (VW, VH))
}
fn sized_pattern(source: (u32, u32), viewer: (u32, u32)) -> (Vec<u8>, Vec<u8>) {
    let rgb = RgbImage::from_fn(source.0, source.1, |x, y| {
        // A large solid marker appears only after the real target click.
        // Its interior survives JPEG; the glyph region must still be nonexact.
        if x >= source.0 - 32 && y >= source.1 - 32 {
            Rgb([31, 167, 223])
        } else if x % 13 == 0 || y % 17 == 0 {
            Rgb([19, 193, 47])
        } else {
            Rgb([211, 37, 173])
        }
    });
    let source = rgb.pixels().flat_map(|p| [p[2], p[1], p[0], 255]).collect();
    let scaled = image::imageops::resize(
        &rgb,
        viewer.0,
        viewer.1,
        image::imageops::FilterType::Nearest,
    );
    let expected = scaled
        .pixels()
        .flat_map(|p| [p[2], p[1], p[0], 255])
        .collect();
    (source, expected)
}
fn has_clicked_pattern_marker(pixels: &[u8]) -> bool {
    if pixels.len() != (VW * VH * 4) as usize {
        return false;
    }
    // 16x16 viewer pixels fully inside the 64x64 nearest-scaled solid patch,
    // well away from its JPEG tile boundaries. Initial red cannot match.
    (VH - 40..VH - 24).all(|y| {
        (VW - 40..VW - 24).all(|x| {
            let at = ((y * VW + x) * 4) as usize;
            pixels[at..at + 3]
                .iter()
                .zip([223u8, 167, 31])
                .all(|(a, b)| a.abs_diff(b) <= 8)
                && pixels[at + 3] == 255
        })
    })
}

fn repaint_pixels(h: &mut Harness, bytes: &[u8]) {
    let (buffer, mut file) = shm(
        &mut h.native,
        &mut h.target.client,
        [0, 0, 0, 255],
        h.source_size.0,
        h.source_size.1,
    );
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(bytes).unwrap();
    h.target
        .client
        .request(h.target.surface, 1, &[buffer, 0, 0]);
    h.target.client.request(
        h.target.surface,
        2,
        &[0, 0, h.source_size.0, h.source_size.1],
    );
    h.target.client.request(h.target.surface, 6, &[]);
    h.native.roundtrip();
    h.native.roundtrip();
}
fn checked_tick(
    h: &mut Harness,
    gl: &mut HeadlessSmithayGles,
    inspect: &mut dyn FnMut(&[u8], u64) -> bool,
    deadline: Instant,
    phase: &str,
) {
    assert!(Instant::now() < deadline, "{phase}: bounded wait expired");
    h.tick_with_inspector(gl, true, Some(inspect));
    if let Ok(result) = h.client_done.try_recv() {
        panic!("{phase}: unexpected client end {result:?}");
    }
    assert!(h.unexpected_wire.is_empty());
    thread::yield_now();
}
fn click(h: &mut Harness) {
    h.button(true);
    h.button(false);
}

#[test]
fn actual_pointer_lossless_repair_requires_new_native_proof_and_preserves_exact_pixels() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_with_lossless(&mut gl, true, true);
    let deadline = Instant::now() + Duration::from_secs(24);
    let initial = h.ready_after(&mut gl, None);
    assert!(h.events.is_empty());
    click(&mut h);
    h.wait(&mut gl, "first exact target click", true, |s| {
        s.events.len() == 3
    });
    assert_eq!(h.events[1].1[2..], [0x110, 1]);
    assert_eq!(h.events[2].1[2..], [0x110, 0]);
    let (source, expected) = pattern();
    // The application changes content only AFTER observing the real recipient.
    repaint_pixels(&mut h, &source);
    let purposes = h.purposes.clone();
    let mut jpeg_seen = false;
    let mut repair = None;
    let waiting_started = Instant::now();
    while repair.is_none() {
        checked_tick(
            &mut h,
            &mut gl,
            &mut |pixels, seq| {
                let purpose = purposes
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|r| r.0 == seq)
                    .unwrap()
                    .1;
                if seq > initial
                    && purpose != FramePurpose::VisualRepair
                    && pixels != expected
                    && has_clicked_pattern_marker(pixels)
                {
                    jpeg_seen = true;
                }
                if purpose == FramePurpose::VisualRepair {
                    assert_eq!(
                        pixels, expected,
                        "PNG must restore every scaled RGB byte, not just sampled colors"
                    );
                    repair = Some(seq);
                }
                true
            },
            deadline,
            "JPEG response then exact PNG",
        );
    }
    assert!(jpeg_seen, "real JPEG response must precede exact repair");
    let repair = repair.unwrap();
    // A PNG ACK returns credits, not pointer authority. Hold the next actual
    // proof's presentation; neither the prior ACK nor repair ACK can make Ready.
    let mut next_proof = None;
    loop {
        checked_tick(
            &mut h,
            &mut gl,
            &mut |pixels, seq| {
                assert_eq!(pixels, expected, "static pixels changed after exact repair");
                let purpose = purposes
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|r| r.0 == seq)
                    .unwrap()
                    .1;
                if seq > repair && purpose == FramePurpose::NativeProof {
                    next_proof = Some(seq);
                    false
                } else {
                    true
                }
            },
            deadline,
            "repair ACK before fresh proof feedback",
        );
        if h.frames.lock().unwrap().1 == Some(repair) && next_proof.is_some() {
            break;
        }
    }
    assert!(
        h.ready.lock().unwrap().is_none(),
        "repair/old ACK must not restore Ready"
    );
    assert_eq!(h.events.len(), 3, "no extra input during repair");
    let proof = next_proof.unwrap();
    // Observe a sustained gate-closed interval while a genuinely new proof is
    // committed but its feedback is withheld. This is not a forged/replayed ACK
    // test: it proves old and real PNG feedback do not transiently enable input.
    let withheld_until = Instant::now() + Duration::from_millis(100);
    while Instant::now() < withheld_until {
        checked_tick(
            &mut h,
            &mut gl,
            &mut |pixels, seq| {
                assert_eq!(seq, proof);
                assert_eq!(pixels, expected);
                false
            },
            deadline,
            "withheld post-repair proof stays noninteractive",
        );
        assert!(h.ready.lock().unwrap().is_none());
        assert_eq!(h.frames.lock().unwrap().1, Some(repair));
        assert_eq!(h.events.len(), 3);
        assert!(remote_pointer::active(&h.native.state));
    }
    while *h.ready.lock().unwrap() != Some(proof) {
        checked_tick(
            &mut h,
            &mut gl,
            &mut |pixels, seq| {
                assert_eq!(seq, proof, "single outstanding proof must stay fixed");
                assert_eq!(pixels, expected);
                true
            },
            deadline,
            "new actual proof presentation and Ready",
        );
    }
    assert_eq!(h.frames.lock().unwrap().1, Some(proof));
    let app_commit_to_repair_then_ready = waiting_started.elapsed();
    let second_click_started = Instant::now();
    click(&mut h);
    while h.events.len() < 6 {
        checked_tick(
            &mut h,
            &mut gl,
            &mut |_, _| true,
            deadline,
            "second target click",
        );
    }
    assert_eq!(h.events.len(), 6);
    assert_eq!(h.events[4].1[2..], [0x110, 1]);
    assert_eq!(h.events[5].1[2..], [0x110, 0]);
    eprintln!(
        "pointer_lossless clickable_actions=2 app_commit_to_repair_then_ready_ms={} second_click_recipient_ms={} exact_bytes={} repair_sequence={} proof_sequence={}",
        app_commit_to_repair_then_ready.as_millis(),
        second_click_started.elapsed().as_millis(),
        expected.len(),
        repair,
        proof
    );
    // Terminal local focus loss must not be undone by subsequent video/PNG.
    let surface = h
        .local
        .state
        .toplevels
        .values()
        .next()
        .unwrap()
        .wl_surface()
        .clone();
    h.pointer.as_ref().unwrap().leave(99, &surface);
    h.pointer.as_ref().unwrap().frame();
    h.local.roundtrip();
    while remote_pointer::active(&h.native.state) {
        checked_tick(
            &mut h,
            &mut gl,
            &mut |_, _| true,
            deadline,
            "focus loss revokes native lease",
        );
    }
    assert!(h.ready.lock().unwrap().is_none());
    assert_eq!(h.events.len(), 6);
    // close(false) never invokes the old red/blue oracle for patterned pixels.
    h.close(&mut gl);
}

fn assert_expected_host_exit(
    result: crate::remote::RemoteResult<()>,
    capture_terminated: bool,
    confirmed_client_denial: bool,
) {
    if let Err(error) = result {
        // Preserve the actual producer type across the thread boundary. A
        // capture-stop allowance never admits encoding/cleanup/panic errors.
        if capture_terminated
            && let Some(host::RuntimeClose::Structured(close)) = host::runtime_close(&error)
            && close.code == crate::remote::close_wire::SessionCloseCode::CaptureUnavailable
        {
            return;
        }
        // Only a previously checked explicit feature-denial client result
        // permits a reset of the authenticated connection during Hello teardown.
        if confirmed_client_denial
            && let Some(crate::remote::protocol::ProtocolError::Io(io)) =
                error.downcast_ref::<crate::remote::protocol::ProtocolError>()
            && io.kind() == std::io::ErrorKind::ConnectionReset
        {
            return;
        }
        // Peer denial closes its authenticated transport. Only transport EOF
        // is allowed here, not arbitrary errors with an EOF-like description.
        assert!(
            host::is_end_of_stream(&error),
            "unexpected host teardown error: {error:?}"
        );
    }
}

fn terminal_join(
    mut h: Harness,
    gl: &mut HeadlessSmithayGles,
    expected_client_error: Option<&str>,
) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        h.tick(gl, false);
        if let Ok(result) = h.client_done.try_recv() {
            if let Some(expected) = expected_client_error {
                let error = result.expect_err("explicit denial/terminal source must not succeed");
                assert!(error.contains(expected), "wrong failure: {error}");
            }
            break;
        }
        assert!(
            Instant::now() < deadline,
            "terminal client must finish explicitly, not timeout"
        );
        thread::yield_now();
    }
    h.shutdown.store(true, Ordering::Release);
    loop {
        h.tick(gl, false);
        if let Ok(result) = h.host_done.try_recv() {
            assert_expected_host_exit(
                result,
                false,
                expected_client_error == Some("static lossless"),
            );
            break;
        }
        assert!(Instant::now() < deadline, "host teardown timeout");
        thread::yield_now();
    }
    assert!(h.unexpected_wire.is_empty());
    assert!(!remote_pointer::active(&h.native.state));
    h.client.take().unwrap().join().unwrap();
    h.host.take().unwrap().join().unwrap();
}

#[test]
fn actual_pointer_lossless_missing_host_permission_is_explicit_denial() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let h = Harness::new_with_lossless(&mut gl, false, true);
    terminal_join(h, &mut gl, Some("static lossless"));
}

#[test]
fn actual_pointer_lossless_lock_and_unmap_aba_cannot_restore_authority() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for lock in [true, false] {
        let mut h = Harness::new_with_lossless(&mut gl, true, true);
        h.ready_after(&mut gl, None);
        assert!(remote_pointer::active(&h.native.state));
        assert!(
            h.events.is_empty(),
            "lock={lock} native action events={:?}; all wire={:?}",
            h.events,
            h.wire_events
        );
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
        assert!(
            !remote_pointer::active(&h.native.state),
            "ABA must permanently revoke native authority"
        );
        // Lock's production revoke_unlocked_input calls unset_grab with
        // restore_focus=true before clearing pointer focus. Smithay emits one
        // same-target motion at the stored (5,5), with time=0. This is local
        // focus restoration, not a remote action. Unmap has no such allowance.
        // Match the complete finite sequence, never an arbitrary baseline.
        let expected_events = if lock {
            vec![(2u16, vec![0, 5 * 256, 5 * 256])]
        } else {
            Vec::new()
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            h.tick(&mut gl, false);
            assert!(
                expected_events.starts_with(&h.events),
                "lock={lock} unexpected lifecycle events={:?}; all wire={:?}",
                h.events,
                h.wire_events
            );
            if h.events == expected_events {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "lock={lock}: missing exact local restoration motion"
            );
            thread::yield_now();
        }
        // Explicit client completion is required, not a timeout. From here
        // every additional motion/button/axis (including a duplicate) fails.
        while h.client_done.try_recv().is_err() {
            h.tick(&mut gl, false);
            assert!(
                h.events == expected_events,
                "lock={lock} native action events={:?}; all wire={:?}",
                h.events,
                h.wire_events
            );
            assert!(h.unexpected_wire.is_empty());
            assert!(!remote_pointer::active(&h.native.state));
            assert!(
                Instant::now() < deadline,
                "terminal source failed to close viewer"
            );
            thread::yield_now();
        }
        h.shutdown.store(true, Ordering::Release);
        loop {
            h.tick(&mut gl, false);
            assert_eq!(
                h.events, expected_events,
                "lock={lock}: extra teardown input; wire={:?}",
                h.wire_events
            );
            if let Ok(result) = h.host_done.try_recv() {
                assert_expected_host_exit(result, true, false);
                break;
            }
            assert!(Instant::now() < deadline, "terminal host teardown timeout");
            thread::yield_now();
        }
        h.client.take().unwrap().join().unwrap();
        h.host.take().unwrap().join().unwrap();
        assert!(
            h.events == expected_events,
            "lock={lock} native action events={:?}; all wire={:?}",
            h.events,
            h.wire_events
        );
        assert!(h.unexpected_wire.is_empty());
    }
}

#[test]
fn actual_pointer_lossless_without_viewer_optin_never_publishes_repair() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_with_lossless(&mut gl, true, false);
    h.ready_after(&mut gl, None);
    click(&mut h);
    h.wait(&mut gl, "ordinary pointer remains usable", true, |s| {
        s.events.len() == 3
    });
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        h.tick(&mut gl, true);
        assert!(
            h.purposes
                .lock()
                .unwrap()
                .iter()
                .all(|(_, purpose)| *purpose != FramePurpose::VisualRepair)
        );
        assert_eq!(h.events.len(), 3);
        if let Ok(result) = h.client_done.try_recv() {
            panic!("ordinary pointer unexpectedly ended: {result:?}");
        }
        thread::yield_now();
    }
    assert!(
        h.purposes.lock().unwrap().len() >= 2,
        "negative must observe real frame traffic"
    );
    h.close(&mut gl);
}

#[test]
fn actual_pointer_lossless_local_host_takeover_revokes_lease() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_with_lossless(&mut gl, true, true);
    h.ready_after(&mut gl, None);
    assert!(remote_pointer::active(&h.native.state));
    // Real trusted-local protocol input invokes the same LOCAL revocation
    // boundary as the libinput callback. This is not a physical-device test:
    // direct seat.motion/focus helpers bypass that acquisition boundary.
    crate::backend::wayland_udev::virtual_pointer::init_virtual_pointer_manager(
        &h.native.display.handle(),
    );
    let mut local_input = h.native.connect();
    let manager = local_input.bind("zwlr_virtual_pointer_manager_v1", 2);
    let pointer = local_input.new_id();
    local_input.request(manager, 0, &[0, pointer]);
    local_input.request(pointer, 0, &[1, 256, 0]); // wl_fixed +1 x motion
    h.native.roundtrip();
    h.native.roundtrip();
    assert!(!remote_pointer::active(&h.native.state));
    h.tick(&mut gl, false);
    let after_physical = h.events.clone();
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.ready.lock().unwrap().is_some() {
        h.tick(&mut gl, true);
        assert!(!remote_pointer::active(&h.native.state));
        assert!(
            Instant::now() < deadline,
            "native revocation must reach viewer"
        );
        thread::yield_now();
    }
    assert_eq!(
        h.events, after_physical,
        "no remote action may follow physical takeover"
    );
    assert!(h.unexpected_wire.is_empty());
    h.close(&mut gl);
}

#[test]
fn clicked_pattern_marker_rejects_initial_red_and_truncated_readback() {
    let (_, pixels) = pattern();
    assert!(has_clicked_pattern_marker(&pixels));
    assert!(!has_clicked_pattern_marker(
        &[0, 0, 255, 255].repeat((VW * VH) as usize)
    ));
    assert!(!has_clicked_pattern_marker(&pixels[..pixels.len() - 1]));
}

#[test]
fn actual_pointer_lossless_800x600_multibatch_reaches_exact_and_remains_clickable() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_sized(
        &mut gl,
        (false, false),
        (false, false),
        true,
        true,
        (800, 600),
        (800, 600),
    );
    // Same bounded test budget as the small fixture, never a lease extension.
    let deadline = Instant::now() + Duration::from_secs(24);
    let initial = h.ready_after(&mut gl, None);
    click(&mut h);
    h.wait(&mut gl, "large first target click", true, |s| {
        s.events.len() == 3
    });
    assert_eq!(h.events[1].1[2..], [0x110, 1]);
    assert_eq!(h.events[2].1[2..], [0x110, 0]);
    let (source, expected) = sized_pattern((800, 600), (800, 600));
    repaint_pixels(&mut h, &source);
    let started = Instant::now();
    let purposes = h.purposes.clone();
    let purpose_of = |seq| {
        purposes
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.0 == seq)
            .unwrap()
            .1
    };
    let mut floor = initial;
    let mut batches = 0usize;
    loop {
        let mut repair = None;
        let mut repaired_pixels = Vec::new();
        while repair.is_none() {
            checked_tick(
                &mut h,
                &mut gl,
                &mut |pixels, seq| {
                    if seq > floor && purpose_of(seq) == FramePurpose::VisualRepair {
                        repair = Some(seq);
                        repaired_pixels = pixels.to_vec();
                    }
                    true
                },
                deadline,
                "large next actual PNG batch",
            );
        }
        let repair = repair.unwrap();
        batches += 1;
        let mut proof = None;
        // Withhold the NEW proof while allowing the real repair feedback to
        // reach the client. A PNG ACK must not reopen the pointer gate.
        loop {
            checked_tick(
                &mut h,
                &mut gl,
                &mut |pixels, seq| {
                    if seq > repair && purpose_of(seq) == FramePurpose::NativeProof {
                        assert_eq!(
                            pixels, repaired_pixels,
                            "fresh proof must retain repaired pixels"
                        );
                        proof = Some(seq);
                        false
                    } else {
                        true
                    }
                },
                deadline,
                "large PNG presentation without proof feedback",
            );
            if h.frames.lock().unwrap().1 == Some(repair) && proof.is_some() {
                break;
            }
        }
        assert!(
            h.ready.lock().unwrap().is_none(),
            "batch {batches}: PNG cannot grant Ready"
        );
        assert_eq!(h.events.len(), 3, "no intermediate clicks");
        assert!(remote_pointer::active(&h.native.state));
        let proof = proof.unwrap();
        while *h.ready.lock().unwrap() != Some(proof) {
            checked_tick(
                &mut h,
                &mut gl,
                &mut |pixels, seq| {
                    assert_eq!(seq, proof);
                    assert_eq!(pixels, repaired_pixels);
                    true
                },
                deadline,
                "large exact new proof presented and Ready",
            );
        }
        assert_eq!(h.frames.lock().unwrap().1, Some(proof));
        floor = proof;
        let sse: u64 = repaired_pixels
            .iter()
            .zip(&expected)
            .map(|(a, b)| {
                let d = i64::from(*a) - i64::from(*b);
                (d * d) as u64
            })
            .sum();
        assert_eq!(repaired_pixels.len(), expected.len());
        eprintln!(
            "pointer_lossless_800x600 batch={batches} repair={repair} proof={proof} sse={sse} elapsed_ms={}",
            started.elapsed().as_millis()
        );
        if sse == 0 {
            break;
        }
    }
    assert!(
        batches > 1,
        "representative image must exercise multiple PNG batches"
    );
    let convergence = started.elapsed();
    click(&mut h);
    while h.events.len() < 6 {
        checked_tick(
            &mut h,
            &mut gl,
            &mut |pixels, _| {
                assert_eq!(pixels, expected);
                true
            },
            deadline,
            "large final real target click",
        );
    }
    assert_eq!(h.events.len(), 6);
    assert_eq!(h.events[4].1[2..], [0x110, 1]);
    assert_eq!(h.events[5].1[2..], [0x110, 0]);
    assert!(remote_pointer::active(&h.native.state));
    eprintln!(
        "pointer_lossless_800x600 completed_batches={batches} clickable_actions=2 exact_sse=0 app_commit_to_final_ready_ms={}",
        convergence.as_millis()
    );
    h.close(&mut gl);
}

#[path = "host_wayland_pointer_lossless_expiry_integration.rs"]
mod expiry_integration;

#[path = "host_wayland_pointer_lossless_scroll_integration.rs"]
mod scroll_integration;
