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
    pub fn enter(&mut self, zone_name: &'static str) -> ProfileZone {
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
        *self.current_frame.entry(zone_name).or_insert(Duration::ZERO) += duration;
    }

    /// Record a zone duration (called automatically by ProfileZone drop)
    fn record_zone(&mut self, zone_name: &'static str, duration: Duration) {
        if !self.enabled {
            return;
        }
        *self.current_frame.entry(zone_name).or_insert(Duration::ZERO) += duration;
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

    /// Get statistics for a zone
    pub fn zone_stats(&self, zone: &str) -> Option<ZoneStats> {
        let samples = self.history.get(zone)?;
        if samples.is_empty() {
            return None;
        }

        let avg = samples.iter().sum::<f32>() / samples.len() as f32;
        let min = samples.iter().copied().fold(f32::MAX, f32::min);
        let max = samples.iter().copied().fold(0.0, f32::max);

        Some(ZoneStats { avg_ms: avg, min_ms: min, max_ms: max })
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

        {
            let _zone = profiler.enter("test_zone");
            sleep(Duration::from_millis(10));
        }

        profiler.end_frame();

        let stats = profiler.zone_stats("test_zone");
        assert!(stats.is_some());
        let stats = stats.unwrap();
        assert!(stats.avg_ms >= 9.0 && stats.avg_ms <= 20.0);
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
