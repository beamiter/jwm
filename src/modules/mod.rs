pub mod audio;
pub mod battery;
pub mod bluetooth;
pub mod brightness;
pub mod clock;
pub mod cpu;
pub mod layout;
pub mod media;
pub mod memory;
pub mod network;
pub mod workspaces;

use crate::state::AppState;

/// Trait for all bar modules
#[allow(dead_code)]
pub trait BarModule: Send {
    fn id(&self) -> &str;
    fn name(&self) -> &str;

    /// Update module state. Returns true if a repaint is needed.
    fn update(&mut self, state: &AppState) -> bool {
        let _ = state;
        false
    }

    /// Render the module's bar content
    fn render_bar(&mut self, ui: &mut egui::Ui, state: &mut AppState);

    /// Render popup/window if this module has one
    fn render_popup(&mut self, _ctx: &egui::Context, _state: &mut AppState) {}

    /// Whether this module has a popup
    fn has_popup(&self) -> bool {
        false
    }

    fn on_click(&mut self, _state: &mut AppState) {}
    fn on_secondary_click(&mut self, _state: &mut AppState) {}
    fn on_scroll(&mut self, _state: &mut AppState, _delta: f32) {}

    fn min_width(&self) -> Option<f32> {
        None
    }
}

/// Module registry that creates and manages bar modules
pub struct ModuleRegistry {
    pub left: Vec<Box<dyn BarModule>>,
    pub center: Vec<Box<dyn BarModule>>,
    pub right: Vec<Box<dyn BarModule>>,
}

impl ModuleRegistry {
    /// Create module registry with default modules
    pub fn new() -> Self {
        // Default module layout
        let left_names = ["workspaces", "layout"];
        let center_names: [&str; 0] = [];
        let right_names = ["cpu", "memory", "battery", "audio", "clock"];

        let left: Vec<Box<dyn BarModule>> = left_names
            .iter()
            .filter_map(|name| create_module(name))
            .collect();

        let center: Vec<Box<dyn BarModule>> = center_names
            .iter()
            .filter_map(|name| create_module(name))
            .collect();

        let right: Vec<Box<dyn BarModule>> = right_names
            .iter()
            .filter_map(|name| create_module(name))
            .collect();

        Self {
            left,
            center,
            right,
        }
    }
}

/// Factory function: create a module by name
fn create_module(name: &str) -> Option<Box<dyn BarModule>> {
    match name {
        "workspaces" => Some(Box::new(workspaces::WorkspacesModule::new())),
        "layout" => Some(Box::new(layout::LayoutModule::new())),
        "clock" => Some(Box::new(clock::ClockModule::new())),
        "cpu" => Some(Box::new(cpu::CpuModule::new())),
        "memory" => Some(Box::new(memory::MemoryModule::new())),
        "battery" => Some(Box::new(battery::BatteryModule::new())),
        "audio" => Some(Box::new(audio::AudioModule::new())),
        "network" => Some(Box::new(network::NetworkModule::new())),
        "bluetooth" => Some(Box::new(bluetooth::BluetoothModule::new())),
        "brightness" => Some(Box::new(brightness::BrightnessModule::new())),
        "media" => Some(Box::new(media::MediaModule::new())),
        other => {
            log::warn!("Unknown module: {}", other);
            None
        }
    }
}
