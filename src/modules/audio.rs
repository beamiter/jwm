use log::error;

use crate::state::AppState;
use crate::theme::icons;

use super::BarModule;

pub struct AudioModule {}

impl AudioModule {
    pub fn new() -> Self {
        Self {}
    }
}

impl BarModule for AudioModule {
    fn id(&self) -> &str {
        "audio"
    }

    fn name(&self) -> &str {
        "Audio"
    }

    fn render_bar(&mut self, ui: &mut egui::Ui, state: &mut AppState) {
        let (volume_icon, tooltip) = if let Some(device) = state.get_master_audio_device() {
            let icon = if device.is_muted || device.volume == 0 {
                icons::VOLUME_MUTED
            } else if device.volume < 30 {
                icons::VOLUME_LOW
            } else if device.volume < 70 {
                icons::VOLUME_MEDIUM
            } else {
                icons::VOLUME_HIGH
            };

            let tooltip = format!(
                "{}: {}%{}",
                device.description,
                device.volume,
                if device.is_muted { " (muted)" } else { "" }
            );

            (icon, tooltip)
        } else {
            (icons::VOLUME_MUTED, "No audio device".to_string())
        };

        let label_response = ui.button(volume_icon).on_hover_text(tooltip);

        if label_response.hovered() {
            let scroll = ui.input(|i| {
                i.raw
                    .events
                    .iter()
                    .filter_map(|event| match event {
                        egui::Event::MouseWheel { delta, .. } => Some(delta.y),
                        _ => None,
                    })
                    .sum::<f32>()
            });
            if scroll > 0.0 {
                if let Some(device_name) = state.get_master_audio_device().map(|d| d.name.clone()) {
                    if let Err(e) = state.audio_manager.adjust_volume(&device_name, 5) {
                        error!("Failed to adjust volume: {}", e);
                    }
                }
            } else if scroll < 0.0 {
                if let Some(device_name) = state.get_master_audio_device().map(|d| d.name.clone()) {
                    if let Err(e) = state.audio_manager.adjust_volume(&device_name, -5) {
                        error!("Failed to adjust volume: {}", e);
                    }
                }
            }
        }

        if label_response.clicked() {
            if let Some(device) = state.get_master_audio_device() {
                if device.has_switch_control {
                    let device_name = device.name.clone();
                    if let Err(e) = state.audio_manager.toggle_mute(&device_name) {
                        error!("Failed to toggle mute: {}", e);
                    }
                }
            }
        }
    }
}
