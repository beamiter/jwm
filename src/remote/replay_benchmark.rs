//! Bounded offline synthetic replay of the production tile codec, not desktop latency.
use super::RemoteResult;
use super::frame::new_decode_buffer_pool;
use super::messages::FrameLimits;
use super::tile::{TileDecoder, TileEncodeRequest, TileEncoder};
use super::x11_capture::CapturedFrame;
use image::{Rgb, RgbImage};
use serde::Serialize;
use std::time::Instant;

/// Hard bounds apply to API callers as well as the developer CLI.
#[derive(Clone, Debug, Serialize)]
pub struct ReplayConfig {
    pub width: u16,
    pub height: u16,
    pub frames: u32,
    pub warmup: u32,
    pub seed: u64,
    pub quality: u8,
}
impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            width: 320,
            height: 180,
            frames: 30,
            warmup: 3,
            seed: 1,
            quality: 75,
        }
    }
}
impl ReplayConfig {
    pub fn validate(&self) -> RemoteResult<()> {
        let pixels = u64::from(self.width) * u64::from(self.height);
        if self.width == 0
            || self.height == 0
            || self.width > 1920
            || self.height > 1080
            || self.frames == 0
            || self.frames > 300
            || self.warmup > 30
            || self.quality == 0
            || self.quality > 100
            || pixels * (u64::from(self.frames) + u64::from(self.warmup)) * 5 > 300_000_000
        {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,
                "replay limits: 1..1920 x 1..1080, frames 1..300, warmup 0..30, quality 1..100, at most 300 million generated pixels across five scenes").into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Scene {
    Idle,
    SparseChange,
    Scroll,
    FullChange,
    Resize,
}
const SCENES: [Scene; 5] = [
    Scene::Idle,
    Scene::SparseChange,
    Scene::Scroll,
    Scene::FullChange,
    Scene::Resize,
];

#[derive(Debug, Serialize)]
pub struct TimingStats {
    samples: usize,
    raw_us: Vec<f64>,
    mean_us: Option<f64>,
    p50_us: Option<f64>,
    p95_us: Option<f64>,
    p99_us: Option<f64>,
}
fn stats(values: &[f64]) -> TimingStats {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let percentile =
        |p: usize| (!sorted.is_empty()).then(|| sorted[(sorted.len() * p).div_ceil(100) - 1]);
    TimingStats {
        samples: sorted.len(),
        raw_us: values.to_vec(),
        mean_us: (!sorted.is_empty()).then(|| sorted.iter().sum::<f64>() / sorted.len() as f64),
        p50_us: percentile(50),
        p95_us: percentile(95),
        p99_us: percentile(99),
    }
}
#[derive(Debug, Serialize)]
pub struct SceneReport {
    scene: Scene,
    frames: u32,
    emitted_frames: u32,
    suppressed_frames: u32,
    encoded_bytes: u64,
    dirty_tiles: u64,
    keyframes: u32,
    plan: TimingStats,
    encode: TimingStats,
    atlas_pack: TimingStats,
    jpeg: TimingStats,
    commit: TimingStats,
    decode: TimingStats,
}
#[derive(Debug, Serialize)]
pub struct ReplayReport {
    schema_version: u32,
    workload_version: u32,
    scope: &'static str,
    build_commit: &'static str,
    debug_assertions: bool,
    config: ReplayConfig,
    scenes: Vec<SceneReport>,
}

// SplitMix64 finalizer, fully specified and independent of external RNG versions.
fn mix(mut n: u64) -> u64 {
    n = (n ^ (n >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    n = (n ^ (n >> 27)).wrapping_mul(0x94d049bb133111eb);
    n ^ (n >> 31)
}
fn generate(config: &ReplayConfig, scene: Scene, index: u32) -> CapturedFrame {
    let resized = matches!(scene, Scene::Resize) && (index / 3) % 2 == 1;
    let width = if resized {
        config.width.saturating_sub(17).max(1)
    } else {
        config.width
    };
    let height = if resized {
        config.height.saturating_sub(9).max(1)
    } else {
        config.height
    };
    let image = RgbImage::from_fn(u32::from(width), u32::from(height), |x, y| {
        let y = if matches!(scene, Scene::Scroll) {
            y + index * 7
        } else {
            y
        };
        let phase = if matches!(scene, Scene::FullChange) {
            u64::from(index)
        } else {
            0
        };
        let cell = mix(config.seed
            ^ (u64::from(x / 8) << 32)
            ^ u64::from(y / 8)
            ^ phase.wrapping_mul(0x9e3779b97f4a7c15));
        let mut rgb = [cell as u8, (cell >> 8) as u8, (cell >> 16) as u8];
        if matches!(scene, Scene::SparseChange) && x < 16 && y < 16 {
            rgb = if index % 2 == 0 { [0; 3] } else { [255; 3] };
        }
        Rgb(rgb)
    });
    CapturedFrame {
        geometry_epoch: if matches!(scene, Scene::Resize) {
            u64::from(index / 3)
        } else {
            0
        },
        image,
        source_width: width,
        source_height: height,
    }
}
fn micros(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1_000_000.0
}

fn replay_scene(config: &ReplayConfig, scene: Scene, frames: u32) -> RemoteResult<SceneReport> {
    let mut encoder = TileEncoder::new();
    let mut decoder = TileDecoder::with_frame_limits(FrameLimits::default())?;
    let pool = new_decode_buffer_pool();
    let mut payload = Vec::new();
    let (mut plans, mut encodes, mut packs, mut jpegs, mut commits, mut decodes) = (
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let (mut emitted, mut bytes, mut dirty, mut keys) = (0, 0, 0, 0);
    for index in 0..frames {
        let frame = generate(config, scene, index); // Deliberately outside all measured intervals.
        let start = Instant::now();
        let plan = encoder.plan(&frame, TileEncodeRequest::Delta)?;
        plans.push(micros(start));
        if !plan.emit {
            continue;
        }
        let sequence = u64::from(emitted) + 1;
        let start = Instant::now();
        encoder.encode_into(&mut payload, sequence, &frame, config.quality)?;
        encodes.push(micros(start));
        if let Some(timing) = encoder.encode_timings() {
            packs.push(timing.atlas_pack.as_secs_f64() * 1_000_000.0);
            jpegs.push(timing.jpeg.as_secs_f64() * 1_000_000.0);
        }
        let start = Instant::now();
        encoder.commit(&frame);
        commits.push(micros(start));
        let start = Instant::now();
        let decoded = decoder.decode_into(&payload, pool.clone())?;
        decodes.push(micros(start));
        // Full RGB materialization and validation are deliberately NOT decode timing.
        if decoded.sequence() != sequence
            || decoded.source_width() != frame.source_width
            || decoded.source_height() != frame.source_height
            || decoded.image().dimensions() != frame.image.dimensions()
            || decoded.image().as_raw().len()
                != usize::from(frame.source_width) * usize::from(frame.source_height) * 3
        {
            return Err(
                std::io::Error::other("replay decoded frame failed structural validation").into(),
            );
        }
        emitted += 1;
        bytes += payload.len() as u64;
        dirty += u64::from(plan.dirty_tiles);
        keys += u32::from(plan.keyframe);
    }
    Ok(SceneReport {
        scene,
        frames,
        emitted_frames: emitted,
        suppressed_frames: frames - emitted,
        encoded_bytes: bytes,
        dirty_tiles: dirty,
        keyframes: keys,
        plan: stats(&plans),
        encode: stats(&encodes),
        atlas_pack: stats(&packs),
        jpeg: stats(&jpegs),
        commit: stats(&commits),
        decode: stats(&decodes),
    })
}

/// Warm each scene separately, then reset codec state for its measured replay.
/// This keeps first-frame keyframes and resize behavior reproducible.
pub fn run(config: ReplayConfig) -> RemoteResult<ReplayReport> {
    config.validate()?;
    let mut scenes = Vec::new();
    for scene in SCENES {
        replay_scene(&config, scene, config.warmup)?;
        scenes.push(replay_scene(&config, scene, config.frames)?);
    }
    Ok(ReplayReport {
        schema_version: 1,
        workload_version: 1,
        scope: "offline_synthetic_tile_codec_only",
        build_commit: option_env!("JWM_BUILD_COMMIT").unwrap_or("unknown"),
        debug_assertions: cfg!(debug_assertions),
        config,
        scenes,
    })
}

/// Export the exact production replay generator, not a second implementation.
/// Creates a new directory only, bounds all RGB files to 64 MiB, and writes the
/// manifest last. A failed export may leave partial files; it never overwrites.
pub fn export_fixture(config: ReplayConfig, output: &std::path::Path) -> RemoteResult<()> {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    config.validate()?;
    let bytes =
        u64::from(config.width) * u64::from(config.height) * u64::from(config.frames) * 5 * 3;
    if bytes > 64 * 1024 * 1024 || config.warmup != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "fixture requires warmup=0 and at most 64 MiB RGB across all five scenes",
        )
        .into());
    }
    std::fs::create_dir(output)?;
    let mut entries = Vec::new();
    for scene in SCENES {
        let name = serde_json::to_value(scene)?.as_str().unwrap().to_owned();
        for index in 0..config.frames {
            let frame = generate(&config, scene, index);
            let file = format!("{name}-{index:04}.rgb");
            let rgb = frame.image.as_raw();
            let digest: String = Sha256::digest(rgb)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output.join(&file))?
                .write_all(rgb)?;
            entries.push(serde_json::json!({"scene":name,"index":index,"width":frame.source_width,"height":frame.source_height,"geometry_epoch":frame.geometry_epoch,"file":file,"sha256":digest}));
        }
    }
    let baseline = run(config.clone())?;
    let manifest = serde_json::json!({"schema_version":1,"workload_version":1,"producer":"jwm-production-replay","pixel_format":"rgb24","config":config,"frames":entries,"tile_baseline":baseline,"baseline_order":"fixture export followed by fresh production tile replay; no warmup"});
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output.join("manifest.json"))?;
    serde_json::to_writer_pretty(file, &manifest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixture_export_is_exact_bounded_and_never_overwrites() {
        use sha2::{Digest, Sha256};
        struct OwnedTestDir(std::path::PathBuf);
        impl Drop for OwnedTestDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let path = std::env::temp_dir().join(format!(
            "jwm-replay-fixture-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        let parent = OwnedTestDir(path);
        let output = parent.0.join("fixture");
        let config = ReplayConfig {
            width: 33,
            height: 25,
            frames: 4,
            warmup: 0,
            seed: 7,
            quality: 75,
        };
        export_fixture(config.clone(), &output).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(output.join("manifest.json")).unwrap()).unwrap();
        let frames = manifest["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 20);
        for (scene_index, scene) in SCENES.into_iter().enumerate() {
            for index in 0..4 {
                let entry = &frames[scene_index * 4 + index];
                let bytes = std::fs::read(output.join(entry["file"].as_str().unwrap())).unwrap();
                assert_eq!(
                    bytes,
                    *generate(&config, scene, index as u32).image.as_raw()
                );
                let hex: String = Sha256::digest(&bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                assert_eq!(entry["sha256"], hex);
            }
        }
        let original = std::fs::read(output.join("manifest.json")).unwrap();
        assert!(export_fixture(config.clone(), &output).is_err());
        assert_eq!(
            std::fs::read(output.join("manifest.json")).unwrap(),
            original
        );
        let invalid = parent.0.join("invalid");
        assert!(
            export_fixture(
                ReplayConfig {
                    width: 1920,
                    height: 1080,
                    frames: 10,
                    ..config.clone()
                },
                &invalid
            )
            .is_err()
        );
        assert!(!invalid.exists());
        assert!(
            export_fixture(
                ReplayConfig {
                    warmup: 1,
                    ..config
                },
                &invalid
            )
            .is_err()
        );
        assert!(!invalid.exists());
        assert_eq!(
            manifest["tile_baseline"]["scenes"][0]["plan"]["raw_us"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
    }
    #[test]
    fn deterministic_seeded_scenes() {
        let config = ReplayConfig {
            width: 33,
            height: 25,
            ..ReplayConfig::default()
        };
        for scene in SCENES {
            for index in 0..7 {
                assert_eq!(
                    generate(&config, scene, index).image,
                    generate(&config, scene, index).image
                );
            }
        }
        assert_ne!(
            generate(&config, Scene::Idle, 0).image,
            generate(&ReplayConfig { seed: 2, ..config }, Scene::Idle, 0).image
        );
    }
    #[test]
    fn scene_changes_follow_the_documented_schedule() {
        let config = ReplayConfig {
            width: 33,
            height: 25,
            ..ReplayConfig::default()
        };
        assert_eq!(
            generate(&config, Scene::Idle, 0).image,
            generate(&config, Scene::Idle, 1).image
        );
        let first = generate(&config, Scene::SparseChange, 0);
        let second = generate(&config, Scene::SparseChange, 1);
        for (x, y, pixel) in first.image.enumerate_pixels() {
            assert_eq!(pixel != second.image.get_pixel(x, y), x < 16 && y < 16);
        }
        assert_ne!(
            generate(&config, Scene::Scroll, 0).image,
            generate(&config, Scene::Scroll, 1).image
        );
        assert_ne!(
            generate(&config, Scene::FullChange, 0).image,
            generate(&config, Scene::FullChange, 1).image
        );
        assert_eq!(
            generate(&config, Scene::Resize, 2).image.dimensions(),
            (33, 25)
        );
        assert_eq!(
            generate(&config, Scene::Resize, 3).image.dimensions(),
            (16, 16)
        );
        assert_eq!(generate(&config, Scene::Resize, 6).geometry_epoch, 2);
    }

    #[test]
    fn quantiles_and_empty_json_are_explicit() {
        let value = serde_json::to_value(stats(&[])).unwrap();
        assert_eq!(value["samples"], 0);
        assert!(value["mean_us"].is_null());
        let value = serde_json::to_value(stats(&[4.0, 1.0, 3.0, 2.0])).unwrap();
        assert_eq!(value["mean_us"], 2.5);
        assert_eq!(value["p50_us"], 2.0);
        assert_eq!(value["p95_us"], 4.0);
        assert_eq!(value["p99_us"], 4.0);
    }
    #[test]
    fn bounded_replay_smoke_and_json_counts() {
        let config = ReplayConfig {
            width: 33,
            height: 25,
            frames: 7,
            warmup: 1,
            ..ReplayConfig::default()
        };
        let report = run(config).unwrap();
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["scenes"].as_array().unwrap().len(), 5);
        assert_eq!(report.scenes[0].emitted_frames, 1);
        assert_eq!(report.scenes[0].suppressed_frames, 6);
        assert_eq!(report.scenes[1].emitted_frames, 7);
        assert_eq!(report.scenes[4].keyframes, 3);
        for scene in &report.scenes {
            assert!(scene.encoded_bytes > 0);
            assert_eq!(scene.plan.samples, 7);
            assert_eq!(scene.decode.samples, scene.emitted_frames as usize);
        }
    }
    #[test]
    fn rejects_unbounded_work_before_allocating() {
        for config in [
            ReplayConfig {
                width: 0,
                ..ReplayConfig::default()
            },
            ReplayConfig {
                frames: u32::MAX,
                ..ReplayConfig::default()
            },
            ReplayConfig {
                warmup: u32::MAX,
                ..ReplayConfig::default()
            },
            ReplayConfig {
                width: 1920,
                height: 1080,
                frames: 300,
                ..ReplayConfig::default()
            },
            ReplayConfig {
                quality: 101,
                ..ReplayConfig::default()
            },
        ] {
            assert!(run(config).is_err());
        }
    }
}
