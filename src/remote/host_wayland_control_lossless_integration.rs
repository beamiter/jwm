//! One real control resource and original recipients, with video-only PNG repair.
use super::*;
use crate::remote::source_wire::FramePurpose;

fn pattern(width: u32, height: u32) -> Vec<u8> {
    (0..height)
        .flat_map(|y| {
            (0..width).flat_map(move |x| {
                if x % 13 == 0 || y % 17 == 0 {
                    [47, 193, 19, 255]
                } else {
                    [173, 37, 211, 255]
                }
            })
        })
        .collect()
}

fn repaint_pixels(h: &mut Harness, pixels: &[u8]) {
    let (w, height) = h.layout.source;
    assert_eq!(pixels.len(), (w * height * 4) as usize);
    let (buffer, mut file) = shm(
        &mut h.native,
        &mut h.target.client,
        [0, 0, 0, 255],
        w,
        height,
    );
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(pixels).unwrap();
    h.target
        .client
        .request(h.target.surface, 1, &[buffer, 0, 0]);
    h.target
        .client
        .request(h.target.surface, 2, &[0, 0, w, height]);
    h.target.client.request(h.target.surface, 6, &[]);
    h.native.roundtrip();
    h.native.roundtrip();
}

fn tick(
    h: &mut Harness,
    gl: &mut HeadlessSmithayGles,
    deadline: Instant,
    phase: &str,
) -> Option<(u64, Vec<u8>)> {
    assert!(Instant::now() < deadline, "{phase}: deadline");
    let mut readback = None;
    h.tick_with_inspector(gl, true, |pixels, seq| {
        readback = Some((seq, pixels.to_vec()));
        false // old boolean is only the blue-colour observation, not feedback permission
    });
    if let Ok(result) = h.client_done.try_recv() {
        panic!("{phase}: client ended {result:?}");
    }
    if let Ok(result) = h.host_done.try_recv() {
        panic!("{phase}: host ended {result:?}");
    }
    h.unexpected.assert_no_input();
    thread::yield_now();
    readback
}

