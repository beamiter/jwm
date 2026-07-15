use xbar_core::UserAction;

use crate::state::AppState;
use crate::theme::colors;

use super::BarModule;

pub struct BrightnessModule;

impl BrightnessModule {
    pub fn new() -> Self {
        Self
    }

    fn brightness_icon(percent: u8) -> &'static str {
        match percent {
            0..=25 => "🔅",
            26..=75 => "🔆",
            _ => "☀️",
        }
    }
}

impl BarModule for BrightnessModule {
    fn id(&self) -> &str {
        "brightness"
    }

    fn name(&self) -> &str {
        "Brightness"
    }

    fn render_bar(&mut self, ui: &mut egui::Ui, state: &mut AppState) {
        let Some(brightness) = state.snapshot.brightness.percent else {
            return;
        };
        let percent = brightness.rounded();
        let icon = Self::brightness_icon(percent);
        let response = ui.add(egui::Button::new(
            egui::RichText::new(format!("{icon} {percent}%")).color(colors::WHEAT),
        ));

        if response.hovered() {
            let scroll = ui.input(|input| input.smooth_scroll_delta.y);
            if scroll > 0.5 {
                state.dispatch(UserAction::BrightnessUp);
            } else if scroll < -0.5 {
                state.dispatch(UserAction::BrightnessDown);
            }
        }
    }
}
