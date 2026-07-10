use crate::state::AppState;
use crate::theme::{colors, icons};

use super::BarModule;

pub struct ClockModule;

impl ClockModule {
    pub fn new() -> Self {
        Self
    }
}

impl BarModule for ClockModule {
    fn id(&self) -> &str {
        "clock"
    }

    fn name(&self) -> &str {
        "Clock"
    }

    fn render_bar(&mut self, ui: &mut egui::Ui, state: &mut AppState) {
        let format_str = if state.ui_state.show_seconds {
            "%Y-%m-%d %H:%M:%S"
        } else {
            "%Y-%m-%d %H:%M"
        };

        let current_time = chrono::Local::now().format(format_str).to_string();

        if ui
            .selectable_label(
                true,
                egui::RichText::new(format!("{} {}", icons::TIME_ICON, current_time))
                    .color(colors::GREEN)
                    .small(),
            )
            .clicked()
        {
            state.ui_state.toggle_time_format();
        }
    }
}
