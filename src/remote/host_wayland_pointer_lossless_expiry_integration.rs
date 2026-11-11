//! Representative 1080p partial repair, then the original natural lease expiry.
//! Real protocol/GL execution belongs to CI. No clock mutation or lease renewal.
use super::*;

fn exactness(pixels: &[u8], expected: &[u8]) -> (usize, u64) {
    assert_eq!(pixels.len(), expected.len());
    let exact = pixels
        .chunks_exact(4)
        .zip(expected.chunks_exact(4))
        .filter(|(a, b)| a == b)
        .count();
    let sse = pixels
        .iter()
        .zip(expected)
        .map(|(a, b)| {
            let d = i64::from(*a) - i64::from(*b);
            (d * d) as u64
        })
        .sum();
    (exact, sse)
}

#[test]
fn actual_pointer_lossless_1080p_partial_progress_and_natural_lease_expiry() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_sized(
        &mut gl,
        (false, false),
        (false, false),
        true,
        true,
        (1920, 1080),
        (1920, 1080),
    );
    h.ready_after(&mut gl, None);
    let timing = remote_pointer::lease_timing_for_test(&h.native.state)
        .expect("real native lease must exist");
    assert_eq!(timing.2.duration_since(timing.1), Duration::from_secs(30));
    let client_timing = h
        .pointer_lifetime
        .lock()
        .unwrap()
        .expect("actual first client pointer request must have been observed");
    assert_eq!(
        client_timing.1.duration_since(client_timing.0),
        Duration::from_secs(30)
    );
    assert!(
        client_timing.0 <= timing.1,
        "native grant cannot precede the actual client request"
    );
    let first_deadline = timing.2.min(client_timing.1);
    // New expiry test: original thirty seconds plus the existing five-second
    // teardown allowance. This does not change any earlier 24-second test.
    let deadline = first_deadline + Duration::from_secs(5);
    click(&mut h);
    h.wait(&mut gl, "1080p actual initial click", true, |s| {
        s.events.len() == 3
    });
    assert_eq!(h.events[1].1[2..], [0x110, 1]);
    assert_eq!(h.events[2].1[2..], [0x110, 0]);
    let (source, expected) = sized_pattern((1920, 1080), (1920, 1080));
    repaint_pixels(&mut h, &source);
    let purposes = h.purposes.clone();
    let mut repairs = Vec::new();
    let mut repair_waiting = None;
    let mut completed_cycles = 0usize;
    loop {
        let mut observed_repair = None;
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
                if purpose == FramePurpose::VisualRepair
                    && repairs
                        .last()
                        .is_none_or(|r: &(u64, usize, u64)| r.0 != seq)
                {
                    observed_repair = Some((seq, pixels.to_vec()));
                }
                true
            },
            deadline,
            "1080p normal progress to original natural expiry",
        );
        // The callback returned true and sent genuine feedback for this exact
        // readback sequence before the expensive full-image CPU oracle runs.
        if let Some((seq, pixels)) = observed_repair {
            assert!(
                repair_waiting.is_none(),
                "previous repair must complete proof/Ready before the next batch"
            );
            let (exact, sse) = exactness(&pixels, &expected);
            if let Some(previous) = repairs.last() {
                assert!(exact > previous.1, "actual PNG must add exact pixels");
                assert!(sse < previous.2, "actual PNG must reduce full-image SSE");
            }
            assert!(
                exact > 0 && sse > 0,
                "1080p case demonstrates partial, not complete, repair"
            );
            repairs.push((seq, exact, sse));
            repair_waiting = Some(seq);
            eprintln!("pointer_lossless_1080p repair={seq} exact_pixels={exact} sse={sse}");
        }
        if let Some(repair) = repair_waiting
            && let Some(ready) = *h.ready.lock().unwrap()
            && ready > repair
        {
            assert_eq!(h.frames.lock().unwrap().1, Some(ready));
            assert_eq!(
                purposes
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|r| r.0 == ready)
                    .unwrap()
                    .1,
                FramePurpose::NativeProof
            );
            completed_cycles += 1;
            repair_waiting = None;
        }
        assert_eq!(h.events.len(), 3, "no extra action while repairing");
        assert_eq!(
            *h.pointer_lifetime.lock().unwrap(),
            Some(client_timing),
            "client request/deadline must never renew"
        );
        if let Some(current) = remote_pointer::lease_timing_for_test(&h.native.state) {
            assert_eq!(
                current, timing,
                "repair must never replace/renew the native lease"
            );
        } else {
            assert!(
                Instant::now() >= first_deadline,
                "native authority ended before either actual original deadline"
            );
            break;
        }
    }
    assert!(
        repairs.len() >= 2 && completed_cycles >= 2,
        "need real multi-batch progress before expiry"
    );
    // No feedback is intentionally delayed across a one-second proof deadline.
    // This checks natural expiry with normal feedback, not an invented race.
    while h.ready.lock().unwrap().is_some() {
        checked_tick(
            &mut h,
            &mut gl,
            &mut |_, _| true,
            deadline,
            "expired lease reaches viewer",
        );
        assert!(!remote_pointer::active(&h.native.state));
    }
    let before = h.frames.lock().unwrap().1.unwrap();
    let buttons_before = *h.button_dispatch.lock().unwrap();
    click(&mut h); // real viewer events after expiry, never a direct network action
    // A new source commit, actual GL readback and client presentation prove
    // that the client loop remains alive after the rejected local click.
    let red: Vec<u8> = [0, 0, 255, 255].repeat((1920 * 1080) as usize);
    repaint_pixels(&mut h, &red);
    let mut red_sequence = None;
    loop {
        checked_tick(
            &mut h,
            &mut gl,
            &mut |pixels, seq| {
                if seq > before
                    && pixels
                        .chunks_exact(4)
                        .all(|p| p[0] <= 3 && p[1] <= 3 && p[2] >= 250 && p[3] == 255)
                {
                    red_sequence = Some(seq);
                }
                true
            },
            deadline,
            "expired pointer remains view-only with actual new video",
        );
        assert!(!remote_pointer::active(&h.native.state));
        assert!(h.ready.lock().unwrap().is_none());
        assert_eq!(h.events.len(), 3, "post-expiry click cannot reach target");
        if red_sequence.is_some()
            && h.frames.lock().unwrap().1 == red_sequence
            && *h.button_dispatch.lock().unwrap() == (buttons_before.0 + 1, buttons_before.1 + 1)
        {
            break;
        }
    }
    eprintln!(
        "pointer_lossless_1080p epoch={} repairs={} completed_cycles={} elapsed_ms={} final_partial_sse={} client_to_native_grant_us={}",
        timing.0,
        repairs.len(),
        completed_cycles,
        timing.1.elapsed().as_millis(),
        repairs.last().unwrap().2,
        timing.1.duration_since(client_timing.0).as_micros()
    );
    h.close(&mut gl);
}

