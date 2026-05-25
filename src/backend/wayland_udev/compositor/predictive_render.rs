use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::Instant;

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum SceneActivity {
    Static,
    Idle,
    Animating,
    HighActivity,
}

pub struct WindowActivity {
    pub damage_count: u32,
    pub last_damage: Instant,
    pub is_focused: bool,
    pub activity_level: SceneActivity,
    pub render_budget_ms: f32,
}

impl WindowActivity {
    fn new() -> Self {
        Self {
            damage_count: 0,
            last_damage: Instant::now(),
            is_focused: false,
            activity_level: SceneActivity::Static,
            render_budget_ms: 33.3,
        }
    }
}

pub struct AnimationPredictor {
    positions: VecDeque<(f32, f32, Instant)>,
}

impl AnimationPredictor {
    pub fn new() -> Self {
        Self {
            positions: VecDeque::with_capacity(10),
        }
    }

    pub fn record_position(&mut self, x: f32, y: f32) {
        if self.positions.len() == 10 {
            self.positions.pop_front();
        }
        self.positions.push_back((x, y, Instant::now()));
    }

    pub fn predicted_position(&self, dt: f32) -> Option<(f32, f32)> {
        if self.positions.len() < 2 {
            return None;
        }

        let last = self.positions.back().unwrap();
        let elapsed_since_last = last.2.elapsed().as_secs_f32();
        if elapsed_since_last > 0.1 {
            return None;
        }

        let prev = &self.positions[self.positions.len() - 2];
        let time_delta = last.2.duration_since(prev.2).as_secs_f32();
        if time_delta <= 0.0 {
            return None;
        }

        let vx = (last.0 - prev.0) / time_delta;
        let vy = (last.1 - prev.1) / time_delta;

        Some((last.0 + vx * dt, last.1 + vy * dt))
    }

    pub fn clear(&mut self) {
        self.positions.clear();
    }
}

pub struct PredictiveRenderManager {
    windows: HashMap<u64, WindowActivity>,
    scene_activity: SceneActivity,
    last_update: Instant,
    power_saving: bool,
    focused_window: Option<u64>,
}

impl PredictiveRenderManager {
    pub fn new() -> Self {
        Self {
            windows: HashMap::new(),
            scene_activity: SceneActivity::Static,
            last_update: Instant::now(),
            power_saving: false,
            focused_window: None,
        }
    }

    pub fn record_window_damage(&mut self, window_id: u64) {
        if let Some(activity) = self.windows.get_mut(&window_id) {
            activity.damage_count += 1;
            activity.last_damage = Instant::now();
        }
    }

    pub fn set_focused_window(&mut self, win: Option<u64>) {
        if let Some(prev) = self.focused_window {
            if let Some(activity) = self.windows.get_mut(&prev) {
                activity.is_focused = false;
                activity.render_budget_ms = 33.3;
            }
        }
        self.focused_window = win;
        if let Some(id) = win {
            if let Some(activity) = self.windows.get_mut(&id) {
                activity.is_focused = true;
                activity.render_budget_ms = 16.6;
            }
        }
    }

    pub fn remove_window(&mut self, window_id: u64) {
        self.windows.remove(&window_id);
        if self.focused_window == Some(window_id) {
            self.focused_window = None;
        }
    }

    pub fn update_scene_activity(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_update).as_secs_f32();
        if elapsed <= 0.0 {
            return;
        }

        let mut total_damage: u32 = 0;
        let mut any_recent = false;

        for activity in self.windows.values() {
            total_damage = total_damage.saturating_add(activity.damage_count);
            if activity.last_damage.elapsed().as_secs_f32() < 2.0 {
                any_recent = true;
            }
        }

        let damage_per_second = total_damage as f32 / elapsed;

        self.scene_activity = if !any_recent {
            SceneActivity::Static
        } else if damage_per_second < 2.0 {
            SceneActivity::Idle
        } else if damage_per_second < 10.0 {
            SceneActivity::Animating
        } else {
            SceneActivity::HighActivity
        };

        // Reset counters
        for activity in self.windows.values_mut() {
            activity.damage_count = 0;
            activity.activity_level = self.scene_activity;
        }

        self.last_update = now;
    }

    pub fn should_render_frame(&self) -> bool {
        match self.scene_activity {
            SceneActivity::Static => self.last_update.elapsed().as_millis() >= 1000,
            SceneActivity::Idle => self.last_update.elapsed().as_millis() >= 100,
            SceneActivity::Animating => true,
            SceneActivity::HighActivity => true,
        }
    }

    pub fn recommended_fps(&self) -> u32 {
        match self.scene_activity {
            SceneActivity::Static => 10,
            SceneActivity::Idle => 30,
            SceneActivity::Animating => 60,
            SceneActivity::HighActivity => {
                if self.power_saving {
                    60
                } else {
                    120
                }
            }
        }
    }

    pub fn scene_activity(&self) -> SceneActivity {
        self.scene_activity
    }

    pub fn set_power_saving(&mut self, enabled: bool) {
        self.power_saving = enabled;
    }

    pub fn get_window_budget(&self, window_id: u64) -> f32 {
        if let Some(activity) = self.windows.get(&window_id) {
            if activity.is_focused {
                16.6
            } else {
                33.3
            }
        } else {
            33.3
        }
    }

    pub fn register_window(&mut self, window_id: u64) {
        let mut activity = WindowActivity::new();
        if self.focused_window == Some(window_id) {
            activity.is_focused = true;
            activity.render_budget_ms = 16.6;
        }
        self.windows.insert(window_id, activity);
    }
}
