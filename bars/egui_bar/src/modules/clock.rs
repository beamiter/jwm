use crate::state::AppState;
use crate::theme::{colors, icons};
use xbar_core::UserAction;

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
        if ui
            .selectable_label(
                true,
                egui::RichText::new(format!("{} {}", icons::TIME_ICON, state.snapshot.time))
                    .color(colors::GREEN)
                    .small(),
            )
            .clicked()
        {
            state.dispatch(UserAction::ToggleSeconds);
        }
    }
}
