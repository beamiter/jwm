//! Vertical wheel consent remains independent while PNG repairs are video only.
use super::*;

fn assert_wheel_800(events: &[(u16, Vec<u32>)], step: i32) {
    assert_eq!(
        events.iter().map(|e| e.0).collect::<Vec<_>>(),
        [2, 6, 8, 4, 5]
    );
    assert_eq!(&events[0].1[1..], &[160 * 256, 120 * 256]);
    assert_eq!(events[1].1, [0]);
    assert_eq!(events[2].1, [0, step as u32]);
    assert_eq!(&events[3].1[1..], &[0, (step * 10 * 256) as u32]);
    assert!(events[4].1.is_empty());
}

#[test]
fn actual_pointer_scroll_lossless_800x600_multibatch_exact_then_opposite_notch() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_sized(
        &mut gl,
        (true, false),
        (true, false),
        true,
        true,
        (800, 600),
        (800, 600),
    );
    // Same bounded test budget as the small fixture, never a lease extension.
    let deadline = Instant::now() + Duration::from_secs(24);
    let initial = h.ready_after(&mut gl, None);
    h.wheel_axis(-1, Axis::VerticalScroll);
    h.wait(&mut gl, "large first target notch", false, |s| {
        s.wire_events.len() >= 5
    });
    assert_wheel_800(&h.wire_events, -1);
    let first_wire = h.wire_events.clone();
    h.wait(&mut gl, "scroll consumes original Ready", false, |s| {
        s.ready.lock().unwrap().is_none()
    });
    h.wheel_axis(1, Axis::VerticalScroll);
    h.wheel_axis(-1, Axis::VerticalScroll);
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
        assert_eq!(
            h.wire_events, first_wire,
            "Waiting wheel must not replay during repair"
        );
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
    h.wheel_axis(1, Axis::VerticalScroll);
    while h.wire_events.len() < 10 {
        checked_tick(
            &mut h,
            &mut gl,
            &mut |pixels, _| {
                assert_eq!(pixels, expected);
                true
            },
            deadline,
            "opposite notch after multi-batch exact repair",
        );
    }
    assert_eq!(h.wire_events.len(), 10);
    assert_wheel_800(&h.wire_events[5..], 1);
    assert_eq!(h.candidate.lock().unwrap().2, 2);
    assert!(remote_pointer::active(&h.native.state));
    eprintln!(
        "pointer_lossless_800x600 completed_batches={batches} actual_notches=2 exact_sse=0 app_commit_to_final_ready_ms={}",
        convergence.as_millis()
    );
    h.close(&mut gl);
}

#[test]
fn actual_pointer_scroll_lossless_waiting_half_group_never_replays() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_sized(
        &mut gl,
        (true, false),
        (true, false),
        true,
        true,
        (SW, SH),
        (VW, VH),
    );
    h.wait(&mut gl, "real unpresented scroll viewer", false, |s| {
        s.pointer.is_some() && !s.feedback.is_empty()
    });
    h.wheel_axis(-1, Axis::VerticalScroll); // full unready group
    let p = h.pointer.as_ref().unwrap().clone();
    p.axis_source(AxisSource::Wheel);
    p.axis_discrete(Axis::VerticalScroll, 1); // incomplete unready group
    h.local.roundtrip();
    let ready = h.ready_after(&mut gl, None);
    assert!(h.wire_events.is_empty());
    p.axis(31, Axis::VerticalScroll, 10.0);
    p.frame(); // must finish discarding the old group, not upgrade it to Ready
    h.local.roundtrip();
    let until = Instant::now() + Duration::from_millis(100);
    while Instant::now() < until {
        h.tick(&mut gl, true);
        assert!(h.wire_events.is_empty());
        assert_eq!(h.candidate.lock().unwrap().2, 0);
    }
    assert_eq!(*h.ready.lock().unwrap(), Some(ready));
    h.wheel_axis(-1, Axis::VerticalScroll);
    h.wait(&mut gl, "new complete authorized wheel group", false, |s| {
        s.wire_events.len() >= 5
    });
    assert_one_wheel(&h.wire_events, -1);
    h.close(&mut gl);
}

#[test]
fn actual_pointer_scroll_lossless_disabled_or_held_wheel_revokes_without_delivery() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for viewer_scroll in [false, true] {
        let mut h = Harness::new_sized(
            &mut gl,
            (true, false),
            (viewer_scroll, false),
            true,
            true,
            (SW, SH),
            (VW, VH),
        );
        let ready = h.ready_after(&mut gl, None);
        if viewer_scroll {
            h.button(true);
            h.wait(&mut gl, "real held left candidate", true, |s| {
                let c = s.candidate.lock().unwrap();
                c.0 == Some(ready) && c.1 && c.2 == 0
            });
        }
        h.wheel_axis(1, Axis::VerticalScroll);
        h.wait(
            &mut gl,
            "disabled or held wheel terminates pointer",
            true,
            |s| !remote_pointer::active(&s.native.state) && s.ready.lock().unwrap().is_none(),
        );
        assert!(h.wire_events.is_empty());
        if viewer_scroll {
            h.button(false);
        }
        h.wheel_axis(-1, Axis::VerticalScroll);
        let until = Instant::now() + Duration::from_millis(100);
        while Instant::now() < until {
            h.tick(&mut gl, true);
            assert!(h.wire_events.is_empty());
            assert!(!remote_pointer::active(&h.native.state));
        }
        h.close(&mut gl);
    }
}