#[test]
fn actual_control_lossless_800x600_click_shift_a_repair_then_click() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_lossless(
        &mut gl,
        FixtureLayout {
            source: (800, 600),
            viewer: (800, 600),
            lossless: (true, true),
        },
    );
    let deadline = Instant::now() + Duration::from_secs(24);
    let initial = h.activate(&mut gl);
    h.click();
    while h.received.pointer_actions.len() < 3 {
        tick(&mut h, &mut gl, deadline, "first original target click");
    }
    assert_eq!(h.received.pointer_actions.len(), 3);
    assert_eq!(
        &h.received.pointer_actions[0].1[1..],
        &[160 * 256, 120 * 256]
    );
    assert_eq!(&h.received.pointer_actions[1].1[2..], &[0x110, 1]);
    assert_eq!(&h.received.pointer_actions[2].1[2..], &[0x110, 0]);
    while !h.ready.lock().unwrap().is_some_and(|seq| seq > initial) {
        tick(
            &mut h,
            &mut gl,
            deadline,
            "new actual proof before keyboard",
        );
    }
    h.key(60, 42, true);
    while h.received.keys.is_empty() {
        tick(&mut h, &mut gl, deadline, "real Shift accepted");
    }
    assert_eq!(h.received.keys, [(42, 1)]);
    while !h
        .repair_observation
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.0 == "completed" && r.2 == 2 && r.3 == 1)
    {
        tick(
            &mut h,
            &mut gl,
            deadline,
            "Shift actual completion while held",
        );
    }
    assert!(
        !h.repair_observation
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.0 == "requested" && r.3 != 0)
    );
    h.key(70, 30, true);
    h.key(72, 30, false);
    h.key(74, 42, false);
    let expected_keys = [(42, 1), (30, 1), (30, 0), (42, 0)];
    while h.received.keys.len() < 4 {
        tick(&mut h, &mut gl, deadline, "Shift+A and release");
    }
    assert_eq!(h.received.keys, expected_keys);
    while !h
        .repair_observation
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.0 == "completed" && r.2 == 5 && r.3 == 0)
    {
        tick(
            &mut h,
            &mut gl,
            deadline,
            "all four key edges completed before repair",
        );
    }
    let expected = pattern(800, 600);
    repaint_pixels(&mut h, &expected);
    let mut batches = 0;
    let mut pending_repair = None;
    let mut exact = false;
    loop {
        if let Some((seq, pixels)) = tick(&mut h, &mut gl, deadline, "multi-batch real PNG repair")
        {
            let purpose = h
                .purposes
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.0 == seq)
                .unwrap()
                .1;
            if purpose == FramePurpose::VisualRepair {
                assert!(
                    pending_repair.is_none(),
                    "prior repair must complete actual proof/Ready"
                );
                batches += 1;
                assert_eq!(pixels.len(), expected.len());
                exact = pixels == expected;
                pending_repair = Some(seq);
            }
        }
        assert_eq!(h.received.keys, expected_keys);
        assert_eq!(h.received.pointer_actions.len(), 3);
        assert!(remote_control::active(&h.native.state));
        assert_eq!(remote_control::attempts_for_test(&h.native.state), 1);
        if let Some(repair) = pending_repair
            && let Some(ready) = *h.ready.lock().unwrap()
            && ready > repair
        {
            assert_eq!(h.frames.lock().unwrap().1, Some(ready));
            assert_eq!(
                h.purposes
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|r| r.0 == ready)
                    .unwrap()
                    .1,
                FramePurpose::NativeProof
            );
            pending_repair = None;
            if exact {
                break;
            }
        }
    }
    assert!(batches > 1);
    h.click();
    while h.received.pointer_actions.len() < 6 {
        tick(
            &mut h,
            &mut gl,
            deadline,
            "second original target click after exact repair",
        );
    }
    assert_eq!(h.received.pointer_actions.len(), 6);
    assert_eq!(&h.received.pointer_actions[4].1[2..], &[0x110, 1]);
    assert_eq!(&h.received.pointer_actions[5].1[2..], &[0x110, 0]);
    assert_eq!(h.received.keys, expected_keys);
    let before = h.received.pointer_actions.clone();
    let surface = viewer_surface(&h.local);
    h.pointer.as_ref().unwrap().leave(80, &surface);
    h.pointer.as_ref().unwrap().frame();
    h.local.roundtrip();
    h.rejected(
        &mut gl,
        &expected_keys,
        &before,
        1,
        "Wayland pointer left own surface",
    );
}

