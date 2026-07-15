use std::time::{Duration, Instant};

use log::{info, warn};
use xbar_core::{
    AudioDeviceInfo, BarEffect, BarRuntime, BarSnapshot, ModelConfig, RuntimeAdapter, RuntimeIssue,
    RuntimeUpdate, SharedTransport, UserAction,
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
    shared_path: String,
    last_tick: Instant,
    last_transport_attempt: Instant,
    pending_platform_effects: Vec<BarEffect>,
}

impl AppState {
    pub fn new(shared_path: String) -> Self {
        let transport = open_transport(&shared_path, true);
        let mut runtime = BarRuntime::with_transport(ModelConfig::default(), transport)
            .expect("default xbar_core model config is valid");
        let mut initial_update = runtime.tick();
        initial_update.merge(runtime.poll_transport());
        let snapshot = runtime.snapshot();

        let mut state = Self {
            runtime,
            snapshot,
            ui_state: UiState::new(),
            shared_path,
            last_tick: Instant::now(),
            last_transport_attempt: Instant::now(),
            pending_platform_effects: Vec::new(),
        };
        state.apply_runtime_update(initial_update);
        state
    }

    /// Poll the non-blocking WM transport every frame and providers once a second.
    pub fn update(&mut self) {
        let mut update = self.runtime.poll_transport();
        if self.last_tick.elapsed() >= Duration::from_secs(1) {
            self.ensure_transport();
            update.merge(self.runtime.tick());
            self.last_tick = Instant::now();
        }
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

    fn ensure_transport(&mut self) {
        if self.shared_path.is_empty()
            || self.runtime.transport().is_some()
            || self.last_transport_attempt.elapsed() < Duration::from_secs(2)
        {
            return;
        }
        self.last_transport_attempt = Instant::now();
        if let Some(transport) = open_transport(&self.shared_path, false) {
            self.runtime.set_transport(Some(transport));
            info!("Connected to WM transport at {}", self.shared_path);
        }
    }

    fn apply_runtime_update(&mut self, update: RuntimeUpdate) {
        let transport_failed = update.issues.iter().any(|issue| {
            matches!(
                issue,
                RuntimeIssue::AdapterFailed {
                    adapter: RuntimeAdapter::Transport,
                    ..
                }
            )
        });
        for issue in &update.issues {
            warn!("xbar runtime issue: {issue:?}");
        }
        if transport_failed {
            self.runtime.set_transport(None);
            self.last_transport_attempt = Instant::now();
        }
        if update.needs_redraw() {
            self.snapshot = self.runtime.snapshot();
        }
        self.pending_platform_effects
            .extend(update.platform_effects);
    }
}

fn open_transport(shared_path: &str, warn_on_error: bool) -> Option<SharedTransport> {
    if shared_path.is_empty() {
        return None;
    }
    match SharedTransport::open(shared_path) {
        Ok(transport) => Some(transport),
        Err(err) => {
            if warn_on_error {
                warn!("Failed to open WM transport at {shared_path}: {err}");
            }
            None
        }
    }
}
