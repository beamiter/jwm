use shared_structures::SharedMessage;
use std::time::Instant;
use xbar_core::audio_manager::{AudioDevice, AudioManager};
use xbar_core::system_monitor::SystemMonitor;

use crate::theme::ui;

/// UI-specific state
#[derive(Debug)]
pub struct UiState {
    /// Current scale factor
    pub scale_factor: f32,
    /// Whether window needs resizing
    pub need_resize: bool,
    /// Time display format toggle
    pub show_seconds: bool,
    /// Last UI update time
    pub last_ui_update: Instant,
    /// Button height for calculations
    pub button_height: f32,
}

impl UiState {
    pub fn new() -> Self {
        Self {
            scale_factor: ui::DEFAULT_SCALE_FACTOR,
            need_resize: false,
            show_seconds: false,
            last_ui_update: Instant::now(),
            button_height: 0.0,
        }
    }

    /// Toggle time format
    pub fn toggle_time_format(&mut self) {
        self.show_seconds = !self.show_seconds;
    }
}

/// Main application state
#[derive(Debug)]
pub struct AppState {
    /// Audio system
    pub audio_manager: AudioManager,
    /// System monitoring
    pub system_monitor: SystemMonitor,
    /// UI state
    pub ui_state: UiState,
    /// Current message from shared memory
    pub current_message: Option<SharedMessage>,
}

impl AppState {
    /// Create new application state
    pub fn new() -> Self {
        Self {
            audio_manager: AudioManager::new(),
            system_monitor: SystemMonitor::new(10),
            ui_state: UiState::new(),
            current_message: None,
        }
    }

    /// Update all subsystems
    pub fn update(&mut self) {
        let now = Instant::now();
        self.system_monitor.update_if_needed();
        self.audio_manager.update_if_needed();
        self.ui_state.last_ui_update = now;
    }

    /// Get master audio device
    pub fn get_master_audio_device(&self) -> Option<&AudioDevice> {
        self.audio_manager.get_master_device()
    }

    /// Get memory info for display
    pub fn get_memory_display_info(&self) -> (f64, f64) {
        if let Some(snapshot) = self.system_monitor.get_snapshot() {
            (
                snapshot.memory_available as f64 / 1e9,
                snapshot.memory_used as f64 / 1e9,
            )
        } else {
            (0.0, 0.0)
        }
    }
}

/// Thread-safe shared application state
#[derive(Debug)]
pub struct SharedAppState {
    pub current_message: Option<SharedMessage>,
    pub last_update: Instant,
}

impl SharedAppState {
    pub fn new() -> Self {
        Self {
            current_message: None,
            last_update: Instant::now(),
        }
    }
}
