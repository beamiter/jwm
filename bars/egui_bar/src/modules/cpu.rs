use crate::state::AppState;
use crate::theme::{colors, icons};

use super::BarModule;

pub struct CpuModule;

impl CpuModule {
    pub fn new() -> Self {
        Self
    }

    fn get_cpu_color(usage: f64) -> egui::Color32 {
        let usage = usage.clamp(0.0, 1.0);
        if usage < 0.3 {
            colors::CPU_LOW
        } else if usage < 0.6 {
            colors::CPU_MEDIUM
        } else if usage < 0.8 {
            colors::CPU_HIGH
        } else {
            colors::CPU_CRITICAL
        }
    }
}

impl BarModule for CpuModule {
    fn id(&self) -> &str {
        "cpu"
    }

    fn name(&self) -> &str {
        "CPU"
    }

    fn render_bar(&mut self, ui: &mut egui::Ui, state: &mut AppState) {
        let cpu_average = state.snapshot.system_details.cpu_average;
        if state.snapshot.system.cpu_percent.is_some() {
            let cpu_color = Self::get_cpu_color(cpu_average as f64 / 100.0);
            ui.label(
                egui::RichText::new(format!("{} {}%", icons::CPU_ICON, cpu_average as i32))
                    .color(cpu_color),
            );

            if cpu_average > 0.8 * 100.0 {
                ui.label(egui::RichText::new("🔥").color(colors::WARNING));
            }
        }
    }
}
