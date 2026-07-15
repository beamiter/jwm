use crate::state::AppState;
use crate::theme::{colors, icons};

use super::BarModule;

pub struct BatteryModule;

impl BatteryModule {
    pub fn new() -> Self {
        Self
    }
}

impl BarModule for BatteryModule {
    fn id(&self) -> &str {
        "battery"
    }

    fn name(&self) -> &str {
        "Battery"
    }

    fn render_bar(&mut self, ui: &mut egui::Ui, state: &mut AppState) {
        if state.snapshot.battery.present {
            let battery_percent = state
                .snapshot
                .battery
                .percent
                .map(|value| value.as_f32())
                .unwrap_or(0.0);
            let is_charging = state.snapshot.battery.charging;

            let battery_color = match battery_percent {
                p if p > 50.0 => colors::BATTERY_HIGH,
                p if p > 20.0 => colors::BATTERY_MEDIUM,
                _ => colors::BATTERY_LOW,
            };

            let battery_icon = if is_charging {
                icons::BATTERY_CHARGING_ICON
            } else {
                icons::BATTERY_ICON
            };

            ui.label(egui::RichText::new(battery_icon).color(battery_color));
            ui.label(egui::RichText::new(format!("{:.0}%", battery_percent)).color(battery_color));

            if battery_percent < 20.0 && !is_charging {
                ui.label(egui::RichText::new("⚠️").color(colors::WARNING));
            }
        } else {
            ui.label(egui::RichText::new("❓").color(colors::UNAVAILABLE));
        }
    }
}
