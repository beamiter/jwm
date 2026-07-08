use egui::Button;
use log::error;
use std::time::{Duration, Instant};

use crate::state::AppState;
use crate::theme::icons;

use super::BarModule;

pub struct AudioModule {
    show_popup: bool,
    button_rect: Option<egui::Rect>,
    last_volume_change: Instant,
    volume_change_debounce: Duration,
}

impl AudioModule {
    pub fn new() -> Self {
        Self {
            show_popup: false,
            button_rect: None,
            last_volume_change: Instant::now(),
            volume_change_debounce: Duration::from_millis(50),
        }
    }
}

impl BarModule for AudioModule {
    fn id(&self) -> &str {
        "audio"
    }

    fn name(&self) -> &str {
        "Audio"
    }

    fn has_popup(&self) -> bool {
        true
    }

    fn render_bar(&mut self, ui: &mut egui::Ui, state: &mut AppState) {
        let (volume_icon, _tooltip) = if let Some(device) = state.get_master_audio_device() {
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

        let label_response = ui.add(Button::new(volume_icon));

        // Store button rect for popup positioning
        self.button_rect = Some(label_response.rect);

        if label_response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll > 1.0 {
                if let Some(device_name) = state.get_master_audio_device().map(|d| d.name.clone()) {
                    if let Err(e) = state.audio_manager.adjust_volume(&device_name, 5) {
                        error!("Failed to adjust volume: {}", e);
                    }
                }
            } else if scroll < -1.0 {
                if let Some(device_name) = state.get_master_audio_device().map(|d| d.name.clone()) {
                    if let Err(e) = state.audio_manager.adjust_volume(&device_name, -5) {
                        error!("Failed to adjust volume: {}", e);
                    }
                }
            }
        }

        if label_response.clicked() {
            self.show_popup = !self.show_popup;
        }
    }

    fn render_popup(&mut self, ctx: &egui::Context, state: &mut AppState) {
        if !self.show_popup {
            return;
        }

        let button_rect = match self.button_rect {
            Some(rect) => rect,
            None => return,
        };

        // Get device info first
        let device_info = state.get_master_audio_device().map(|d| (d.name.clone(), d.volume, d.is_muted, d.has_switch_control));

        // Position popup to the right of button
        let popup_pos = egui::pos2(button_rect.right() + 10.0, button_rect.top());
        let popup_id = egui::Id::new("audio_popup");

        egui::Area::new(popup_id)
            .fixed_pos(popup_pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    if let Some((device_name, volume, is_muted, has_switch)) = device_info {
                        draw_simple_volume_control(ui, state, &device_name, volume, is_muted, has_switch, &mut self.last_volume_change, &self.volume_change_debounce);
                    } else {
                        ui.label("No audio device");
                    }
                });
            });

        // Close popup when clicking outside
        if ctx.input(|i| i.pointer.any_click()) {
            let popup_rect = ctx.memory(|mem| mem.area_rect(popup_id));
            let pointer_pos = ctx.pointer_latest_pos();

            let is_on_button = pointer_pos.map_or(false, |p| button_rect.contains(p));
            let is_on_popup = popup_rect.map_or(false, |r| pointer_pos.map_or(false, |p| r.contains(p)));

            if !is_on_button && !is_on_popup {
                self.show_popup = false;
            }
        }
    }
}

fn draw_simple_volume_control(ui: &mut egui::Ui, state: &mut AppState, device_name: &str, mut current_volume: i32, is_muted: bool, has_switch_control: bool, last_volume_change: &mut Instant, volume_change_debounce: &Duration) {
    ui.horizontal(|ui| {
        // Volume slider
        let slider_response = ui.add(
            egui::Slider::new(&mut current_volume, 0..=100)
                .show_value(false)
                .text("")
                .fixed_decimals(0),
        );

        if slider_response.changed() {
            let now = std::time::Instant::now();
            if now.duration_since(*last_volume_change) > *volume_change_debounce {
                *last_volume_change = now;
                if let Err(e) = state.audio_manager.set_volume(device_name, current_volume, is_muted) {
                    error!("Failed to set volume: {}", e);
                }
            }
        }

        // Mute button
        if has_switch_control {
            let mute_icon = if is_muted {
                icons::VOLUME_MUTED
            } else {
                icons::VOLUME_HIGH
            };
            let mute_btn = ui.button(mute_icon);

            if mute_btn.clicked() {
                if let Err(e) = state.audio_manager.toggle_mute(device_name) {
                    error!("Failed to toggle mute: {}", e);
                }
            }
        }

        // Volume percentage
        ui.label(format!("{}%", current_volume));
    });
}
