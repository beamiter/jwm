//! Bounded offline A/B of production scroll-copy; no display, socket or GPU.
//! Run explicitly with `--ignored --nocapture`; timings are CPU work, not frame
//! or physical-presentation latency. Inputs and quality are identical per arm.
use crate::remote::{
    frame::new_decode_buffer_pool,
    messages::FrameLimits,
    tile::{TileDecoder, TileEncodeRequest, TileEncoder},
    x11_capture::CapturedFrame,
};
use image::{Rgb, RgbImage};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::Instant;

const FRAMES: u32 = 8;
const QUALITY: u8 = 75;
const WIDTH: u16 = 321;
const HEIGHT: u16 = 193;
const RGB_BUDGET: u64 = 64 * 1024 * 1024;
const SCENES: [&str; 6] = [
    "static",
    "up_7px",
    "down_7px",
    "random_change",
    "fixed_toolbar",
    "odd_resize",
];

fn mix(mut n: u64) -> u64 {
    n = (n ^ (n >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    n = (n ^ (n >> 27)).wrapping_mul(0x94d049bb133111eb);
    n ^ (n >> 31)
}
fn input(scene: &str, index: u32) -> CapturedFrame {
    let resized = scene == "odd_resize" && index >= 4;
    let (width, height) = if resized { (303, 181) } else { (WIDTH, HEIGHT) };
    let image = RgbImage::from_fn(u32::from(width), u32::from(height), |x, y| {
        let world_y = match scene {
            "up_7px" => y + index * 7,
            "down_7px" => y + 256 - index * 7,
            "fixed_toolbar" if y >= 24 => y + index * 7,
            _ => y,
        };
        let phase = if scene == "random_change" {
            u64::from(index)
        } else {
            0
        };
        let cell = mix(0x6929_a591
            ^ (u64::from(x / 5) << 32)
            ^ u64::from(world_y / 3)
            ^ phase.wrapping_mul(0x9e3779b97f4a7c15));
        Rgb([cell as u8, (cell >> 8) as u8, (cell >> 16) as u8])
    });
    CapturedFrame {
        image,
        source_width: width,
        source_height: height,
        geometry_epoch: if resized { 2 } else { 1 },
    }
}
fn us(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1_000_000.0
}

#[test]
#[ignore = "explicit bounded offline CPU A/B; emits per-frame JSON, not a latency guarantee"]
fn tile_scroll_copy_offline_ab_benchmark() {
    // Budget counts BOTH arms' offered source RGB, including suppressed frames.
    assert!(
        u64::from(WIDTH) * u64::from(HEIGHT) * 3 * u64::from(FRAMES) * SCENES.len() as u64 * 2
            <= RGB_BUDGET
    );
    let mut offered_rgb = 0u64;
    let mut fingerprints = Vec::new();
    for enabled in [false, true] {
        let mode = if enabled {
            "scroll_copy"
        } else {
            "ordinary_tiles"
        };
        let (mut total_bytes, mut total_sse, mut total_samples) = (0u64, 0u64, 0u64);
        let mut total_plan_us = 0.0;
        let mut total_encode_us = 0.0;
        let mut total_decode_us = 0.0;
        let mut total_commit_us = 0.0;
        let mut worst = 0u8;
        let mut all_copies = 0usize;
        let mut scene_fingerprint = 0usize;
        for scene in SCENES {
            let mut encoder = TileEncoder::new();
            let mut decoder = TileDecoder::with_frame_limits(FrameLimits::default()).unwrap();
            encoder.set_scroll_copy_enabled(enabled);
            decoder.set_scroll_copy_enabled(enabled);
            let pool = new_decode_buffer_pool();
            let mut payload = Vec::new();
            let mut sequence = 0u64;
            let mut copies = 0usize;
            for index in 0..FRAMES {
                // Generation, hash and distortion are outside measured codec work.
                // One offered source frame at a time; no replay RGB history.
                let frame = input(scene, index);
                offered_rgb += frame.image.as_raw().len() as u64;
                assert!(offered_rgb <= RGB_BUDGET);
                let digest = Sha256::digest(frame.image.as_raw());
                if enabled {
                    assert_eq!(
                        fingerprints[scene_fingerprint], digest,
                        "A/B must use identical RGB"
                    );
                } else {
                    fingerprints.push(digest);
                }
                scene_fingerprint += 1;
                let started = Instant::now();
                let plan = encoder.plan(&frame, TileEncodeRequest::Delta).unwrap();
                let plan_us = us(started);
                let (mut encode_us, mut decode_us, mut commit_us) = (None, None, None);
                let mut scroll = false;
                payload.clear();
                if plan.emit {
                    sequence += 1;
                    let started = Instant::now();
                    encoder
                        .encode_into(&mut payload, sequence, &frame, QUALITY)
                        .unwrap();
                    encode_us = Some(us(started));
                    assert!(payload.len() >= 32);
                    scroll = payload[17] & 2 != 0;
                    let started = Instant::now();
                    encoder.commit(&frame);
                    commit_us = Some(us(started));
                    let started = Instant::now();
                    let decoded = decoder.decode_into(&payload, pool.clone()).unwrap();
                    decode_us = Some(us(started));
                    assert_eq!(decoded.sequence(), sequence);
                    assert_eq!(decoded.geometry_epoch(), frame.geometry_epoch);
                    assert_eq!(
                        (decoded.source_width(), decoded.source_height()),
                        (frame.source_width, frame.source_height)
                    );
                    // Materialization is outside decode timing.
                    assert_eq!(decoded.image().dimensions(), frame.image.dimensions());
                }
                total_plan_us += plan_us;
                total_encode_us += encode_us.unwrap_or(0.0);
                total_decode_us += decode_us.unwrap_or(0.0);
                total_commit_us += commit_us.unwrap_or(0.0);
                copies += usize::from(scroll);
                let canvas = decoder
                    .canvas()
                    .expect("first frame establishes decoded base");
                assert_eq!(canvas.dimensions(), frame.image.dimensions());
                let mut sse = 0u64;
                let mut max_error = 0u8;
                for (&original, &decoded) in frame.image.as_raw().iter().zip(canvas.as_raw()) {
                    let error = original.abs_diff(decoded);
                    sse += u64::from(error) * u64::from(error);
                    max_error = max_error.max(error);
                }
                let samples = frame.image.as_raw().len() as u64;
                total_bytes += payload.len() as u64;
                total_sse += sse;
                total_samples += samples;
                worst = worst.max(max_error);
                println!(
                    "{}",
                    json!({"type":"scroll_copy_ab_frame", "schema_version":1,
                    "mode":mode,"scene":scene,"index":index,"quality":QUALITY,
                    "width":frame.source_width,"height":frame.source_height,
                    "geometry_epoch":frame.geometry_epoch,"sequence":sequence,
                    "emitted":plan.emit,"keyframe":plan.keyframe,"scroll_copy":scroll,
                    "payload_bytes":payload.len(),"raw_plan_us":plan_us,"encode_us":encode_us,
                    "commit_us":commit_us,"decode_us":decode_us,"sse":sse,
                    "channel_samples":samples,"mse":sse as f64 / samples as f64,
                    "max_abs_error":max_error,"input_sha256":digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>()})
                );
            }
            if enabled && matches!(scene, "up_7px" | "down_7px") {
                assert!(
                    copies > 0,
                    "positive scroll workload must exercise production copy"
                );
            } else {
                assert_eq!(copies, 0, "fallback workload must not use copy");
            }
            all_copies += copies;
        }
        println!(
            "{}",
            json!({"type":"scroll_copy_ab_total","schema_version":1,
            "scope":"offline CPU codec work only; not capture, transport, GPU or presentation latency",
            "debug_assertions":cfg!(debug_assertions),"mode":mode,"quality":QUALITY,
            "offered_frames":SCENES.len() as u32 * FRAMES,"payload_bytes":total_bytes,
            "scroll_copy_frames":all_copies,"raw_plan_us":total_plan_us,"encode_us":total_encode_us,
            "decode_us":total_decode_us,"commit_us":total_commit_us,
            "sse":total_sse,"channel_samples":total_samples,
            "mse":total_sse as f64 / total_samples as f64,"max_abs_error":worst,
            "offered_rgb_budget_both_arms":RGB_BUDGET})
        );
    }
}
