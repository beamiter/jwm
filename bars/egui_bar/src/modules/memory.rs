use crate::state::AppState;
use crate::theme::{colors, icons};

use super::BarModule;

pub struct MemoryModule;

impl MemoryModule {
    pub fn new() -> Self {
        Self
    }
}

impl BarModule for MemoryModule {
    fn id(&self) -> &str {
        "memory"
    }

    fn name(&self) -> &str {
        "Memory"
    }

    fn render_bar(&mut self, ui: &mut egui::Ui, state: &mut AppState) {
        let (available_gb, used_gb) = state.get_memory_display_info();

        ui.label(
            egui::RichText::new(format!("{} {:.1}G", icons::MEMORY_ICON, available_gb))
                .color(colors::MEMORY_AVAILABLE),
        );

        ui.label(egui::RichText::new(format!("{:.1}G", used_gb)).color(colors::MEMORY_USED));

        if state.snapshot.system_details.memory_usage_percent > 0.8 * 100.0 {
            ui.label("⚠️");
        }
        ui.separator();
    }
}
