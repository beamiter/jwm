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

    pub fn start(&mut self, target_frames: u32, warmup_frames: u32) {
        self.frame_times_us.clear();
        self.zone_times.clear();
        self.blur_cost_samples.clear();
        self.input_latency_samples.clear();
        self.gl_draw_calls.clear();
        self.gl_state_changes.clear();
        self.gl_texture_binds.clear();
        self.blur_cache_hits = 0;
        self.blur_cache_misses = 0;
        self.warmup_frames = warmup_frames;
        self.target_frames = target_frames;
        self.frame_times_us.reserve(target_frames as usize);
        self.state = if warmup_frames > 0 {
            BenchmarkState::Warmup { remaining: warmup_frames }
        } else {
            BenchmarkState::Running { target_frames, collected: 0 }
        };
        self.start_time = Some(Instant::now());
        log::info!("benchmark: started (warmup={}, target={})", warmup_frames, target_frames);
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
        matches!(self.state, BenchmarkState::Warmup { .. } | BenchmarkState::Running { .. })
    }

    pub fn is_complete(&self) -> bool {
        self.state == BenchmarkState::Complete
    }

    pub fn record_frame(&mut self, dt_us: u64) {
        match &mut self.state {
            BenchmarkState::Warmup { remaining } => {
                if *remaining > 1 {
                    *remaining -= 1;
                } else {
                    let target = self.target_frames;
                    self.state = BenchmarkState::Running { target_frames: target, collected: 0 };
                    self.start_time = Some(Instant::now());
                    log::info!("benchmark: warmup complete, collecting {} frames", target);
                }
            }
            BenchmarkState::Running { target_frames, collected } => {
                self.frame_times_us.push(dt_us);
                *collected += 1;
                if *target_frames > 0 && *collected >= *target_frames {
                    let c = *collected;
                    let elapsed = self.start_time.map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.0);
                    self.state = BenchmarkState::Complete;
                    log::info!("benchmark: complete ({} frames in {:.1}s)", c, elapsed);
                }
            }
            _ => {}
        }
    }

    pub fn record_zone(&mut self, name: &str, time_ms: f32) {
        if !self.is_collecting() { return; }
        self.zone_times.entry(name.to_string()).or_default().push(time_ms);
    }

    pub fn record_blur_cost(&mut self, pixel_count: u64, time_ms: f32) {
        if !self.is_collecting() { return; }
        self.blur_cost_samples.push(BlurCostSample { pixel_count, time_ms });
    }

    pub fn record_input_latency(&mut self, latency_ms: f32) {
        if !self.is_collecting() { return; }
        if latency_ms > 0.0 {
            self.input_latency_samples.push(latency_ms);
        }
    }

    pub fn record_gl_stats(&mut self, draw_calls: u32, state_changes: u32, texture_binds: u32) {
        if !self.is_collecting() { return; }
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
                count: 0, avg_ms: 0.0, min_ms: 0.0, max_ms: 0.0,
                p50_ms: 0.0, p95_ms: 0.0, p99_ms: 0.0, stddev_ms: 0.0, fps_avg: 0.0,
            };
        }

        let mut sorted: Vec<f64> = self.frame_times_us.iter().map(|&us| us as f64 / 1000.0).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let count = sorted.len() as u32;
        let sum: f64 = sorted.iter().sum();
        let avg = sum / sorted.len() as f64;
        let variance: f64 = sorted.iter().map(|&x| (x - avg).powi(2)).sum::<f64>() / sorted.len() as f64;

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
            return LatencyStats { count: 0, avg_ms: 0.0, p50_ms: 0.0, p95_ms: 0.0, p99_ms: 0.0 };
        }

        let mut sorted: Vec<f64> = self.input_latency_samples.iter().map(|&x| x as f64).collect();
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
            let hit_rate = if total > 0 { self.blur_cache_hits as f64 / total as f64 * 100.0 } else { 0.0 };
            return BlurStats { cost_per_megapixel_ms: 0.0, avg_total_ms: 0.0, cache_hit_rate: hit_rate };
        }

        let total_pixels: f64 = self.blur_cost_samples.iter().map(|s| s.pixel_count as f64).sum();
        let total_time: f64 = self.blur_cost_samples.iter().map(|s| s.time_ms as f64).sum();
        let megapixels = total_pixels / 1_000_000.0;
        let cost_per_mp = if megapixels > 0.0 { total_time / megapixels } else { 0.0 };
        let avg_total = total_time / self.blur_cost_samples.len() as f64;

        let total_cache = self.blur_cache_hits + self.blur_cache_misses;
        let hit_rate = if total_cache > 0 { self.blur_cache_hits as f64 / total_cache as f64 * 100.0 } else { 0.0 };

        BlurStats {
            cost_per_megapixel_ms: cost_per_mp,
            avg_total_ms: avg_total,
            cache_hit_rate: hit_rate,
        }
    }

    fn compute_zone_reports(&self) -> HashMap<String, ZoneReport> {
        let mut reports = HashMap::new();
        for (name, samples) in &self.zone_times {
            if samples.is_empty() { continue; }
            let mut sorted: Vec<f64> = samples.iter().map(|&x| x as f64).collect();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let avg = sorted.iter().sum::<f64>() / sorted.len() as f64;
            reports.insert(name.clone(), ZoneReport {
                avg_ms: avg,
                min_ms: sorted[0],
                max_ms: sorted[sorted.len() - 1],
                p99_ms: percentile(&sorted, 0.99),
            });
        }
        reports
    }

    fn compute_gl_stats(&self) -> GLStatsReport {
        let avg = |v: &[u32]| -> f64 {
            if v.is_empty() { 0.0 } else { v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64 }
        };
        GLStatsReport {
            draw_calls_per_frame: avg(&self.gl_draw_calls),
            state_changes_per_frame: avg(&self.gl_state_changes),
            texture_binds_per_frame: avg(&self.gl_texture_binds),
        }
    }
}

impl Default for BenchmarkHarness {
    fn default() -> Self { Self::new() }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() { return 0.0; }
    let idx = (p * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}