#[test]
fn actual_control_lossless_requires_explicit_host_permission() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_lossless(
        &mut gl,
        FixtureLayout {
            lossless: (false, true),
            ..FixtureLayout::default()
        },
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        h.tick(&mut gl, false);
        h.received.assert_no_input();
        if let Ok(result) = h.client_done.try_recv() {
            let error = result.expect_err("missing explicit host lossless consent");
            assert!(
                error.contains("static lossless permission was not mutually negotiated"),
                "unexpected permission error: {error}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "missing explicit client denial");
        thread::yield_now();
    }
    h.shutdown.store(true, Ordering::Release);
    loop {
        h.tick(&mut gl, false);
        if let Ok(result) = h.host_done.try_recv() {
            if let Err(error) = result {
                let reset = matches!(error.downcast_ref::<crate::remote::protocol::ProtocolError>(),
                    Some(crate::remote::protocol::ProtocolError::Io(e))
                        if e.kind()==std::io::ErrorKind::ConnectionReset);
                assert!(
                    reset || host::is_end_of_stream(&error),
                    "unexpected host error after explicit negotiation denial: {error:?}"
                );
            }
            break;
        }
        assert!(Instant::now() < deadline, "host denial teardown timeout");
        thread::yield_now();
    }
    h.received.assert_no_input();
    h.unexpected.assert_no_input();
    assert_eq!(remote_control::attempts_for_test(&h.native.state), 0);
    assert!(!remote_control::active(&h.native.state));
    h.client.take().unwrap().join().unwrap();
    h.host.take().unwrap().join().unwrap();
}

// Observe the actual host Accepted status, not a label or elapsed idle delay.
fn accepted_without_presenting(h: &mut Harness, gl: &mut HeadlessSmithayGles) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        h.tick(gl, false);
        h.received.assert_no_input();
        h.unexpected.assert_no_input();
        assert!(
            h.client_done.try_recv().is_err(),
            "client ended before Accepted"
        );
        let records = h.repair_observation.lock().unwrap();
        if let Some(accepted) = records.iter().find(|r| r.0 == "accepted") {
            assert_eq!(accepted.2, 0);
            assert_eq!(accepted.3, 0);
            assert!(
                records
                    .iter()
                    .any(|r| r.0 == "requested" && r.1 == accepted.1)
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "real host Accepted was not observed"
        );
        drop(records);
        thread::yield_now();
    }
    assert!(h.ready.lock().unwrap().is_none());
}

#[test]
fn actual_control_lossless_accepted_key_is_terminal_without_delivery() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_lossless(
        &mut gl,
        FixtureLayout {
            lossless: (true, true),
            ..FixtureLayout::default()
        },
    );
    h.activate(&mut gl);
    accepted_without_presenting(&mut h, &mut gl);
    h.key(90, 30, true);
    h.local.roundtrip();
    terminal_repair_input(h, &mut gl);
}

fn terminal_repair_input(mut h: Harness, gl: &mut HeadlessSmithayGles) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        h.tick(gl, false);
        h.received.assert_no_input();
        h.unexpected.assert_no_input();
        if let Ok(result) = h.client_done.try_recv() {
            let error = result.expect_err("input during accepted repair must terminate");
            assert!(
                error.contains("control repair ordering, scope or deadline invalid"),
                "{error}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "accepted-phase input was not rejected"
        );
        thread::yield_now();
    }
    h.shutdown.store(true, Ordering::Release);
    loop {
        h.tick(gl, false);
        h.received.assert_no_input();
        h.unexpected.assert_no_input();
        if let Ok(result) = h.host_done.try_recv() {
            if let Err(error) = result {
                assert!(
                    host::is_end_of_stream(&error),
                    "unexpected host teardown: {error:?}"
                );
            }
            break;
        }
        assert!(
            Instant::now() < deadline,
            "accepted input cleanup did not finish"
        );
        thread::yield_now();
    }
    assert!(!remote_control::active(&h.native.state));
    assert_eq!(remote_control::attempts_for_test(&h.native.state), 1);
    h.client.take().unwrap().join().unwrap();
    h.host.take().unwrap().join().unwrap();
}

