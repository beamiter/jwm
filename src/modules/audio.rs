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
        let (volume_icon, volume) = if let Some(device) = state.get_master_audio_device() {
            let icon = if device.is_muted || device.volume == 0 {
                icons::VOLUME_MUTED
            } else if device.volume < 30 {
                icons::VOLUME_LOW
            } else if device.volume < 70 {
                icons::VOLUME_MEDIUM
            } else {
                icons::VOLUME_HIGH
            };

            (icon, Some(device.volume))
        } else {
            (icons::VOLUME_MUTED, None)
        };

        let mut label_clicked = false;
        let mut label_hovered = false;
        ui.horizontal(|ui| {
            let label_response = ui.button(volume_icon);
            label_clicked = label_response.clicked();
            label_hovered = label_response.hovered();
            ui.label(volume.map_or_else(|| "--".to_string(), |value| format!("{value}%")));
        });

        if label_hovered {
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

        if label_clicked {
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
