/// Rendering statistics and diagnostics
///
/// Tracks detailed statistics about the rendering pipeline to identify
/// bottlenecks and optimization opportunities
use std::collections::HashMap;
use std::time::Instant;

/// Statistics for a rendering pass
#[derive(Debug, Clone, Copy, Default)]
pub struct PassStats {
    pub call_count: u64,
    pub total_time_ms: f64,
    pub avg_time_ms: f32,
    pub min_time_ms: f32,
    pub max_time_ms: f32,
}

/// Rendering statistics collector
pub struct RenderStats {
    /// Per-pass statistics
    passes: HashMap<&'static str, Vec<f32>>,
    /// Current frame pass timings
    current_frame: HashMap<&'static str, (Instant, u32)>, // (start, count)
    /// History size
    max_samples: usize,
    /// Total frames tracked
    frame_count: u64,
    /// GL call counts
    draw_calls: u64,
    state_changes: u64,
    texture_binds: u64,
    program_changes: u64,
}

impl RenderStats {
    pub fn new() -> Self {
        Self {
            passes: HashMap::new(),
            current_frame: HashMap::new(),
            max_samples: 120, // 2 seconds at 60fps
            frame_count: 0,
            draw_calls: 0,
            state_changes: 0,
            texture_binds: 0,
            program_changes: 0,
        }
    }

    /// Begin a new frame
    pub fn begin_frame(&mut self) {
        self.current_frame.clear();
        self.frame_count += 1;
    }

    /// Start timing a render pass
    pub fn begin_pass(&mut self, pass_name: &'static str) {
        let count = self
            .current_frame
            .get(pass_name)
            .map(|(_, c)| *c)
            .unwrap_or(0);
        self.current_frame
            .insert(pass_name, (Instant::now(), count + 1));
    }

    /// End timing a render pass
    pub fn end_pass(&mut self, pass_name: &'static str) {
        if let Some((start, _count)) = self.current_frame.remove(pass_name) {
            let elapsed_ms = start.elapsed().as_secs_f32() * 1000.0;

            let samples = self.passes.entry(pass_name).or_insert_with(Vec::new);
            samples.push(elapsed_ms);
            if samples.len() > self.max_samples {
                samples.remove(0);
            }
        }
    }

    /// Record a draw call
    pub fn record_draw_call(&mut self) {
        self.draw_calls += 1;
    }

    /// Record a state change
    pub fn record_state_change(&mut self) {
        self.state_changes += 1;
    }

    /// Record a texture bind
    pub fn record_texture_bind(&mut self) {
        self.texture_binds += 1;
    }

    /// Record a program change
    pub fn record_program_change(&mut self) {
        self.program_changes += 1;
    }

    /// Get statistics for a pass
    pub fn pass_stats(&self, pass_name: &str) -> Option<PassStats> {
        let samples = self.passes.get(pass_name)?;
        if samples.is_empty() {
            return None;
        }

        let sum: f32 = samples.iter().sum();
        let avg = sum / samples.len() as f32;
        let min = samples.iter().copied().fold(f32::MAX, f32::min);
        let max = samples.iter().copied().fold(0.0, f32::max);

        Some(PassStats {
            call_count: samples.len() as u64,
            total_time_ms: sum as f64,
            avg_time_ms: avg,
            min_time_ms: min,
            max_time_ms: max,
        })
    }

