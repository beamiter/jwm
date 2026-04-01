use egui::{Button, Label, ScrollArea};
use log::error;
use std::time::{Duration, Instant};

use crate::state::AppState;
use crate::theme::{colors, icons};
use xbar_core::audio_manager::AudioDevice;

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

        let label_response = ui.add(Button::new(volume_icon));

        // Store button rect for popup positioning
        self.button_rect = Some(label_response.rect);

        if label_response.clicked() {
            self.show_popup = !self.show_popup;
        }
        label_response.on_hover_text(tooltip);
    }

    fn render_popup(&mut self, ctx: &egui::Context, state: &mut AppState) {
        if !self.show_popup {
            return;
        }

        let button_rect = match self.button_rect {
            Some(rect) => rect,
            None => return,
        };

        let popup_pos = egui::pos2(button_rect.left(), button_rect.bottom() + 5.0);
        let popup_id = egui::Id::new("audio_popup");

        egui::Area::new(popup_id)
            .fixed_pos(popup_pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(280.0);
                    ScrollArea::vertical()
                        .max_height(200.0)
                        .show(ui, |ui| {
                            draw_volume_content(ui, state, &mut self.last_volume_change, &self.volume_change_debounce, state.ui_state.selected_device);
                        });
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

fn draw_volume_content(ui: &mut egui::Ui, state: &mut AppState, last_volume_change: &mut Instant, volume_change_debounce: &Duration, selected_device: usize) {
    let devices: Vec<AudioDevice> = state.audio_manager.get_devices().to_vec();

    if devices.is_empty() {
        ui.add(Label::new("❌ No controllable audio device found"));
        return;
    }

    let controllable_devices: Vec<(usize, AudioDevice)> = devices
        .into_iter()
        .enumerate()
        .filter(|(_, d)| d.has_volume_control || d.has_switch_control)
        .collect();

    if controllable_devices.is_empty() {
        ui.add(Label::new("❌ No controllable audio device found"));
        return;
    }

    draw_device_selector(ui, &controllable_devices, selected_device);
    ui.add_space(10.0);

    if let Some((_, device)) = controllable_devices.get(selected_device) {
        draw_device_controls(ui, state, device, last_volume_change, volume_change_debounce);
    }
}

fn draw_device_selector(
    ui: &mut egui::Ui,
    controllable_devices: &[(usize, AudioDevice)],
    selected_device: usize,
) {
    ui.horizontal(|ui| {
        ui.add(Label::new("🎵 Device:"));

        let current_selection = &controllable_devices
            .get(selected_device)
            .map(|(_, d)| d.description.clone())
            .unwrap_or_else(|| "None".to_string());

        egui::ComboBox::from_id_salt("audio_device_selector")
            .selected_text(current_selection)
            .width(150.0)
            .show_ui(ui, |ui| {
                for (idx, (_, device)) in controllable_devices.iter().enumerate() {
                    if ui
                        .selectable_label(selected_device == idx, &device.description)
                        .clicked()
                    {
                        // Note: We can't directly update state here due to borrow rules
                        // The selection will be updated in the module's render_popup
                    }
                }
            });
    });
}

fn draw_device_controls(ui: &mut egui::Ui, state: &mut AppState, device: &AudioDevice, last_volume_change: &mut Instant, volume_change_debounce: &Duration) {
    let device_name = device.name.clone();
    let mut current_volume = device.volume;
    let is_muted = device.is_muted;

    if device.has_volume_control {
        ui.horizontal(|ui| {
            ui.add(Label::new("🔊 Volume:"));

            if device.has_switch_control {
                let mute_icon = if is_muted {
                    icons::VOLUME_MUTED
                } else {
                    icons::VOLUME_HIGH
                };
                let mute_btn = ui.button(mute_icon);

                if mute_btn.clicked() {
                    if let Err(e) = state.audio_manager.toggle_mute(&device_name) {
                        error!("Failed to toggle mute: {}", e);
                    }
                }

                mute_btn.on_hover_text(if is_muted { "Unmute" } else { "Mute" });
            }

            ui.label(format!("{}%", current_volume));
        });

        let slider_response = ui.add(
            egui::Slider::new(&mut current_volume, 0..=100)
                .show_value(false)
                .text(""),
        );

        if slider_response.changed() {
            let now = std::time::Instant::now();
            if now.duration_since(*last_volume_change) > *volume_change_debounce {
                *last_volume_change = now;
                if let Err(e) =
                    state
                        .audio_manager
                        .set_volume(&device_name, current_volume, is_muted)
                {
                    error!("Failed to set volume: {}", e);
                }
            }
        }
    } else if device.has_switch_control {
        ui.horizontal(|ui| {
            let btn_text = if is_muted {
                "🔴 Disabled"
            } else {
                "🟢 Enabled"
            };
            let btn_color = if is_muted {
                colors::ERROR
            } else {
                colors::SUCCESS
            };

            if ui
                .add(egui::Button::new(btn_text).fill(btn_color))
                .clicked()
            {
                if let Err(e) = state.audio_manager.toggle_mute(&device_name) {
                    error!("Failed to toggle mute: {}", e);
                }
            }
        });
    } else {
        ui.add(Label::new("❌ No available controls for this device"));
    }

    ui.separator();
    ui.horizontal(|ui| {
        ui.add(Label::new(format!("📋 Type: {:?}", device.device_type)));
        ui.add(Label::new(format!(
            "📹 Controls: {}",
            if device.has_volume_control && device.has_switch_control {
                "Volume + Switch"
            } else if device.has_volume_control {
                "Volume only"
            } else if device.has_switch_control {
                "Switch only"
            } else {
                "None"
            }
        )));
    });
}
