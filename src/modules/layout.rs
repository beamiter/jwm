use egui::{Button, Color32, Vec2};
use log::info;
use shared_structures::SharedRingBuffer;
use std::sync::Arc;

use crate::ipc;
use crate::state::AppState;
use crate::theme::{colors, with_alpha};

use super::BarModule;

pub struct LayoutModule {
    shared_buffer: Option<Arc<SharedRingBuffer>>,
    show_popup: bool,
    button_rect: Option<egui::Rect>,
}

impl LayoutModule {
    pub fn new(shared_buffer: Option<Arc<SharedRingBuffer>>) -> Self {
        Self {
            shared_buffer,
            show_popup: false,
            button_rect: None,
        }
    }

    fn get_layout_symbol(state: &AppState) -> String {
        state
            .current_message
            .as_ref()
            .map(|m| m.monitor_info.get_ltsymbol())
            .unwrap_or_else(|| "?".to_string())
    }

    fn detect_layout_type(symbol: &str) -> &'static str {
        if symbol.contains("[]=") {
            "tiled"
        } else if symbol.contains("><>") {
            "floating"
        } else if symbol.contains("[M]") {
            "monocle"
        } else {
            "unknown"
        }
    }
}

impl BarModule for LayoutModule {
    fn id(&self) -> &str {
        "layout"
    }

    fn name(&self) -> &str {
        "Layout"
    }

    fn render_bar(&mut self, ui: &mut egui::Ui, state: &mut AppState) {
        let layout_symbol = Self::get_layout_symbol(state);
        let layout_type = Self::detect_layout_type(&layout_symbol);

        // Display layout symbol button
        let button_text = egui::RichText::new(&layout_symbol).monospace().size(12.0);

        let button_bg_color = match layout_type {
            "tiled" => with_alpha(colors::BLUE, 70),
            "floating" => with_alpha(colors::CYAN, 70),
            "monocle" => with_alpha(colors::VIOLET, 70),
            _ => Color32::TRANSPARENT,
        };

        let button = Button::new(button_text)
            .min_size(Vec2::new(50.0, 26.0))
            .fill(button_bg_color);

        let response = ui.add(button);
        let rect = response.rect;
        state.ui_state.button_height = rect.height();

        // Store button rect for popup positioning
        self.button_rect = Some(rect);

        // Toggle popup on click
        if response.clicked() {
            self.show_popup = !self.show_popup;
        }
    }

    fn render_popup(&mut self, ctx: &egui::Context, state: &mut AppState) {
        if !self.show_popup {
            return;
        }

        let monitor_num = state
            .current_message
            .as_ref()
            .map(|m| m.monitor_info.monitor_num as i32)
            .unwrap_or(0);

        let button_rect = match self.button_rect {
            Some(rect) => rect,
            None => return,
        };

        let popup_pos = egui::pos2(button_rect.left(), button_rect.bottom() + 5.0);
        let popup_id = egui::Id::new("layout_popup");

        egui::Area::new(popup_id)
            .fixed_pos(popup_pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        // Tiled layout button
                        if ui.button("Tiled []=").clicked() {
                            info!("Setting layout to tiled");
                            ipc::send_layout_command(&self.shared_buffer, monitor_num, 0);
                            self.show_popup = false;
                        }

                        // Floating layout button
                        if ui.button("Floating ><>").clicked() {
                            info!("Setting layout to floating");
                            ipc::send_layout_command(&self.shared_buffer, monitor_num, 1);
                            self.show_popup = false;
                        }

                        // Monocle layout button
                        if ui.button("Monocle [M]").clicked() {
                            info!("Setting layout to monocle");
                            ipc::send_layout_command(&self.shared_buffer, monitor_num, 2);
                            self.show_popup = false;
                        }
                    });
                });
            });

        // Close popup when clicking outside
        if ctx.input(|i| i.pointer.any_click()) {
            let popup_rect = ctx.memory(|mem| mem.area_rect(popup_id));
            let pointer_pos = ctx.pointer_latest_pos();

            let is_on_button = pointer_pos.map_or(false, |p| button_rect.contains(p));
            let is_on_popup =
                popup_rect.map_or(false, |r| pointer_pos.map_or(false, |p| r.contains(p)));

            if !is_on_button && !is_on_popup {
                self.show_popup = false;
            }
        }
    }

    fn has_popup(&self) -> bool {
        self.show_popup
    }
}
