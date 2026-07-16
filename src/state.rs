use std::time::{Duration, Instant};

use log::warn;
use xbar_core::{
    AudioDeviceInfo, BarEffect, BarRuntime, BarSnapshot, ModelConfig, RuntimeSchedule,
    RuntimeUpdate, TransportRecoveryConfig, UserAction,
};

/// UI-specific state that is intentionally outside the semantic core model.
#[derive(Debug)]
pub struct UiState {
    pub need_resize: bool,
    pub last_ui_update: Instant,
    pub button_height: f32,
}

impl UiState {
    pub fn new() -> Self {
        Self {
            need_resize: false,
            last_ui_update: Instant::now(),
            button_height: 0.0,
        }
    }
}

/// UI projection plus the single owner of every xbar_core provider and WM adapter.
pub struct AppState {
    runtime: BarRuntime,
    pub snapshot: BarSnapshot,
    pub ui_state: UiState,
    schedule: RuntimeSchedule,
    pending_platform_effects: Vec<BarEffect>,
}

impl AppState {
    pub fn new(shared_path: String) -> Self {
        let mut runtime = if shared_path.is_empty() {
            BarRuntime::new(ModelConfig::default())
        } else {
            let recovery = TransportRecoveryConfig::new(shared_path, Duration::from_secs(2))
                .expect("static transport recovery config is valid");
            BarRuntime::with_managed_transport(ModelConfig::default(), recovery)
        }
        .expect("default xbar_core model config is valid");
        let mut schedule = RuntimeSchedule::default();
        let initial_update = schedule.service(&mut runtime);
        let snapshot = runtime.snapshot();

        let mut state = Self {
            runtime,
            snapshot,
            ui_state: UiState::new(),
            schedule,
            pending_platform_effects: Vec::new(),
        };
        state.apply_runtime_update(initial_update);
        state
    }

    /// Poll the non-blocking WM transport every frame and providers once a second.
    pub fn update(&mut self) {
        let update = self.schedule.service(&mut self.runtime);
        self.apply_runtime_update(update);
        self.ui_state.last_ui_update = Instant::now();
    }

    pub fn dispatch(&mut self, action: UserAction) {
        let update = self.runtime.dispatch(action);
        self.apply_runtime_update(update);
    }

    pub fn take_platform_effects(&mut self) -> Vec<BarEffect> {
        std::mem::take(&mut self.pending_platform_effects)
    }

    pub fn get_master_audio_device(&self) -> Option<&AudioDeviceInfo> {
        self.snapshot.audio_device.as_ref()
    }

    /// Raw byte counters come from SystemDetails and are never reconstructed from a ratio.
    pub fn get_memory_display_info(&self) -> (f64, f64) {
        (
            self.snapshot.system_details.memory_available as f64 / 1e9,
            self.snapshot.system_details.memory_used as f64 / 1e9,
        )
    }

    fn apply_runtime_update(&mut self, update: RuntimeUpdate) {
        for issue in &update.issues {
            warn!("xbar runtime issue: {issue:?}");
        }
        if update.needs_redraw() {
            self.snapshot = self.runtime.snapshot();
        }
        self.pending_platform_effects
            .extend(update.platform_effects);
    }
}
