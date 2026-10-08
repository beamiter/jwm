/// Frame Profiler - Track timing of render pipeline stages
///
/// Provides detailed breakdown of where frame time is spent
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Profiling zone guard - automatically records duration when dropped
pub struct ProfileZone<'a> {
    profiler: &'a mut FrameProfiler,
    zone_name: &'static str,
    start: Instant,
}

impl<'a> Drop for ProfileZone<'a> {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        self.profiler.record_zone(self.zone_name, elapsed);
    }
}

/// Frame profiler for tracking render pipeline timing
pub struct FrameProfiler {
    /// Current frame zones: name -> duration
    current_frame: HashMap<&'static str, Duration>,
    /// Historical data: zone -> Vec<duration samples>
    history: HashMap<&'static str, Vec<f32>>,
    /// History buffer size
    max_samples: usize,
    /// Frame start time
    frame_start: Instant,
    /// Enable/disable profiling
    enabled: bool,
}

impl FrameProfiler {
    pub fn new() -> Self {
        Self {
            current_frame: HashMap::new(),
            history: HashMap::new(),
            max_samples: 120, // 2 seconds at 60fps
            frame_start: Instant::now(),
            enabled: false,
        }
    }

    /// Start a new frame
    pub fn begin_frame(&mut self) {
        if !self.enabled {
            return;
        }
        self.current_frame.clear();
        self.frame_start = Instant::now();
    }

    /// Enter a profiling zone, returns a guard that records timing on drop
    pub fn enter(&mut self, zone_name: &'static str) -> ProfileZone<'_> {
        ProfileZone {
            profiler: self,
            zone_name,
            start: Instant::now(),
        }
    }

    /// Manual zone timing - start a zone
    pub fn zone_start(&self, _zone_name: &'static str) -> Instant {
        Instant::now()
    }

    /// Manual zone timing - end a zone and record duration
    pub fn zone_end(&mut self, zone_name: &'static str, start: Instant) {
        if !self.enabled {
            return;
        }
        let duration = start.elapsed();
        *self
            .current_frame
            .entry(zone_name)
            .or_insert(Duration::ZERO) += duration;
    }

    /// Record a zone duration (called automatically by ProfileZone drop)
    fn record_zone(&mut self, zone_name: &'static str, duration: Duration) {
        if !self.enabled {
            return;
        }
        *self
            .current_frame
            .entry(zone_name)
            .or_insert(Duration::ZERO) += duration;
    }

    /// End frame and store results in history
    pub fn end_frame(&mut self) -> f32 {
        if !self.enabled {
            return 0.0;
        }

        let frame_time_ms = self.frame_start.elapsed().as_secs_f32() * 1000.0;

        // Store current frame zones in history
        for (zone, &duration) in &self.current_frame {
            let samples = self.history.entry(zone).or_insert_with(Vec::new);
            samples.push(duration.as_secs_f32() * 1000.0);
            if samples.len() > self.max_samples {
                samples.remove(0);
            }
        }

        frame_time_ms
    }

    /// Fresh zone totals for the current frame, without rolling HUD history.
    /// Missing zones produce no sample; repeated visits are summed per frame.
    pub(crate) fn frame_zone_times(&self) -> impl Iterator<Item = (&'static str, f32)> + '_ {
        self.current_frame
            .iter()
            .filter(move |_| self.enabled)
            .map(|(&name, duration)| (name, duration.as_secs_f32() * 1000.0))
    }

    /// Get statistics for a zone
    pub fn zone_stats(&self, zone: &str) -> Option<ZoneStats> {
        let samples = self.history.get(zone)?;
        if samples.is_empty() {
            return None;
        }

        let avg = samples.iter().sum::<f32>() / samples.len() as f32;
        let min = samples.iter().copied().fold(f32::MAX, f32::min);
        let max = samples.iter().copied().fold(0.0, f32::max);

        Some(ZoneStats {
            avg_ms: avg,
            min_ms: min,
            max_ms: max,
        })
    }

    /// Get all zone statistics
    pub fn all_zone_stats(&self) -> HashMap<&'static str, ZoneStats> {
        self.history
            .keys()
            .filter_map(|&zone| self.zone_stats(zone).map(|stats| (zone, stats)))
            .collect()
    }

    /// Get formatted report for current frame
    pub fn frame_report(&self) -> String {
        if !self.enabled || self.current_frame.is_empty() {
            return String::new();
        }

        let mut lines = vec!["Frame Profile:".to_string()];
        let mut zones: Vec<_> = self.current_frame.iter().collect();
        zones.sort_by_key(|(_, duration)| std::cmp::Reverse(*duration));

        for (zone, duration) in zones {
            let ms = duration.as_secs_f32() * 1000.0;
            lines.push(format!("  {}: {:.2}ms", zone, ms));
        }

        lines.join("\n")
    }

    /// Enable profiling
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.current_frame.clear();
            self.history.clear();
        }
    }

    /// Check if profiling is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Clear all history
    pub fn clear_history(&mut self) {
        self.history.clear();
    }
}

