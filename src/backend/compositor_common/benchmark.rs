use crate::application::BenchmarkRequest;
use serde::Serialize;
use std::collections::HashMap;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BenchmarkState {
    Idle,
    Warmup { remaining: u32 },
    Running { target_frames: u32, collected: u32 },
    Complete,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlurCostSample {
    pub pixel_count: u64,
    pub time_ms: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrameTimeStats {
    pub count: u32,
    pub avg_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub stddev_ms: f64,
    pub fps_avg: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LatencyStats {
    pub count: u32,
    pub avg_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BlurStats {
    pub cost_per_megapixel_ms: f64,
    pub avg_total_ms: f64,
    pub cache_hit_rate: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ZoneReport {
    pub avg_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub p99_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GLStatsReport {
    pub draw_calls_per_frame: f64,
    pub state_changes_per_frame: f64,
    pub texture_binds_per_frame: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemInfo {
    pub gpu: String,
    pub driver: String,
    pub resolution: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchmarkConfig {
    pub blur_enabled: bool,
    pub blur_strength: u32,
    /// Tracked compositor windows at the end of the first measured frame.
    /// With no measured frames, retain the caller's start-time snapshot.
    /// This is neither a visible-window count nor an average over the run.
    pub window_count: usize,
    pub hdr_enabled: bool,
    pub vrr_active: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchmarkReport {
    pub version: String,
    pub timestamp: String,
    pub system: SystemInfo,
    pub config: BenchmarkConfig,
    pub frame_time: FrameTimeStats,
    pub input_latency: LatencyStats,
    pub blur: BlurStats,
    pub zones: HashMap<String, ZoneReport>,
    pub gl: GLStatsReport,
}

pub struct BenchmarkHarness {
    state: BenchmarkState,
    target_frames: u32,
    frame_times_us: Vec<u64>,
    zone_times: HashMap<String, Vec<f32>>,
    blur_cost_samples: Vec<BlurCostSample>,
    input_latency_samples: Vec<f32>,
    gl_draw_calls: Vec<u32>,
    gl_state_changes: Vec<u32>,
    gl_texture_binds: Vec<u32>,
    warmup_frames: u32,
    start_time: Option<Instant>,
    // Populated by caller before report generation
    pub system_info: SystemInfo,
    pub bench_config: BenchmarkConfig,
    pub blur_cache_hits: u64,
    pub blur_cache_misses: u64,
}

impl BenchmarkHarness {
    pub fn new() -> Self {
        Self {
            state: BenchmarkState::Idle,
            target_frames: 0,
            frame_times_us: Vec::new(),
            zone_times: HashMap::new(),
            blur_cost_samples: Vec::new(),
            input_latency_samples: Vec::new(),
            gl_draw_calls: Vec::new(),
            gl_state_changes: Vec::new(),
            gl_texture_binds: Vec::new(),
            warmup_frames: 60,
            start_time: None,
            system_info: SystemInfo {
                gpu: String::new(),
                driver: String::new(),
                resolution: String::new(),
            },
            bench_config: BenchmarkConfig {
                blur_enabled: false,
                blur_strength: 0,
                window_count: 0,
                hdr_enabled: false,
                vrr_active: false,
            },
            blur_cache_hits: 0,
            blur_cache_misses: 0,
        }
    }

    /// Start a benchmark, preserving the original fire-and-forget API.
    ///
    /// Invalid or unallocatable requests are refused and logged without
    /// changing the current run. Internal callers that need the outcome use
    /// [`Self::try_start`].
    pub fn start(&mut self, target_frames: u32, warmup_frames: u32) {
        if let Err(error) = self.try_start(target_frames, warmup_frames) {
            log::warn!("benchmark: refused to start: {error}");
        }
    }

    pub(crate) fn try_start(
        &mut self,
        target_frames: u32,
        warmup_frames: u32,
    ) -> Result<(), String> {
        let request = BenchmarkRequest::new(target_frames, warmup_frames)?;
        let mut frame_times_us = Vec::new();
        frame_times_us
            .try_reserve_exact(request.frames as usize)
            .map_err(|error| format!("failed to reserve benchmark sample buffer: {error}"))?;

        self.state = BenchmarkState::Idle;
        self.frame_times_us = frame_times_us;
        self.zone_times.clear();
        self.blur_cost_samples.clear();
        self.input_latency_samples.clear();
        self.gl_draw_calls.clear();
        self.gl_state_changes.clear();
        self.gl_texture_binds.clear();
        self.blur_cache_hits = 0;
        self.blur_cache_misses = 0;
        self.warmup_frames = request.warmup;
        self.target_frames = request.frames;
        self.state = if request.warmup > 0 {
            BenchmarkState::Warmup {
                remaining: request.warmup,
            }
        } else {
            BenchmarkState::Running {
                target_frames: request.frames,
                collected: 0,
            }
        };
        self.start_time = Some(Instant::now());
        log::info!(
            "benchmark: started (warmup={}, target={})",
            request.warmup,
            request.frames
        );
        Ok(())
    }

    pub fn stop(&mut self) -> Option<BenchmarkReport> {
        if self.state == BenchmarkState::Idle {
            return None;
        }
        let report = self.generate_report();
        self.state = BenchmarkState::Idle;
        Some(report)
    }

    pub fn is_running(&self) -> bool {
        matches!(
            self.state,
            BenchmarkState::Warmup { .. } | BenchmarkState::Running { .. }
        )
    }

    pub fn is_complete(&self) -> bool {
        self.state == BenchmarkState::Complete
    }

    /// Record this frame's supplemental samples before advancing its state.
    ///
    /// In particular, the last warm-up frame must not contribute samples, and
    /// the final measured frame must contribute them before becoming Complete.
    pub(crate) fn finish_frame(&mut self, dt_us: u64, record_samples: impl FnOnce(&mut Self)) {
        if self.is_collecting() {
            record_samples(self);
        }
        self.record_frame(dt_us);
    }

    /// Advance the benchmark by one frame. Record supplemental samples first;
    /// compositor callers use `finish_frame` to preserve that ordering.
    pub fn record_frame(&mut self, dt_us: u64) {
        match &mut self.state {
            BenchmarkState::Warmup { remaining } => {
                if *remaining > 1 {
                    *remaining -= 1;
                } else {
                    let target = self.target_frames;
                    self.state = BenchmarkState::Running {
                        target_frames: target,
                        collected: 0,
                    };
                    self.start_time = Some(Instant::now());
                    log::info!("benchmark: warmup complete, collecting {} frames", target);
                }
            }
            BenchmarkState::Running {
                target_frames,
                collected,
            } => {
                self.frame_times_us.push(dt_us);
                *collected += 1;
                if *target_frames > 0 && *collected >= *target_frames {
                    let c = *collected;
                    let elapsed = self
                        .start_time
                        .map(|t| t.elapsed().as_secs_f32())
                        .unwrap_or(0.0);
                    self.state = BenchmarkState::Complete;
                    log::info!("benchmark: complete ({} frames in {:.1}s)", c, elapsed);
                }
            }
            _ => {}
        }
    }

    /// Capture the tracked-window inventory once, before the first measured
    /// frame advances its state. Call from the `finish_frame` sample callback;
    /// warmup and later frames must not overwrite this workload snapshot.
    pub(crate) fn record_window_count(&mut self, window_count: usize) {
        if matches!(self.state, BenchmarkState::Running { collected: 0, .. }) {
            self.bench_config.window_count = window_count;
        }
    }

    pub fn record_zone(&mut self, name: &str, time_ms: f32) {
        if !self.is_collecting() {
            return;
        }
        self.zone_times
            .entry(name.to_string())
            .or_default()
            .push(time_ms);
    }

    pub fn record_blur_cost(&mut self, pixel_count: u64, time_ms: f32) {
        if !self.is_collecting() {
            return;
        }
        self.blur_cost_samples.push(BlurCostSample {
            pixel_count,
            time_ms,
        });
    }

    pub fn record_input_latency(&mut self, latency_ms: f32) {
        if !self.is_collecting() {
            return;
        }
        if latency_ms > 0.0 {
            self.input_latency_samples.push(latency_ms);
        }
    }

    pub fn record_gl_stats(&mut self, draw_calls: u32, state_changes: u32, texture_binds: u32) {
        if !self.is_collecting() {
            return;
        }
        self.gl_draw_calls.push(draw_calls);
        self.gl_state_changes.push(state_changes);
        self.gl_texture_binds.push(texture_binds);
    }

    pub fn generate_report(&self) -> BenchmarkReport {
        BenchmarkReport {
            version: "1.0".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            system: self.system_info.clone(),
            config: self.bench_config.clone(),
            frame_time: self.compute_frame_time_stats(),
            input_latency: self.compute_latency_stats(),
            blur: self.compute_blur_stats(),
            zones: self.compute_zone_reports(),
            gl: self.compute_gl_stats(),
        }
    }

    fn is_collecting(&self) -> bool {
        matches!(self.state, BenchmarkState::Running { .. })
    }

    fn compute_frame_time_stats(&self) -> FrameTimeStats {
        if self.frame_times_us.is_empty() {
            return FrameTimeStats {
                count: 0,
                avg_ms: 0.0,
                min_ms: 0.0,
                max_ms: 0.0,
                p50_ms: 0.0,
                p95_ms: 0.0,
                p99_ms: 0.0,
                stddev_ms: 0.0,
                fps_avg: 0.0,
            };
        }

        let mut sorted: Vec<f64> = self
            .frame_times_us
            .iter()
            .map(|&us| us as f64 / 1000.0)
            .collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let count = sorted.len() as u32;
        let sum: f64 = sorted.iter().sum();
        let avg = sum / sorted.len() as f64;
        let variance: f64 =
            sorted.iter().map(|&x| (x - avg).powi(2)).sum::<f64>() / sorted.len() as f64;

        FrameTimeStats {
            count,
            avg_ms: avg,
            min_ms: sorted[0],
            max_ms: sorted[sorted.len() - 1],
            p50_ms: percentile(&sorted, 0.50),
            p95_ms: percentile(&sorted, 0.95),
            p99_ms: percentile(&sorted, 0.99),
            stddev_ms: variance.sqrt(),
            fps_avg: 1000.0 / avg,
        }
    }

    fn compute_latency_stats(&self) -> LatencyStats {
        if self.input_latency_samples.is_empty() {
            return LatencyStats {
                count: 0,
                avg_ms: 0.0,
                p50_ms: 0.0,
                p95_ms: 0.0,
                p99_ms: 0.0,
            };
        }

        let mut sorted: Vec<f64> = self
            .input_latency_samples
            .iter()
            .map(|&x| x as f64)
            .collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let avg = sorted.iter().sum::<f64>() / sorted.len() as f64;

        LatencyStats {
            count: sorted.len() as u32,
            avg_ms: avg,
            p50_ms: percentile(&sorted, 0.50),
            p95_ms: percentile(&sorted, 0.95),
            p99_ms: percentile(&sorted, 0.99),
        }
    }

    fn compute_blur_stats(&self) -> BlurStats {
        if self.blur_cost_samples.is_empty() {
            let total = self.blur_cache_hits + self.blur_cache_misses;
            let hit_rate = if total > 0 {
                self.blur_cache_hits as f64 / total as f64 * 100.0
            } else {
                0.0
            };
            return BlurStats {
                cost_per_megapixel_ms: 0.0,
                avg_total_ms: 0.0,
                cache_hit_rate: hit_rate,
            };
        }

        let total_pixels: f64 = self
            .blur_cost_samples
            .iter()
            .map(|s| s.pixel_count as f64)
            .sum();
        let total_time: f64 = self
            .blur_cost_samples
            .iter()
            .map(|s| s.time_ms as f64)
            .sum();
        let megapixels = total_pixels / 1_000_000.0;
        let cost_per_mp = if megapixels > 0.0 {
            total_time / megapixels
        } else {
            0.0
        };
        let avg_total = total_time / self.blur_cost_samples.len() as f64;

        let total_cache = self.blur_cache_hits + self.blur_cache_misses;
        let hit_rate = if total_cache > 0 {
            self.blur_cache_hits as f64 / total_cache as f64 * 100.0
        } else {
            0.0
        };

        BlurStats {
            cost_per_megapixel_ms: cost_per_mp,
            avg_total_ms: avg_total,
            cache_hit_rate: hit_rate,
        }
    }

    fn compute_zone_reports(&self) -> HashMap<String, ZoneReport> {
        let mut reports = HashMap::new();
        for (name, samples) in &self.zone_times {
            if samples.is_empty() {
                continue;
            }
            let mut sorted: Vec<f64> = samples.iter().map(|&x| x as f64).collect();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let avg = sorted.iter().sum::<f64>() / sorted.len() as f64;
            reports.insert(
                name.clone(),
                ZoneReport {
                    avg_ms: avg,
                    min_ms: sorted[0],
                    max_ms: sorted[sorted.len() - 1],
                    p99_ms: percentile(&sorted, 0.99),
                },
            );
        }
        reports
    }

    fn compute_gl_stats(&self) -> GLStatsReport {
        let avg = |v: &[u32]| -> f64 {
            if v.is_empty() {
                0.0
            } else {
                v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64
            }
        };
        GLStatsReport {
            draw_calls_per_frame: avg(&self.gl_draw_calls),
            state_changes_per_frame: avg(&self.gl_state_changes),
            texture_binds_per_frame: avg(&self.gl_texture_binds),
        }
    }
}

impl Default for BenchmarkHarness {
    fn default() -> Self {
        Self::new()
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (p * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::{BenchmarkHarness, BenchmarkState};
    use crate::application::BenchmarkRequest;

    #[test]
    fn start_rejects_invalid_sizes_before_reserving_samples() {
        let mut harness = BenchmarkHarness::new();
        let initial_capacity = harness.frame_times_us.capacity();

        assert!(harness.try_start(0, 0).is_err());
        assert!(harness.try_start(u32::MAX, 0).is_err());
        assert!(
            harness
                .try_start(1, BenchmarkRequest::MAX_WARMUP_FRAMES + 1)
                .is_err()
        );
        assert_eq!(harness.state, BenchmarkState::Idle);
        assert_eq!(harness.frame_times_us.capacity(), initial_capacity);
    }

    #[test]
    fn start_accepts_valid_request_and_completes_collection() {
        let mut harness = BenchmarkHarness::new();
        harness.start(2, 0);

        harness.record_frame(16_000);
        assert!(harness.is_running());
        harness.record_frame(17_000);

        assert!(harness.is_complete());
        assert_eq!(harness.frame_times_us, vec![16_000, 17_000]);
    }

    #[test]
    fn finish_frame_includes_every_sample_in_a_one_frame_run() {
        let mut harness = BenchmarkHarness::new();
        harness.start(1, 0);
        harness.finish_frame(16_000, |frame| {
            frame.record_input_latency(7.0);
            frame.record_zone("render", 3.0);
            frame.record_gl_stats(11, 5, 2);
            frame.record_blur_cost(1_000_000, 4.0);
        });

        assert!(harness.is_complete());
        assert_eq!(harness.frame_times_us, [16_000]);
        assert_eq!(harness.input_latency_samples, [7.0]);
        assert_eq!(harness.zone_times["render"], [3.0]);
        assert_eq!(harness.gl_draw_calls, [11]);
        assert_eq!(harness.gl_state_changes, [5]);
        assert_eq!(harness.gl_texture_binds, [2]);
        assert_eq!(harness.blur_cost_samples.len(), 1);
        let report = harness.generate_report();
        assert_eq!(report.frame_time.count, 1);
        assert_eq!(report.input_latency.count, 1);
        assert_eq!(report.gl.draw_calls_per_frame, 11.0);
        assert_eq!(report.zones["render"].avg_ms, 3.0);
        assert_eq!(report.blur.cost_per_megapixel_ms, 4.0);
    }

    #[test]
    fn finish_frame_excludes_warmup_and_includes_the_final_measured_frame() {
        let mut harness = BenchmarkHarness::new();
        harness.start(2, 2);
        let mut collected_metadata = Vec::new();
        for value in [90_u32, 80, 10, 20] {
            harness.finish_frame(u64::from(value) * 1000, |frame| {
                collected_metadata.push(value);
                frame.record_input_latency(value as f32);
                frame.record_zone("render", value as f32);
                frame.record_gl_stats(value, value + 1, value + 2);
                frame.record_blur_cost(1_000_000, value as f32);
            });
        }

        assert!(harness.is_complete());
        assert_eq!(collected_metadata, [10, 20]);
        assert_eq!(harness.frame_times_us, [10_000, 20_000]);
        assert_eq!(harness.input_latency_samples, [10.0, 20.0]);
        assert_eq!(harness.zone_times["render"], [10.0, 20.0]);
        assert_eq!(harness.gl_draw_calls, [10, 20]);
        assert_eq!(harness.gl_state_changes, [11, 21]);
        assert_eq!(harness.gl_texture_binds, [12, 22]);
        assert_eq!(harness.blur_cost_samples.len(), 2);
        let report = harness.generate_report();
        assert_eq!(report.frame_time.count, 2);
        assert_eq!(report.frame_time.avg_ms, 15.0);
        assert_eq!(report.input_latency.count, 2);
        assert_eq!(report.input_latency.avg_ms, 15.0);
        assert_eq!(report.zones["render"].avg_ms, 15.0);
        assert_eq!(report.gl.draw_calls_per_frame, 15.0);
    }

    #[test]
    fn finish_frame_never_invokes_metadata_outside_collection() {
        let mut harness = BenchmarkHarness::new();
        harness.finish_frame(90_000, |_| panic!("idle callback"));
        harness.start(1, 1);
        harness.finish_frame(80_000, |_| panic!("warmup callback"));
        assert_eq!(
            harness.state,
            BenchmarkState::Running {
                target_frames: 1,
                collected: 0,
            }
        );
        harness.finish_frame(10_000, |frame| frame.record_gl_stats(9, 8, 7));
        harness.finish_frame(70_000, |_| panic!("complete callback"));
        assert!(harness.is_complete());
        assert_eq!(harness.frame_times_us, [10_000]);
        assert_eq!(harness.gl_draw_calls, [9]);
        harness.stop();
        harness.finish_frame(60_000, |_| panic!("stopped callback"));
    }

    #[test]
    fn finish_frame_without_fresh_input_does_not_repeat_a_latency_sample() {
        let mut harness = BenchmarkHarness::new();
        harness.start(3, 0);
        for latency in [Some(5.0), None, Some(9.0)] {
            harness.finish_frame(16_000, |frame| {
                if let Some(latency) = latency {
                    frame.record_input_latency(latency);
                }
                frame.record_gl_stats(1, 0, 0);
            });
        }

        assert!(harness.is_complete());
        assert_eq!(harness.input_latency_samples, [5.0, 9.0]);
        let report = harness.generate_report();
        assert_eq!(report.frame_time.count, 3);
        assert_eq!(report.input_latency.count, 2);
        assert_eq!(report.input_latency.avg_ms, 7.0);
    }

    #[test]
    fn finish_frame_restart_clears_samples_and_uses_new_warmup() {
        let mut harness = BenchmarkHarness::new();
        harness.start(1, 0);
        harness.finish_frame(10_000, |frame| {
            frame.record_input_latency(5.0);
            frame.record_zone("old", 4.0);
            frame.record_gl_stats(3, 2, 1);
        });
        harness.start(1, 1);
        harness.finish_frame(80_000, |_| panic!("restarted warmup callback"));
        harness.finish_frame(20_000, |frame| {
            frame.record_zone("new", 2.0);
            frame.record_gl_stats(6, 5, 4);
        });

        assert!(harness.is_complete());
        assert_eq!(harness.frame_times_us, [20_000]);
        assert!(harness.input_latency_samples.is_empty());
        assert!(!harness.zone_times.contains_key("old"));
        assert_eq!(harness.zone_times["new"], [2.0]);
        assert_eq!(harness.gl_draw_calls, [6]);
    }

    #[test]
    fn window_count_captures_the_first_and_final_single_frame() {
        let mut harness = BenchmarkHarness::new();
        harness.start(1, 0);
        harness.bench_config.window_count = 0;
        harness.finish_frame(10_000, |frame| frame.record_window_count(3));
        assert!(harness.is_complete());
        let report = harness.generate_report();
        assert_eq!(report.frame_time.count, 1);
        assert_eq!(report.config.window_count, 3);
    }

    #[test]
    fn window_count_excludes_warmup_and_does_not_follow_later_inventory() {
        let mut harness = BenchmarkHarness::new();
        harness.start(2, 2);
        harness.bench_config.window_count = 9;
        for count in [8, 7] {
            harness.finish_frame(10_000, |frame| frame.record_window_count(count));
            assert_eq!(harness.generate_report().config.window_count, 9);
        }
        // An empty first measured inventory is valid, not an unset sentinel.
        harness.finish_frame(10_000, |frame| frame.record_window_count(0));
        harness.finish_frame(10_000, |frame| frame.record_window_count(5));
        harness.finish_frame(10_000, |frame| frame.record_window_count(6));
        let report = harness.generate_report();
        assert_eq!(report.frame_time.count, 2);
        assert_eq!(report.config.window_count, 0);
    }

    #[test]
    fn window_count_without_measured_frames_keeps_the_start_snapshot() {
        for (warmup, rendered) in [(0, 0), (2, 1), (2, 2)] {
            let mut harness = BenchmarkHarness::new();
            harness.start(1, warmup);
            harness.bench_config.window_count = 9;
            for _ in 0..rendered {
                harness.finish_frame(10_000, |frame| frame.record_window_count(3));
            }
            let report = harness.stop().expect("started benchmark");
            assert_eq!(report.frame_time.count, 0);
            assert_eq!(report.config.window_count, 9);
        }
    }

    #[test]
    fn window_count_restart_captures_the_new_first_measured_inventory() {
        let mut harness = BenchmarkHarness::new();
        harness.start(1, 0);
        harness.finish_frame(10_000, |frame| frame.record_window_count(3));
        assert_eq!(harness.generate_report().config.window_count, 3);
        harness.start(2, 1);
        harness.bench_config.window_count = 9;
        harness.finish_frame(10_000, |frame| frame.record_window_count(8));
        assert_eq!(harness.generate_report().config.window_count, 9);
        harness.finish_frame(10_000, |frame| frame.record_window_count(4));
        harness.finish_frame(10_000, |frame| frame.record_window_count(7));
        assert_eq!(harness.generate_report().config.window_count, 4);
    }

    #[test]
    fn window_count_ignores_inactive_calls_and_survives_a_refused_restart() {
        let mut harness = BenchmarkHarness::new();
        harness.bench_config.window_count = 9;
        harness.record_window_count(1);
        assert_eq!(harness.generate_report().config.window_count, 9);
        harness.start(2, 1);
        harness.record_window_count(2);
        assert_eq!(harness.generate_report().config.window_count, 9);
        harness.finish_frame(10_000, |_| {});
        harness.finish_frame(10_000, |frame| frame.record_window_count(3));
        assert!(harness.try_start(0, 0).is_err());
        harness.finish_frame(10_000, |frame| frame.record_window_count(4));
        harness.record_window_count(5);
        assert_eq!(harness.generate_report().config.window_count, 3);
        harness.stop();
        harness.record_window_count(6);
        assert_eq!(harness.generate_report().config.window_count, 3);
    }
}
