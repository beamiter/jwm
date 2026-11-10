//! Offline policy A/B: same frames and production codec, simulated observation
//! clock. Codec durations are real CPU-wall samples, convergence is model time.
use super::*;
use crate::remote::frame::new_decode_buffer_pool;
use image::Rgb;

#[test]
#[ignore = "explicit bounded offline policy evidence; no sockets, GL or sleeps"]
fn offline_blinking_tile_exact_stability_ab() {
    let mut reports = Vec::new();
    for legacy_global_gate in [true, false] {
        let mut encoder = TileEncoder::new();
        encoder.set_lossless_enabled(true);
        let mut decoder = TileDecoder::new();
        decoder.set_lossless_enabled(true);
        let base = Instant::now();
        let mut last_global_change = base;
        let mut last_repair = None;
        let mut sequence = 0;
        let mut rows = Vec::new();
        let mut first_exact_ms = None;
        let mut input_hash = Sha256::new();
        let mut total_bytes = 0;
        let mut cursor_generations = Vec::new();
        for tick in 0..=100u64 {
            let ms = tick * 100;
            let now = base + Duration::from_millis(ms);
            let generation = ms / 500;
            let cursor_on = generation % 2 == 1;
            let image = RgbImage::from_fn(160, 96, |x, y| {
                if x < 16 && y < 16 {
                    return Rgb([if cursor_on { 224 } else { 32 }; 3]);
                }
                // Synthetic fine strokes and small block glyph-like cells, not
                // application screenshots or font-rendering coverage.
                let ink = x % 13 == 1 || y % 11 == 2 || ((x / 5 + y / 7) % 3 == 0 && x % 5 == 0);
                if ink {
                    Rgb([17, 51, 93])
                } else {
                    Rgb([239, 241, 243])
                }
            });
            input_hash.update(image.as_raw());
            let frame = CapturedFrame {
                geometry_epoch: 1,
                source_width: 160,
                source_height: 96,
                image,
            };
            let started = Instant::now();
            let request = if tick == 0 {
                TileEncodeRequest::Keyframe
            } else {
                TileEncodeRequest::Delta
            };
            let mut plan = encoder.plan_at(&frame, request, now).unwrap();
            if encoder.detected_exact_change() {
                last_global_change = now;
            }
            let stable = if legacy_global_gate {
                now.duration_since(last_global_change) >= Duration::from_secs(2)
            } else {
                encoder.has_stable_exact_debt(now)
            };
            let cadence =
                last_repair.is_none_or(|at| now.duration_since(at) >= Duration::from_millis(100));
            if !plan.keyframe && plan.dirty_tiles == 0 && stable && cadence {
                plan = encoder
                    .plan_at(&frame, TileEncodeRequest::LosslessRepair, now)
                    .unwrap();
            }
            let plan_ns = started.elapsed().as_nanos() as u64;
            if !plan.emit {
                continue;
            }
            sequence += 1;
            let mut payload = Vec::new();
            let start = Instant::now();
            encoder
                .encode_into(&mut payload, sequence, &frame, 75)
                .unwrap();
            let encode_ns = start.elapsed().as_nanos() as u64;
            let start = Instant::now();
            let decoded = decoder
                .decode_into(&payload, new_decode_buffer_pool())
                .unwrap();
            let decode_ns = start.elapsed().as_nanos() as u64;
            encoder.commit(&frame);
            let png = decoder.last_frame_was_lossless();
            if png {
                last_repair = Some(now);
            }
            let mut static_sse = 0u64;
            for (x, y, pixel) in frame.image.enumerate_pixels() {
                if x < 16 && y < 16 {
                    continue;
                }
                for (a, b) in pixel.0.into_iter().zip(decoded.image().get_pixel(x, y).0) {
                    static_sse += i64::from(a).abs_diff(i64::from(b)).pow(2);
                }
            }
            if static_sse == 0 {
                first_exact_ms.get_or_insert(ms);
            }
            let observed_on = decoded.image().get_pixel(8, 8).0[0] > 128;
            assert_eq!(observed_on, cursor_on, "cursor update was lost");
            if tick % 5 == 0 {
                cursor_generations.push(generation);
            }
            total_bytes += payload.len();
            rows.push(serde_json::json!({"model_ms":ms,"sequence":sequence,"png":png,"dirty_tiles":plan.dirty_tiles,
                "tile_payload_bytes":payload.len(),"static_sse":static_sse,"cursor_generation":generation,
                "cursor_on":observed_on,"plan_ns":plan_ns,"encode_ns":encode_ns,"decode_ns":decode_ns}));
        }
        assert_eq!(cursor_generations, (0..=20).collect::<Vec<_>>());
        if legacy_global_gate {
            assert_eq!(first_exact_ms, None);
        } else {
            assert!(first_exact_ms.is_some_and(|ms| (2000..=2500).contains(&ms)));
        }
        let hash = input_hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        reports.push(serde_json::json!({"legacy_global_gate":legacy_global_gate,"input_sequence_sha256":hash,
            "first_static_exact_model_ms":first_exact_ms,"tile_payload_bytes":total_bytes,"cursor_generations":cursor_generations,"frames":rows}));
    }
    assert_eq!(
        reports[0]["input_sequence_sha256"],
        reports[1]["input_sequence_sha256"]
    );
    println!(
        "TILE_STABILITY_AB_JSON={}",
        serde_json::json!({"schema":1,"debug_assertions":cfg!(debug_assertions),
        "dimensions":[160,96],"observation_interval_model_ms":100,"blink_period_model_ms":500,"duration_model_ms":10000,
        "scope":"same production codec; old global eligibility policy vs per-tile policy; simulated convergence clock; no network or presentation",
        "arms":reports})
    );
}
