use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const FRAME_HISTORY_CAP: usize = 30;
const RENDER_SAMPLES_CAP: usize = 60;

/// Per-output timing state from wp-presentation feedback
pub(crate) struct OutputTiming {
    pub output_id: u32,
    pub last_ust: u64,
    pub last_msc: u64,
    pub refresh_interval_ns: u64,
    pub last_presented: Instant,
    pub frame_history: VecDeque<FrameTimestamp>,
}

pub(crate) struct FrameTimestamp {
    pub msc: u64,
    pub ust: u64,
    pub render_start: Instant,
    pub presented: Instant,
    pub latency: Duration,
}

/// Tracks presentation feedback from all outputs
pub(crate) struct PresentationTimingManager {
    outputs: HashMap<u32, OutputTiming>,
    global_frame_count: u64,
}

/// Adaptive frame scheduler using presentation timing
pub(crate) struct AdaptiveFrameScheduler {
    target_fps: u32,
    min_fps: u32,
    max_fps: u32,
    current_fps: u32,
    last_schedule: Instant,
    frame_budget: Duration,
    consecutive_late_frames: u32,
    consecutive_early_frames: u32,
    avg_render_time: Duration,
    render_time_samples: VecDeque<Duration>,
}

// --- OutputTiming ---

impl OutputTiming {
    pub fn new(output_id: u32, refresh_interval_ns: u64) -> Self {
        Self {
            output_id,
            last_ust: 0,
            last_msc: 0,
            refresh_interval_ns,
            last_presented: Instant::now(),
            frame_history: VecDeque::with_capacity(FRAME_HISTORY_CAP),
        }
    }

    pub fn estimated_vblank_time(&self) -> Instant {
        self.last_presented + Duration::from_nanos(self.refresh_interval_ns)
    }
}

// --- PresentationTimingManager ---

impl PresentationTimingManager {
    pub fn new() -> Self {
        Self {
            outputs: HashMap::new(),
            global_frame_count: 0,
        }
    }

    pub fn register_output(&mut self, output_id: u32, refresh_interval_ns: u64) {
        self.outputs
            .insert(output_id, OutputTiming::new(output_id, refresh_interval_ns));
    }

    pub fn remove_output(&mut self, output_id: u32) {
        self.outputs.remove(&output_id);
    }

    pub fn on_frame_presented(
        &mut self,
        output_id: u32,
        ust: u64,
        msc: u64,
        render_start: Instant,
    ) {
        let now = Instant::now();
        self.global_frame_count += 1;

        if let Some(output) = self.outputs.get_mut(&output_id) {
            output.last_ust = ust;
            output.last_msc = msc;
            output.last_presented = now;

            let latency = now.duration_since(render_start);
            let timestamp = FrameTimestamp {
                msc,
                ust,
                render_start,
                presented: now,
                latency,
            };

            if output.frame_history.len() >= FRAME_HISTORY_CAP {
                output.frame_history.pop_front();
            }
            output.frame_history.push_back(timestamp);
        }
    }

    pub fn time_until_next_vblank(&self, output_id: u32) -> Option<Duration> {
        let output = self.outputs.get(&output_id)?;
        let refresh = Duration::from_nanos(output.refresh_interval_ns);
        let expected_vblank = output.last_presented + refresh;
        let now = Instant::now();

        if now >= expected_vblank {
            Some(Duration::ZERO)
        } else {
            Some(expected_vblank - now)
        }
    }

    pub fn estimate_next_msc(&self, output_id: u32) -> Option<u64> {
        let output = self.outputs.get(&output_id)?;
        let refresh = Duration::from_nanos(output.refresh_interval_ns);
        let expected_vblank = output.last_presented + refresh;
        let now = Instant::now();

        if now >= expected_vblank {
            // We're past the expected vblank, so next is +2
            Some(output.last_msc + 2)
        } else {
            Some(output.last_msc + 1)
        }
    }

    pub fn get_refresh_interval(&self, output_id: u32) -> Option<Duration> {
        let output = self.outputs.get(&output_id)?;
        Some(Duration::from_nanos(output.refresh_interval_ns))
    }

    pub fn avg_frame_latency(&self, output_id: u32) -> Option<Duration> {
        let output = self.outputs.get(&output_id)?;
        if output.frame_history.is_empty() {
            return None;
        }
        let total: Duration = output.frame_history.iter().map(|f| f.latency).sum();
        Some(total / output.frame_history.len() as u32)
    }

    pub fn missed_frames(&self, output_id: u32) -> u64 {
        let Some(output) = self.outputs.get(&output_id) else {
            return 0;
        };
        let refresh = Duration::from_nanos(output.refresh_interval_ns);
        output
            .frame_history
            .iter()
            .filter(|f| f.latency > refresh)
            .count() as u64
    }

    pub fn output_count(&self) -> usize {
        self.outputs.len()
    }

    pub fn global_frame_count(&self) -> u64 {
        self.global_frame_count
    }
}

// --- AdaptiveFrameScheduler ---

