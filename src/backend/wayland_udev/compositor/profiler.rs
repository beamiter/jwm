use std::collections::{HashMap, VecDeque};
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct ZoneStats {
    pub avg_ms: f32,
    pub min_ms: f32,
    pub max_ms: f32,
    pub sample_count: u32,
}

const MAX_SAMPLES: usize = 120;

pub struct FrameProfiler {
    enabled: bool,
    frame_start: Option<Instant>,
    current_frame: HashMap<&'static str, f32>,
    zones: HashMap<&'static str, VecDeque<f32>>,
    active_zone: Option<(&'static str, Instant)>,
    last_frame_ms: f32,
}

impl FrameProfiler {
    pub fn new() -> Self {
        Self {
            enabled: false,
            frame_start: None,
            current_frame: HashMap::new(),
            zones: HashMap::new(),
            active_zone: None,
            last_frame_ms: 0.0,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.current_frame.clear();
            self.active_zone = None;
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn begin_frame(&mut self) {
        if !self.enabled {
            return;
        }
        self.current_frame.clear();
        self.active_zone = None;
        self.frame_start = Some(Instant::now());
    }

    pub fn end_frame(&mut self) -> f32 {
        if !self.enabled {
            return 0.0;
        }
        let elapsed = match self.frame_start.take() {
            Some(start) => start.elapsed().as_secs_f32() * 1000.0,
            None => 0.0,
        };
        self.last_frame_ms = elapsed;
        elapsed
    }

    pub fn zone_start(&mut self, name: &'static str) {
        if !self.enabled {
            return;
        }
        self.active_zone = Some((name, Instant::now()));
    }

    pub fn zone_end(&mut self) {
        if !self.enabled {
            return;
        }
        if let Some((name, start)) = self.active_zone.take() {
            let duration_ms = start.elapsed().as_secs_f32() * 1000.0;
            self.record_zone_duration(name, duration_ms);
        }
    }

    fn record_zone_duration(&mut self, name: &'static str, duration_ms: f32) {
        if !self.enabled {
            return;
        }
        *self.current_frame.entry(name).or_default() += duration_ms;
        let samples = self
            .zones
            .entry(name)
            .or_insert_with(|| VecDeque::with_capacity(MAX_SAMPLES));
        if samples.len() >= MAX_SAMPLES {
            samples.pop_front();
        }
        samples.push_back(duration_ms);
    }

    /// Fresh zone totals for the current frame, without rolling HUD history.
    /// Missing zones produce no sample; repeated visits are summed per frame.
    pub(crate) fn frame_zone_times(&self) -> impl Iterator<Item = (&'static str, f32)> + '_ {
        self.current_frame
            .iter()
            .filter(move |_| self.enabled)
            .map(|(&name, &duration_ms)| (name, duration_ms))
    }

    pub fn zone_stats(&self, name: &'static str) -> Option<ZoneStats> {
        let samples = self.zones.get(name)?;
        if samples.is_empty() {
            return None;
        }
        let sample_count = samples.len() as u32;
        let sum: f32 = samples.iter().sum();
        let avg_ms = sum / sample_count as f32;
        let min_ms = samples.iter().copied().fold(f32::INFINITY, f32::min);
        let max_ms = samples.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        Some(ZoneStats {
            avg_ms,
            min_ms,
            max_ms,
            sample_count,
        })
    }

    pub fn all_zone_stats(&self) -> Vec<(&'static str, ZoneStats)> {
        let mut stats = Vec::new();
        for (&name, samples) in &self.zones {
            if samples.is_empty() {
                continue;
            }
            let sample_count = samples.len() as u32;
            let sum: f32 = samples.iter().sum();
            let avg_ms = sum / sample_count as f32;
            let min_ms = samples.iter().copied().fold(f32::INFINITY, f32::min);
            let max_ms = samples.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            stats.push((
                name,
                ZoneStats {
                    avg_ms,
                    min_ms,
                    max_ms,
                    sample_count,
                },
            ));
        }
        stats.sort_by(|a, b| a.0.cmp(b.0));
        stats
    }

    pub fn frame_report(&self) -> String {
        let mut report = String::new();
        report.push_str(&format!("Frame time: {:.2} ms\n", self.last_frame_ms));
        report.push_str("Zones:\n");
        for (name, stats) in self.all_zone_stats() {
            report.push_str(&format!(
                "  {}: avg={:.3}ms min={:.3}ms max={:.3}ms samples={}\n",
                name, stats.avg_ms, stats.min_ms, stats.max_ms, stats.sample_count
            ));
        }
        report
    }

    pub fn clear_history(&mut self) {
        self.zones.clear();
        self.last_frame_ms = 0.0;
    }

    pub fn last_frame_ms(&self) -> f32 {
        self.last_frame_ms
    }
}

impl Default for FrameProfiler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod frame_zone_tests {
    use super::*;
    fn record(p: &mut FrameProfiler, ms: u64) {
        p.record_zone_duration("windows", ms as f32);
    }
    fn frame(p: &mut FrameProfiler, ms: u64) {
        p.begin_frame();
        record(p, ms);
        p.end_frame();
    }
    fn sample(p: &FrameProfiler) -> Option<f32> {
        p.frame_zone_times()
            .find(|(name, _)| *name == "windows")
            .map(|(_, ms)| ms)
    }
    #[test]
    fn measured_zone_excludes_slow_warmup() {
        let mut p = FrameProfiler::new();
        p.set_enabled(true);
        frame(&mut p, 100);
        frame(&mut p, 1);
        assert_eq!(sample(&p), Some(1.0));
        assert_eq!(p.zone_stats("windows").unwrap().avg_ms, 50.5);
    }
    #[test]
    fn absent_zone_is_not_replayed_on_a_later_frame() {
        let mut p = FrameProfiler::new();
        p.set_enabled(true);
        frame(&mut p, 1);
        p.begin_frame();
        p.end_frame();
        assert_eq!(sample(&p), None);
        assert!(p.zone_stats("windows").is_some());
    }
    #[test]
    fn repeated_zone_visits_sum_within_one_frame() {
        let mut p = FrameProfiler::new();
        p.set_enabled(true);
        p.begin_frame();
        record(&mut p, 2);
        record(&mut p, 3);
        p.end_frame();
        assert_eq!(sample(&p), Some(5.0));
    }
    #[test]
    fn disabled_profiling_exposes_no_fresh_zones() {
        let mut p = FrameProfiler::new();
        p.set_enabled(true);
        frame(&mut p, 1);
        p.set_enabled(false);
        record(&mut p, 9);
        assert_eq!(sample(&p), None);
    }
    #[test]
    fn reenabled_frame_starts_without_previous_samples() {
        let mut p = FrameProfiler::new();
        p.set_enabled(true);
        frame(&mut p, 8);
        p.set_enabled(false);
        p.set_enabled(true);
        p.begin_frame();
        p.end_frame();
        assert_eq!(sample(&p), None);
        frame(&mut p, 2);
        assert_eq!(sample(&p), Some(2.0));
    }
    #[test]
    fn benchmark_report_excludes_warmup_and_absent_zones() {
        use crate::backend::compositor_common::benchmark::BenchmarkHarness;
        let mut profiler = FrameProfiler::new();
        profiler.set_enabled(true);
        let mut benchmark = BenchmarkHarness::new();
        benchmark.start(2, 1);
        for duration in [Some(100), Some(1), None] {
            profiler.begin_frame();
            if let Some(ms) = duration {
                record(&mut profiler, ms);
            }
            profiler.end_frame();
            benchmark.finish_frame(1_000, |sample| {
                for (zone, ms) in profiler.frame_zone_times() {
                    sample.record_zone(zone, ms);
                }
            });
        }
        let report = benchmark.generate_report();
        assert_eq!(report.frame_time.count, 2);
        let zone = &report.zones["windows"];
        assert_eq!(
            (zone.avg_ms, zone.min_ms, zone.max_ms, zone.p99_ms),
            (1.0, 1.0, 1.0, 1.0)
        );
        assert_eq!(profiler.zone_stats("windows").unwrap().avg_ms, 50.5);
    }

    #[test]
    fn restarted_benchmark_does_not_reuse_previous_zone_history() {
        use crate::backend::compositor_common::benchmark::BenchmarkHarness;
        let mut profiler = FrameProfiler::new();
        profiler.set_enabled(true);
        let mut benchmark = BenchmarkHarness::new();
        for ms in [100, 2] {
            benchmark.start(1, 0);
            frame(&mut profiler, ms);
            benchmark.finish_frame(1_000, |sample| {
                for (zone, ms) in profiler.frame_zone_times() {
                    sample.record_zone(zone, ms);
                }
            });
            assert_eq!(
                benchmark.generate_report().zones["windows"].avg_ms,
                ms as f64
            );
        }
        assert_eq!(profiler.zone_stats("windows").unwrap().avg_ms, 51.0);
    }

    #[test]
    fn unfinished_zone_cannot_cross_a_frame_boundary() {
        let mut profiler = FrameProfiler::new();
        profiler.set_enabled(true);
        profiler.begin_frame();
        profiler.zone_start("abandoned");
        profiler.begin_frame();
        profiler.zone_end();
        profiler.end_frame();
        assert_eq!(profiler.frame_zone_times().count(), 0);
        assert!(profiler.zone_stats("abandoned").is_none());
    }
}
