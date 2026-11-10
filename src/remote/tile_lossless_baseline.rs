//! Offline preparation only: production JPEG tiles versus a separate PNG image.
//! Not a production lossless codec, negotiated payload, or physical latency test.
use crate::remote::{
    frame::{BoundedPayloadWriter, new_decode_buffer_pool},
    tile::{TileDecoder, TileEncodeRequest, TileEncoder},
    x11_capture::CapturedFrame,
};
use image::{Rgb, RgbImage};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{io::Cursor, time::Instant};
const W: u16 = 320;
const H: u16 = 180;
const CAP: usize = 1024 * 1024;
// Handwritten 5x7 bitmap letters C,O,D,E, not a sampled application/font.
const GLYPHS: [[u8; 7]; 4] = [
    [14, 17, 16, 16, 16, 17, 14],
    [14, 17, 17, 17, 17, 17, 14],
    [30, 17, 17, 17, 17, 17, 30],
    [31, 16, 16, 30, 16, 16, 31],
];
fn fixture(scene: u8) -> CapturedFrame {
    let image = RgbImage::from_fn(u32::from(W), u32::from(H), |x, y| {
        if scene == 2 {
            // Incompressible-ish negative control, still deterministic.
            let mut v = u64::from(x + y * u32::from(W)) + 0x827ab925;
            v = (v ^ (v >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
            v = (v ^ (v >> 27)).wrapping_mul(0x94d049bb133111eb);
            return Rgb([(v >> 8) as u8, (v >> 24) as u8, (v >> 40) as u8]);
        }
        let gx = (x / 6 % 4) as usize;
        let row = (y % 12) as usize;
        let glyph = row < 7 && x % 6 < 5 && GLYPHS[gx][row] & (1 << (4 - x % 6)) != 0;
        let line = y % 23 == 22 || x % 47 == 46;
        if line {
            Rgb([220, 35, 90])
        } else if glyph {
            if scene == 0 {
                Rgb([20, 25, 30])
            } else {
                Rgb([230, 235, 240])
            }
        } else if scene == 0 {
            Rgb([245, 245, 245])
        } else {
            Rgb([18, 20, 22])
        }
    });
    CapturedFrame {
        image,
        source_width: W,
        source_height: H,
        geometry_epoch: 1,
    }
}
fn distortion(a: &[u8], b: &[u8]) -> serde_json::Value {
    assert_eq!(a.len(), b.len());
    let mut sse = 0u64;
    let mut changed = 0u64;
    let mut max = 0u8;
    for (&a, &b) in a.iter().zip(b) {
        let d = a.abs_diff(b);
        sse += u64::from(d) * u64::from(d);
        changed += u64::from(d != 0);
        max = max.max(d);
    }
    json!({"sse":sse,"channel_samples":a.len(),"changed_channels":changed,"max_channel_error":max,"mse":sse as f64/a.len()as f64})
}
fn png_roundtrip(rgb: &RgbImage) -> (usize, serde_json::Value, u128, u128) {
    let mut bytes = Vec::new();
    let start = Instant::now();
    {
        let mut encoder = png::Encoder::new(
            BoundedPayloadWriter::new(&mut bytes, CAP),
            rgb.width(),
            rgb.height(),
        );
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(rgb.as_raw()).unwrap();
        writer.finish().unwrap();
    }
    let encode = start.elapsed().as_nanos();
    let start = Instant::now();
    let mut decoder = png::Decoder::new(Cursor::new(&bytes));
    decoder.set_limits(png::Limits { bytes: CAP });
    let mut reader = decoder.read_info().unwrap();
    assert_eq!(
        (reader.info().width, reader.info().height),
        (u32::from(W), u32::from(H))
    );
    let size = reader.output_buffer_size().unwrap();
    assert_eq!(size, usize::from(W) * usize::from(H) * 3);
    let mut decoded = vec![0; size];
    let info = reader.next_frame(&mut decoded).unwrap();
    assert_eq!(info.color_type, png::ColorType::Rgb);
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    let decode = start.elapsed().as_nanos();
    let metric = distortion(rgb.as_raw(), &decoded[..info.buffer_size()]);
    assert_eq!(metric["sse"], 0);
    (bytes.len(), metric, encode, decode)
}
#[test]
#[ignore = "bounded offline codec evidence, explicit opt-in; no sockets or GL"]
fn offline_static_text_production_jpeg_and_separate_png_baseline() {
    let mut rows = Vec::new();
    for (scene_id, scene) in [
        "synthetic_light_glyphs_lines",
        "synthetic_dark_glyphs_lines",
        "synthetic_noise_negative",
    ]
    .into_iter()
    .enumerate()
    {
        let frame = fixture(scene_id as u8);
        let hash: String = Sha256::digest(frame.image.as_raw())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        for quality in [75, 90, 100] {
            for trial in 0..5 {
                let mut encoder = TileEncoder::new();
                let mut decoder = TileDecoder::new();
                let pool = new_decode_buffer_pool();
                let start = Instant::now();
                let plan = encoder.plan(&frame, TileEncodeRequest::Keyframe).unwrap();
                let plan_ns = start.elapsed().as_nanos();
                assert!(plan.keyframe);
                let mut payload = Vec::new();
                let start = Instant::now();
                encoder
                    .encode_into(&mut payload, 1, &frame, quality)
                    .unwrap();
                let encode_ns = start.elapsed().as_nanos();
                assert!(payload.len() <= CAP);
                encoder.commit(&frame);
                let start = Instant::now();
                let decoded = decoder.decode_into(&payload, pool).unwrap();
                let decode_ns = start.elapsed().as_nanos();
                let start = Instant::now();
                let metric = distortion(frame.image.as_raw(), decoded.image().as_raw());
                let materialize_and_measure_ns = start.elapsed().as_nanos();
                assert!(
                    !encoder.has_quality_debt(quality),
                    "quality target reached is not lossless proof"
                );
                let next = encoder.plan(&frame, TileEncodeRequest::Delta).unwrap();
                assert!(
                    !next.emit,
                    "ordinary unchanged video suppresses after JPEG target"
                );
                rows.push(json!({"scene":scene,"sha256":hash,"codec":"production_tile_jpeg","quality":quality,"trial":trial,"warmup":trial<2,"encoded_bytes":payload.len(),"distortion":metric,"plan_ns":plan_ns,"encode_ns":encode_ns,"decode_ns":decode_ns,"materialize_and_measure_ns":materialize_and_measure_ns,"unchanged_next_emitted":next.emit,"quality_debt":false}));
            }
        }
        for trial in 0..5 {
            let (bytes, metric, encode_ns, decode_ns) = png_roundtrip(&frame.image);
            rows.push(json!({"scene":scene,"sha256":hash,"codec":"separate_full_rgb_png_not_integrated","trial":trial,"warmup":trial<2,"encoded_bytes":bytes,"distortion":metric,"encode_ns":encode_ns,"decode_ns":decode_ns}));
        }
    }
    println!(
        "{}",
        json!({"schema_version":1,"benchmark":"static_text_lossless_preparation","debug_assertions":cfg!(debug_assertions),"dimensions":[W,H],"source":"handwritten synthetic bitmap glyphs/lines and deterministic noise; not representative application coverage","target":"post-scaling encoded RGB; no scaling performed in this fixture","png":"png 0.18 default encoder settings, RGB8, independent full-image container, not production tile payload","byte_scope":"tile payload versus PNG file; neither includes source envelope or authenticated transport","timing_scope":"CPU operations only; JPEG materialization excluded from decode_ns, PNG buffer allocation included; no performance winner inferred","rows":rows})
    );
}