impl Default for FrameProfiler {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics for a profiling zone
#[derive(Debug, Clone, Copy)]
pub struct ZoneStats {
    pub avg_ms: f32,
    pub min_ms: f32,
    pub max_ms: f32,
}

/// Macro for easy profiling
#[macro_export]
macro_rules! profile_zone {
    ($profiler:expr, $name:expr, $code:block) => {{
        let _guard = $profiler.enter($name);
        $code
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn test_profiler_creation() {
        let profiler = FrameProfiler::new();
        assert!(!profiler.is_enabled());
    }

    #[test]
    fn test_profiler_enable() {
        let mut profiler = FrameProfiler::new();
        profiler.set_enabled(true);
        assert!(profiler.is_enabled());
    }

    #[test]
    fn test_zone_recording() {
        let mut profiler = FrameProfiler::new();
        profiler.set_enabled(true);
        profiler.begin_frame();

        // The zone guard reads the wall clock, so the only bounds that hold
        // under any scheduler are the sleep itself (a floor) and a clock that
        // brackets the whole zone (a ceiling). A fixed ceiling such as 20 ms
        // failed whenever a loaded test run woke the thread late.
        let bracket = Instant::now();
        {
            let _zone = profiler.enter("test_zone");
            sleep(Duration::from_millis(10));
        }
        let bracket_ms = bracket.elapsed().as_secs_f32() * 1000.0;

        profiler.end_frame();

        let stats = profiler.zone_stats("test_zone");
        assert!(stats.is_some());
        let stats = stats.unwrap();
        assert!(
            stats.avg_ms >= 9.0,
            "zone shorter than its sleep: {stats:?}"
        );
        assert!(
            stats.avg_ms <= bracket_ms,
            "zone {stats:?} outlasted the {bracket_ms} ms bracket around it"
        );
    }

    #[test]
    fn zone_stats_summarize_recorded_durations_exactly() {
        // Synthetic durations keep the arithmetic independent of the
        // scheduler: two frames of 10 ms and 20 ms average to exactly 15 ms.
        let mut profiler = FrameProfiler::new();
        profiler.set_enabled(true);
        for millis in [10, 20] {
            profiler.begin_frame();
            profiler.record_zone("zone", Duration::from_millis(millis));
            profiler.end_frame();
        }

        let stats = profiler
            .zone_stats("zone")
            .expect("both frames recorded the zone");
        assert!((stats.avg_ms - 15.0).abs() < 1e-3, "{stats:?}");
        assert!((stats.min_ms - 10.0).abs() < 1e-3, "{stats:?}");
        assert!((stats.max_ms - 20.0).abs() < 1e-3, "{stats:?}");
    }

    #[test]
    fn a_zone_entered_twice_in_one_frame_accumulates() {
        let mut profiler = FrameProfiler::new();
        profiler.set_enabled(true);
        profiler.begin_frame();
        profiler.record_zone("zone", Duration::from_millis(3));
        profiler.record_zone("zone", Duration::from_millis(4));
        profiler.end_frame();

        let stats = profiler
            .zone_stats("zone")
            .expect("the frame recorded the zone");
        assert!((stats.avg_ms - 7.0).abs() < 1e-3, "{stats:?}");
    }

    #[test]
    fn test_frame_report() {
        let mut profiler = FrameProfiler::new();
        profiler.set_enabled(true);
        profiler.begin_frame();

        {
            let _z1 = profiler.enter("zone_a");
            sleep(Duration::from_millis(5));
        }
        {
            let _z2 = profiler.enter("zone_b");
            sleep(Duration::from_millis(3));
        }

        let report = profiler.frame_report();
        assert!(report.contains("zone_a"));
        assert!(report.contains("zone_b"));
    }

    #[test]
    fn test_history_limit() {
        let mut profiler = FrameProfiler::new();
        profiler.max_samples = 10;
        profiler.set_enabled(true);

        for _ in 0..20 {
            profiler.begin_frame();
            {
                let _zone = profiler.enter("test");
                sleep(Duration::from_millis(1));
            }
            profiler.end_frame();
        }

        let samples = profiler.history.get("test").unwrap();
        assert_eq!(samples.len(), 10);
    }
}

#[cfg(test)]
mod frame_zone_tests {
    use super::*;
    use std::time::Duration;
    fn record(p: &mut FrameProfiler, ms: u64) {
        p.record_zone("windows", Duration::from_millis(ms));
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
}
