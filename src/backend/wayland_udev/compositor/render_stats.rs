use std::collections::{HashMap, VecDeque};
use std::time::Instant;

pub(crate) struct PassStats {
    pub call_count: u32,
    pub total_time_ms: f32,
    pub avg_time_ms: f32,
    pub min_time_ms: f32,
    pub max_time_ms: f32,
}

pub(crate) struct GLCallStats {
    pub draw_calls: u64,
    pub state_changes: u64,
    pub texture_binds: u64,
    pub program_changes: u64,
    pub frame_count: u64,
}

pub(crate) struct RenderStats {
    passes: HashMap<&'static str, VecDeque<f32>>,
    current_frame: HashMap<&'static str, Instant>,
    max_samples: usize,
    frame_count: u64,
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
            max_samples: 120,
            frame_count: 0,
            draw_calls: 0,
            state_changes: 0,
            texture_binds: 0,
            program_changes: 0,
        }
    }

    pub fn begin_frame(&mut self) {
        self.current_frame.clear();
        self.frame_count += 1;
    }

    pub fn begin_pass(&mut self, name: &'static str) {
        self.current_frame.insert(name, Instant::now());
    }

    pub fn end_pass(&mut self, name: &'static str) {
        if let Some(start) = self.current_frame.remove(name) {
            let elapsed_ms = start.elapsed().as_secs_f32() * 1000.0;
            let samples = self.passes.entry(name).or_default();
            samples.push_back(elapsed_ms);
            if samples.len() > self.max_samples {
                samples.pop_front();
            }
        }
    }

    pub fn record_draw_call(&mut self) {
        self.draw_calls += 1;
    }

    pub fn record_state_change(&mut self) {
        self.state_changes += 1;
    }

    pub fn record_texture_bind(&mut self) {
        self.texture_binds += 1;
    }

    pub fn record_program_change(&mut self) {
        self.program_changes += 1;
    }

    pub fn pass_stats(&self, name: &'static str) -> Option<PassStats> {
        self.passes.get(name).and_then(|samples| {
            if samples.is_empty() {
                return None;
            }
            let call_count = samples.len() as u32;
            let total_time_ms: f32 = samples.iter().sum();
            let avg_time_ms = total_time_ms / call_count as f32;
            let min_time_ms = samples.iter().copied().fold(f32::INFINITY, f32::min);
            let max_time_ms = samples.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            Some(PassStats {
                call_count,
                total_time_ms,
                avg_time_ms,
                min_time_ms,
                max_time_ms,
            })
        })
    }

    pub fn gl_stats(&self) -> GLCallStats {
        GLCallStats {
            draw_calls: self.draw_calls,
            state_changes: self.state_changes,
            texture_binds: self.texture_binds,
            program_changes: self.program_changes,
            frame_count: self.frame_count,
        }
    }

    pub fn reset(&mut self) {
        self.passes.clear();
        self.current_frame.clear();
        self.frame_count = 0;
        self.draw_calls = 0;
        self.state_changes = 0;
        self.texture_binds = 0;
        self.program_changes = 0;
    }

    pub fn report(&self) -> String {
        let mut out = String::from("=== Render Stats Report ===\n");
        out.push_str(&format!("Frames: {}\n", self.frame_count));
        out.push_str(&format!(
            "GL Calls — draw: {}, state_changes: {}, tex_binds: {}, program_changes: {}\n",
            self.draw_calls, self.state_changes, self.texture_binds, self.program_changes
        ));

        if self.frame_count > 0 {
            out.push_str(&format!(
                "Per-frame avg — draw: {:.1}, state: {:.1}, tex: {:.1}, prog: {:.1}\n",
                self.draw_calls as f64 / self.frame_count as f64,
                self.state_changes as f64 / self.frame_count as f64,
                self.texture_binds as f64 / self.frame_count as f64,
                self.program_changes as f64 / self.frame_count as f64,
            ));
        }

        out.push_str("\n--- Pass Timings ---\n");
        let mut pass_names: Vec<&&'static str> = self.passes.keys().collect();
        pass_names.sort();
        for &name in &pass_names {
            if let Some(stats) = self.pass_stats(name) {
                out.push_str(&format!(
                    "  {:<20} calls: {:>4}  avg: {:>7.3}ms  min: {:>7.3}ms  max: {:>7.3}ms  total: {:>8.2}ms\n",
                    name,
                    stats.call_count,
                    stats.avg_time_ms,
                    stats.min_time_ms,
                    stats.max_time_ms,
                    stats.total_time_ms,
                ));
            }
        }

        out
    }
}