    /// Get all pass statistics
    pub fn all_pass_stats(&self) -> HashMap<&'static str, PassStats> {
        self.passes
            .keys()
            .filter_map(|&name| self.pass_stats(name).map(|stats| (name, stats)))
            .collect()
    }

    /// Get GL call statistics
    pub fn gl_stats(&self) -> GLCallStats {
        GLCallStats {
            draw_calls: self.draw_calls,
            state_changes: self.state_changes,
            texture_binds: self.texture_binds,
            program_changes: self.program_changes,
            frame_count: self.frame_count,
        }
    }

    /// Reset all statistics
    pub fn reset(&mut self) {
        self.passes.clear();
        self.current_frame.clear();
        self.frame_count = 0;
        self.draw_calls = 0;
        self.state_changes = 0;
        self.texture_binds = 0;
        self.program_changes = 0;
    }

    /// Get formatted report
    pub fn report(&self) -> String {
        let mut lines = vec!["Render Statistics:".to_string()];

        // Pass timings
        let mut pass_names: Vec<_> = self.passes.keys().collect();
        pass_names.sort();

        for &&pass_name in &pass_names {
            if let Some(stats) = self.pass_stats(pass_name) {
                lines.push(format!(
                    "  {}: avg={:.2}ms min={:.2}ms max={:.2}ms ({}x)",
                    pass_name,
                    stats.avg_time_ms,
                    stats.min_time_ms,
                    stats.max_time_ms,
                    stats.call_count
                ));
            }
        }

        // GL call stats
        if self.frame_count > 0 {
            lines.push(format!(
                "\nGL Calls (total over {} frames):",
                self.frame_count
            ));
            lines.push(format!(
                "  Draw calls: {} ({:.1}/frame)",
                self.draw_calls,
                self.draw_calls as f32 / self.frame_count as f32
            ));
            lines.push(format!(
                "  State changes: {} ({:.1}/frame)",
                self.state_changes,
                self.state_changes as f32 / self.frame_count as f32
            ));
            lines.push(format!(
                "  Texture binds: {} ({:.1}/frame)",
                self.texture_binds,
                self.texture_binds as f32 / self.frame_count as f32
            ));
            lines.push(format!(
                "  Program changes: {} ({:.1}/frame)",
                self.program_changes,
                self.program_changes as f32 / self.frame_count as f32
            ));
        }

        lines.join("\n")
    }
}

impl Default for RenderStats {
    fn default() -> Self {
        Self::new()
    }
}

/// GL call statistics
#[derive(Debug, Clone, Copy)]
pub struct GLCallStats {
    pub draw_calls: u64,
    pub state_changes: u64,
    pub texture_binds: u64,
    pub program_changes: u64,
    pub frame_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn test_stats_creation() {
        let stats = RenderStats::new();
        assert_eq!(stats.frame_count, 0);
    }

    #[test]
    fn test_pass_timing() {
        let mut stats = RenderStats::new();
        stats.begin_frame();

        stats.begin_pass("test_pass");
        sleep(Duration::from_millis(10));
        stats.end_pass("test_pass");

        let pass_stats = stats.pass_stats("test_pass");
        assert!(pass_stats.is_some());
        let ps = pass_stats.unwrap();
        assert!(ps.avg_time_ms >= 9.0 && ps.avg_time_ms <= 20.0);
    }

    #[test]
    fn test_gl_call_counting() {
        let mut stats = RenderStats::new();

        stats.record_draw_call();
        stats.record_draw_call();
        stats.record_state_change();

        let gl_stats = stats.gl_stats();
        assert_eq!(gl_stats.draw_calls, 2);
        assert_eq!(gl_stats.state_changes, 1);
    }

    #[test]
    fn test_multiple_passes() {
        let mut stats = RenderStats::new();
        stats.begin_frame();

        stats.begin_pass("pass_a");
        sleep(Duration::from_millis(5));
        stats.end_pass("pass_a");

        stats.begin_pass("pass_b");
        sleep(Duration::from_millis(3));
        stats.end_pass("pass_b");

        assert!(stats.pass_stats("pass_a").is_some());
        assert!(stats.pass_stats("pass_b").is_some());
    }

    #[test]
    fn test_report_generation() {
        let mut stats = RenderStats::new();
        stats.begin_frame();

        stats.begin_pass("test");
        sleep(Duration::from_millis(5));
        stats.end_pass("test");

        stats.record_draw_call();
        stats.record_state_change();

        let report = stats.report();
        assert!(report.contains("test"));
        assert!(report.contains("Draw calls"));
    }
}