#[test]
fn actual_control_lossless_held_shift_release_completes_before_repair() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_lossless(
        &mut gl,
        FixtureLayout {
            lossless: (true, true),
            ..FixtureLayout::default()
        },
    );
    h.activate(&mut gl);
    let deadline = Instant::now() + Duration::from_secs(8);
    h.key(100, 42, true);
    loop {
        tick(&mut h, &mut gl, deadline, "held Shift completion");
        if h.repair_observation
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.0 == "completed" && r.2 == 1 && r.3 == 1)
        {
            break;
        }
    }
    assert_eq!(h.received.keys, [(42, 1)]);
    // Exercise more than the ordinary idle interval with a real held native key.
    // Time is not taken as proof of Accepted: that status is separately observed below.
    let held_until = Instant::now() + Duration::from_millis(2200);
    while Instant::now() < held_until {
        tick(&mut h, &mut gl, deadline, "held Shift cannot enter repair");
        assert_eq!(h.received.keys, [(42, 1)]);
        assert!(
            !h.repair_observation
                .lock()
                .unwrap()
                .iter()
                .any(|r| matches!(r.0, "requested" | "accepted"))
        );
    }
    h.key(110, 42, false);
    loop {
        // Withhold new presentation only after release. Actual native keyup and
        // its exact completion must precede the first real repair request.
        h.tick(&mut gl, false);
        h.unexpected.assert_no_input();
        assert!(
            Instant::now() < deadline,
            "release/repair transition timed out"
        );
        assert!(
            h.client_done.try_recv().is_err(),
            "client ended during release"
        );
        let records = h.repair_observation.lock().unwrap();
        if let Some(index) = records.iter().position(|r| r.0 == "accepted") {
            assert_eq!(h.received.keys, [(42, 1), (42, 0)]);
            assert!(
                records[..index]
                    .iter()
                    .any(|r| r.0 == "completed" && r.2 == 2 && r.3 == 0)
            );
            assert!(
                records[..index]
                    .iter()
                    .filter(|r| r.0 == "requested")
                    .all(|r| r.2 == 2 && r.3 == 0)
            );
            break;
        }
        drop(records);
        thread::yield_now();
    }
    let before = h.received.pointer_actions.clone();
    let surface = viewer_surface(&h.local);
    h.keyboard.as_ref().unwrap().leave(120, &surface);
    h.local.roundtrip();
    h.rejected(
        &mut gl,
        &[(42, 1), (42, 0)],
        &before,
        1,
        "keyboard focus left",
    );
}

#[test]
fn actual_control_lossless_lock_and_unmap_aba_terminate_one_resource() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for lock in [true, false] {
        let mut h = Harness::new_lossless(
            &mut gl,
            FixtureLayout {
                lossless: (true, true),
                ..FixtureLayout::default()
            },
        );
        h.activate(&mut gl);
        accepted_without_presenting(&mut h, &mut gl);
        if lock {
            h.native.state.session_lock_supported = true;
            let mut locker = h.native.connect();
            let manager = locker.bind("ext_session_lock_manager_v1", 1);
            let resource = locker.new_id();
            locker.request(manager, 1, &[resource]);
            h.native.roundtrip();
            h.native.roundtrip();
            assert!(h.native.state.session_locked);
            locker.request(resource, 2, &[]);
            h.native.roundtrip();
            h.native.roundtrip();
            assert!(!h.native.state.session_locked);
        } else {
            h.target.unmap(&mut h.native);
            h.target.remap(&mut h.native);
        }
        assert!(!remote_control::active(&h.native.state));
        // Production lock cleanup restores the stored local pointer location
        // exactly once before clearing focus. It is not remote input.
        let expected = if lock {
            vec![(2u16, vec![0, 1280, 1280])]
        } else {
            Vec::new()
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut client_finished = false;
        let mut host_finished = false;
        loop {
            h.tick(&mut gl, false);
            assert!(
                expected.starts_with(&h.received.pointer_actions),
                "lock={lock}: {:?}",
                h.received.pointer_wire
            );
            assert!(h.received.keys.is_empty());
            h.unexpected.assert_no_input();
            assert!(!remote_control::active(&h.native.state));
            assert_eq!(remote_control::attempts_for_test(&h.native.state), 1);
            if !client_finished && let Ok(result) = h.client_done.try_recv() {
                eprintln!("control lossless terminal capture lock={lock}: {result:?}");
                client_finished = true;
                h.shutdown.store(true, Ordering::Release);
            }
            if !host_finished && let Ok(result) = h.host_done.try_recv() {
                if let Err(error) = result {
                    let unavailable = matches!(host::runtime_close(&error),Some(host::RuntimeClose::Structured(close)) if close.code==crate::remote::close_wire::SessionCloseCode::CaptureUnavailable);
                    assert!(
                        unavailable || host::is_end_of_stream(&error),
                        "unexpected lifecycle host error: {error:?}"
                    );
                }
                host_finished = true;
            }
            if client_finished && host_finished && h.received.pointer_actions == expected {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "lock={lock}: terminal cleanup timeout"
            );
            thread::yield_now();
        }
        h.client.take().unwrap().join().unwrap();
        h.host.take().unwrap().join().unwrap();
    }
}