#[test]
fn actual_pointer_scroll_lossless_requires_host_scroll_permission() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_sized(
        &mut gl,
        (false, false),
        (true, false),
        true,
        true,
        (SW, SH),
        (VW, VH),
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        h.tick(&mut gl, false);
        if let Ok(result) = h.client_done.try_recv() {
            let error = result.expect_err("host must explicitly deny unapproved scroll");
            assert!(
                error.contains("Wayland scroll permission was not mutually negotiated"),
                "wrong denial: {error}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "explicit scroll denial missing");
        thread::yield_now();
    }
    h.shutdown.store(true, Ordering::Release);
    loop {
        h.tick(&mut gl, false);
        if let Ok(result) = h.host_done.try_recv() {
            // Only after the exact client negotiation denial above: the
            // authenticated peer may close via typed EOF or ConnectionReset.
            assert_expected_host_exit(result, false, true);
            break;
        }
        assert!(Instant::now() < deadline, "host denial cleanup timeout");
        thread::yield_now();
    }
    assert!(h.wire_events.is_empty());
    assert!(h.unexpected_wire.is_empty());
    assert!(!remote_pointer::active(&h.native.state));
    h.client.take().unwrap().join().unwrap();
    h.host.take().unwrap().join().unwrap();
}

#[test]
fn actual_pointer_scroll_lossless_lock_and_unmap_aba_cannot_restore_authority() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for lock in [true, false] {
        let mut h = Harness::new_sized(
            &mut gl,
            (true, false),
            (true, false),
            true,
            true,
            (SW, SH),
            (VW, VH),
        );
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
fn actual_pointer_scroll_lossless_offer_partial_and_accepted_groups_never_replay() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_sized(
        &mut gl,
        (true, false),
        (true, false),
        true,
        true,
        (SW, SH),
        (VW, VH),
    );
    h.wait(
        &mut gl,
        "prepresent pointer for real half group",
        false,
        |s| s.pointer.is_some() && !s.feedback.is_empty(),
    );
    let p = h.pointer.as_ref().unwrap().clone();
    p.axis_source(AxisSource::Wheel); // no Frame: genuine viewer-internal busy
    h.local.roundtrip();
    // Ready's real display.sync drains the earlier same-connection AxisSource.
    // New presentation poisons that unfinished group, but cannot make it idle.
    h.ready_after(&mut gl, None);
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        h.tick(&mut gl, true);
        let rows = h.repair_observation.lock().unwrap();
        assert!(
            rows.iter().all(|(kind, _)| *kind == "offered"),
            "unfinished real group must not select or accept Request: {rows:?}"
        );
        if rows.iter().any(|(kind, _)| *kind == "offered") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no actual Offer while half group remains pending"
        );
        assert!(h.wire_events.is_empty());
        thread::yield_now();
    }
    // Complete the poisoned old group. This is not a new valid detent.
    p.axis_discrete(Axis::VerticalScroll, 1);
    p.axis(41, Axis::VerticalScroll, 10.0);
    p.frame();
    h.local.roundtrip();
    let phase_deadline = Instant::now() + Duration::from_secs(4);
    let accepted = loop {
        h.tick(&mut gl, false);
        assert!(h.wire_events.is_empty());
        if let Some((_, token)) = h
            .repair_observation
            .lock()
            .unwrap()
            .iter()
            .find(|(kind, _)| *kind == "accepted")
            .copied()
        {
            break token;
        }
        assert!(
            Instant::now() < phase_deadline,
            "real Request/Accepted missing after group completion"
        );
        thread::yield_now();
    };
    assert!(
        h.repair_observation
            .lock()
            .unwrap()
            .iter()
            .any(|(kind, token)| *kind == "requested" && *token == accepted)
    );
    assert!(h.ready.lock().unwrap().is_none());
    h.wheel_axis(-1, Axis::VerticalScroll); // complete group while phase closed
    p.axis_source(AxisSource::Wheel);
    p.axis_discrete(Axis::VerticalScroll, 1); // partial group across RepairReady
    h.local.roundtrip();
    let recovery_deadline = Instant::now() + Duration::from_secs(4);
    loop {
        h.tick(&mut gl, true);
        assert!(
            h.wire_events.is_empty(),
            "phase input cannot be queued for later"
        );
        if h.ready.lock().unwrap().is_some() {
            break;
        }
        assert!(
            Instant::now() < recovery_deadline,
            "fresh actual proof did not restore Ready"
        );
        thread::yield_now();
    }
    p.axis(42, Axis::VerticalScroll, 10.0);
    p.frame();
    h.local.roundtrip();
    // Ordered tail followed by a new whole notch: if the old partial upgrades
    // into a down notch, exact recipient direction/count below must fail.
    h.wheel_axis(-1, Axis::VerticalScroll);
    h.wait(
        &mut gl,
        "new notch only after accepted repair",
        false,
        |s| s.wire_events.len() >= 5,
    );
    assert_one_wheel(&h.wire_events, -1);
    assert_eq!(h.candidate.lock().unwrap().2, 1);
    h.close(&mut gl);
}
