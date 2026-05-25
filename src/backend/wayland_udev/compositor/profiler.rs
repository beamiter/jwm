use std::collections::HashMap;
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
    zones: HashMap<&'static str, Vec<f32>>,
    active_zone: Option<(&'static str, Instant)>,
    last_frame_ms: f32,
}

impl FrameProfiler {
    pub fn new() -> Self {
        Self {
            enabled: false,
            frame_start: None,
            zones: HashMap::new(),
            active_zone: None,
            last_frame_ms: 0.0,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn begin_frame(&mut self) {
        if !self.enabled {
            return;
        }
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
            let samples = self.zones.entry(name).or_insert_with(|| Vec::with_capacity(MAX_SAMPLES));
            if samples.len() >= MAX_SAMPLES {
                samples.remove(0);
            }
            samples.push(duration_ms);
        }
    }

    pub fn zone_stats(&self, name: &'static str) -> Option<ZoneStats> {
        let samples = self.zones.get(name)?;
        if samples.is_empty() {
            return None;
        }
        let sample_count = samples.len() as u32;
        let sum: f32 = samples.iter().sum();
        let avg_ms = sum / sample_count as f32;
        let min_ms = samples.iter().cloned().fold(f32::INFINITY, f32::min);
        let max_ms = samples.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
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
            let min_ms = samples.iter().cloned().fold(f32::INFINITY, f32::min);
            let max_ms = samples.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
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