impl AdaptiveFrameScheduler {
    pub fn new(target_fps: u32) -> Self {
        let frame_budget = Duration::from_secs(1) / target_fps;
        Self {
            target_fps,
            min_fps: 15,
            max_fps: target_fps,
            current_fps: target_fps,
            last_schedule: Instant::now(),
            frame_budget,
            consecutive_late_frames: 0,
            consecutive_early_frames: 0,
            avg_render_time: Duration::ZERO,
            render_time_samples: VecDeque::with_capacity(RENDER_SAMPLES_CAP),
        }
    }

    pub fn with_range(min_fps: u32, max_fps: u32) -> Self {
        let target = max_fps;
        let frame_budget = Duration::from_secs(1) / target;
        Self {
            target_fps: target,
            min_fps,
            max_fps,
            current_fps: target,
            last_schedule: Instant::now(),
            frame_budget,
            consecutive_late_frames: 0,
            consecutive_early_frames: 0,
            avg_render_time: Duration::ZERO,
            render_time_samples: VecDeque::with_capacity(RENDER_SAMPLES_CAP),
        }
    }

    pub fn schedule_next_frame(&mut self, timing: Option<&OutputTiming>) -> Duration {
        self.last_schedule = Instant::now();

        let delay = if let Some(output) = timing {
            let refresh = Duration::from_nanos(output.refresh_interval_ns);
            let next_vblank = output.last_presented + refresh;
            let now = Instant::now();

            if now >= next_vblank {
                // Already past vblank, schedule immediately
                Duration::from_millis(1)
            } else {
                let time_to_vblank = next_vblank - now;
                // Start rendering early enough to hit the vblank
                if time_to_vblank > self.avg_render_time {
                    time_to_vblank - self.avg_render_time
                } else {
                    Duration::from_millis(1)
                }
            }
        } else {
            // No timing info, use fixed frame budget
            self.frame_budget
        };

        // Clamp to [1ms, 100ms]
        delay.clamp(Duration::from_millis(1), Duration::from_millis(100))
    }

    pub fn on_frame_completed(&mut self, render_time: Duration) {
        // Record sample
        if self.render_time_samples.len() >= RENDER_SAMPLES_CAP {
            self.render_time_samples.pop_front();
        }
        self.render_time_samples.push_back(render_time);

        // Update average render time
        let total: Duration = self.render_time_samples.iter().sum();
        self.avg_render_time = total / self.render_time_samples.len() as u32;

        // Check frame budget utilization
        let budget_90 = self.frame_budget * 9 / 10;
        let budget_50 = self.frame_budget / 2;

        if render_time > budget_90 {
            self.consecutive_late_frames += 1;
            self.consecutive_early_frames = 0;
        } else if render_time < budget_50 {
            self.consecutive_early_frames += 1;
            self.consecutive_late_frames = 0;
        } else {
            // In the normal range, reset both counters
            self.consecutive_late_frames = 0;
            self.consecutive_early_frames = 0;
        }

        // Adapt FPS
        if self.consecutive_late_frames >= 5 {
            self.decrease_fps();
            self.consecutive_late_frames = 0;
        } else if self.consecutive_early_frames >= 30 {
            self.increase_fps();
            self.consecutive_early_frames = 0;
        }
    }

    pub fn on_frame_presented(&mut self, was_late: bool) {
        if was_late {
            self.consecutive_late_frames += 1;
            self.consecutive_early_frames = 0;

            if self.consecutive_late_frames >= 3 {
                self.decrease_fps();
                self.consecutive_late_frames = 0;
            }
        }
    }

    pub fn current_fps(&self) -> u32 {
        self.current_fps
    }

    pub fn frame_budget(&self) -> Duration {
        self.frame_budget
    }

    pub fn set_target_fps(&mut self, fps: u32) {
        self.target_fps = fps;
        self.max_fps = fps;
        self.current_fps = fps;
        self.frame_budget = Duration::from_secs(1) / fps;
        self.consecutive_late_frames = 0;
        self.consecutive_early_frames = 0;
    }

    pub fn avg_render_time(&self) -> Duration {
        self.avg_render_time
    }

    pub fn is_gpu_bound(&self) -> bool {
        self.avg_render_time > self.frame_budget * 8 / 10
    }

    pub fn stats_string(&self) -> String {
        let render_ms = self.avg_render_time.as_secs_f64() * 1000.0;
        format!(
            "FPS: {}/{}, avg render: {:.1}ms, late: {}",
            self.current_fps, self.target_fps, render_ms, self.consecutive_late_frames
        )
    }

    fn decrease_fps(&mut self) {
        if self.current_fps > self.min_fps {
            self.current_fps = (self.current_fps - 5).max(self.min_fps);
            self.frame_budget = Duration::from_secs(1) / self.current_fps;
        }
    }

    fn increase_fps(&mut self) {
        if self.current_fps < self.max_fps {
            self.current_fps = (self.current_fps + 5).min(self.max_fps);
            self.frame_budget = Duration::from_secs(1) / self.current_fps;
        }
    }
}
