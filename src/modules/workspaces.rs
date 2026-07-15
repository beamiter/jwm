use egui::{Button, Color32, Stroke, StrokeKind, Vec2};
use log::info;
use xbar_core::{TagId, UserAction};

use crate::state::AppState;
use crate::theme::{colors, icons, with_alpha};

use super::BarModule;

pub struct WorkspacesModule;

impl WorkspacesModule {
    pub fn new() -> Self {
        Self
    }
}

impl BarModule for WorkspacesModule {
    fn id(&self) -> &str {
        "workspaces"
    }

    fn name(&self) -> &str {
        "Workspaces"
    }

    fn render_bar(&mut self, ui: &mut egui::Ui, state: &mut AppState) {
        let bold_thickness = 2.5;
        let light_thickness = 1.5;
        let wm_available = state.snapshot.wm_available;
        let monitor = state.snapshot.monitor;

        for (index, &tag_icon) in icons::TAG_ICONS.iter().enumerate() {
            let tag_color = colors::TAG_COLORS[index];
            let tag_bit = 1 << index;

            let rich_text = egui::RichText::new(tag_icon).monospace();

            let mut is_urg = false;
            let mut is_filled = false;
            let mut is_selected = false;
            let mut tooltip = format!("Tag {}", index + 1);
            let mut button_bg_color = Color32::TRANSPARENT;

            if let Some(tag_status) = wm_available
                .then(|| state.snapshot.tags.get(index))
                .flatten()
            {
                if tag_status.urgent {
                    tooltip.push_str(" (urgent)");
                    is_urg = true;
                    button_bg_color = with_alpha(colors::RED, 90);
                } else if tag_status.filled {
                    is_filled = true;
                    tooltip.push_str(" (has windows)");
                    button_bg_color = with_alpha(tag_color, 55);
                } else if tag_status.selected {
                    tooltip.push_str(" (current)");
                    is_selected = true;
                    button_bg_color = with_alpha(tag_color, 85);
                } else if tag_status.occupied {
                    button_bg_color = with_alpha(tag_color, 40);
                }
            }

            let button = Button::new(rich_text)
                .min_size(Vec2::new(34.0, 26.0))
                .fill(button_bg_color);

            let label_response = ui.add(button);
            let rect = label_response.rect;
            state.ui_state.button_height = rect.height();

            // Draw border decorations
            if is_urg {
                ui.painter().rect_stroke(
                    rect,
                    1.0,
                    Stroke::new(bold_thickness, colors::VIOLET),
                    StrokeKind::Inside,
                );
            } else if is_filled {
                ui.painter().rect_stroke(
                    rect,
                    1.0,
                    Stroke::new(bold_thickness, tag_color),
                    StrokeKind::Inside,
                );
            } else if is_selected {
                ui.painter().rect_stroke(
                    rect,
                    1.0,
                    Stroke::new(light_thickness, tag_color),
                    StrokeKind::Inside,
                );
            }

            // Handle interactions
            if label_response.clicked() {
                info!("{} clicked", tag_bit);
                if wm_available && let Some(tag) = TagId::new(index) {
                    state.dispatch(UserAction::ViewTagOn { tag, monitor });
                }
            }

            if label_response.secondary_clicked() {
                info!("{} secondary_clicked", tag_bit);
                if wm_available && let Some(tag) = TagId::new(index) {
                    state.dispatch(UserAction::ToggleTagOn { tag, monitor });
                }
            }
        }
    }
}