#[test]
fn expired_repair_and_proof_ack_cannot_restore_phase() {
    use crate::remote::pointer_repair_phase::{Error, RepairPhase, Scope, VisualKind};
    let t = Instant::now();
    let deadline = t + Duration::from_secs(30);
    let now = deadline - Duration::from_millis(400);
    let ready = Scope {
        source: 1,
        geometry: 2,
        sequence: 10,
    };
    let visual = Scope {
        sequence: 11,
        ..ready
    };
    let proof = Scope {
        sequence: 12,
        ..ready
    };
    for ack in [visual, proof] {
        let mut phase = RepairPhase::new(1, deadline);
        let token = phase.offer(ready, 1, now, t, true, now).unwrap().unwrap();
        phase.request(token, ready, 1, true, now, now).unwrap();
        phase
            .visual_flushed(token, visual, VisualKind::Repair, now)
            .unwrap();
        phase
            .native_proof(
                token,
                proof,
                now + Duration::from_millis(1),
                now + Duration::from_millis(2),
            )
            .unwrap();
        assert_eq!(
            phase.native_proof_ack(token, ack, deadline),
            Err(Error::Deadline)
        );
        assert_eq!(
            phase.native_proof_ack(token, proof, deadline),
            Err(Error::Ended)
        );
    }
}