#[test]
fn actual_control_lossless_original_lease_expires_without_renewal() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_lossless(
        &mut gl,
        FixtureLayout {
            lossless: (true, true),
            ..FixtureLayout::default()
        },
    );
    h.activate(&mut gl);
    let native = remote_control::lease_timing_for_test(&h.native.state).expect("one native lease");
    let client = h
        .control_lifetime
        .lock()
        .unwrap()
        .expect("actual original client request");
    assert_eq!(native.2.duration_since(native.1), Duration::from_secs(30));
    assert_eq!(client.1.duration_since(client.0), Duration::from_secs(30));
    let expires = native.2.min(client.1);
    let deadline = expires + Duration::from_secs(5);
    h.key(140, 30, true);
    h.key(142, 30, false);
    let mut repaired = false;
    let client_result = loop {
        assert!(Instant::now() < deadline, "natural lease cleanup deadline");
        let mut frame = None;
        h.tick_with_inspector(&mut gl, true, |pixels, seq| {
            frame = Some((seq, pixels.to_vec()));
            false
        });
        if let Some((seq, pixels)) = frame {
            let purpose = h
                .purposes
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.0 == seq)
                .unwrap()
                .1;
            if purpose == FramePurpose::VisualRepair {
                assert!(pixels.chunks_exact(4).all(|p| p == [0, 0, 255, 255]));
                repaired = true;
            }
        }
        h.unexpected.assert_no_input();
        assert!(h.received.pointer_actions.is_empty());
        assert!([(30, 1), (30, 0)].starts_with(&h.received.keys));
        assert_eq!(*h.control_lifetime.lock().unwrap(), Some(client));
        assert_eq!(remote_control::attempts_for_test(&h.native.state), 1);
        if let Some(current) = remote_control::lease_timing_for_test(&h.native.state) {
            assert_eq!(current, native);
        } else {
            assert!(
                Instant::now() >= expires,
                "native revoked before either real deadline"
            );
        }
        if let Ok(result) = h.client_done.try_recv() {
            break result;
        }
        if let Ok(result) = h.host_done.try_recv() {
            panic!("host ended before observed client natural TTL: {result:?}");
        }
        thread::yield_now();
    };
    assert!(Instant::now() >= expires, "premature client termination");
    let error = client_result.expect_err("this control API closes strictly at its deadline");
    assert!(
        error.contains("control lease expired"),
        "not a verified natural control deadline: {error}"
    );
    assert!(
        repaired,
        "actual PNG exact presentation must occur before natural expiry"
    );
    assert_eq!(h.received.keys, [(30, 1), (30, 0)]);
    h.shutdown.store(true, Ordering::Release);
    loop {
        h.tick(&mut gl, false);
        assert_eq!(h.received.keys, [(30, 1), (30, 0)]);
        assert!(h.received.pointer_actions.is_empty());
        h.unexpected.assert_no_input();
        if let Ok(result) = h.host_done.try_recv() {
            if let Err(error) = result {
                assert!(
                    host::is_end_of_stream(&error),
                    "unexpected TTL host cleanup: {error:?}"
                );
            }
            break;
        }
        assert!(Instant::now() < deadline, "TTL host cleanup timeout");
        thread::yield_now();
    }
    assert!(!remote_control::active(&h.native.state));
    assert_eq!(remote_control::attempts_for_test(&h.native.state), 1);
    h.client.take().unwrap().join().unwrap();
    h.host.take().unwrap().join().unwrap();
}
