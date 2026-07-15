use egui::{Button, Color32, Vec2};
use log::info;
use xbar_core::{LayoutId, UserAction};

use crate::state::AppState;
use crate::theme::{colors, with_alpha};

use super::BarModule;

pub struct LayoutModule {
    show_popup: bool,
    button_rect: Option<egui::Rect>,
}

impl LayoutModule {
    pub fn new() -> Self {
        Self {
            show_popup: false,
            button_rect: None,
        }
    }

    fn get_layout_symbol(state: &AppState) -> String {
        if state.snapshot.wm_available {
            state.snapshot.layout_symbol.clone()
        } else {
            "?".to_string()
        }
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

    fn layout_button(
        ui: &mut egui::Ui,
        icon: &str,
        active: bool,
        accent: Color32,
    ) -> egui::Response {
        let fill = if active {
            with_alpha(accent, 90)
        } else {
            Color32::TRANSPARENT
        };

        ui.add(
            Button::new(egui::RichText::new(icon).size(18.0))
                .min_size(Vec2::new(30.0, 26.0))
                .fill(fill),
        )
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
            state.dispatch(UserAction::ToggleLayoutSelector);
            self.show_popup = state.snapshot.layout_selector_open;
        }
    }

    fn render_popup(&mut self, ctx: &egui::Context, state: &mut AppState) {
        if !self.show_popup {
            return;
        }

        let layout_symbol = Self::get_layout_symbol(state);
        let layout_type = Self::detect_layout_type(&layout_symbol);
        let monitor = state.snapshot.monitor;
        let wm_available = state.snapshot.wm_available;

        let button_rect = match self.button_rect {
            Some(rect) => rect,
            None => return,
        };

        // Keep the layout choices beside the trigger instead of covering it.
        let popup_pos = egui::pos2(button_rect.right() + 5.0, button_rect.top());
        let popup_id = egui::Id::new("layout_popup");

        egui::Area::new(popup_id)
            .fixed_pos(popup_pos)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if Self::layout_button(ui, "▦", layout_type == "tiled", colors::BLUE).clicked()
                    {
                        info!("Setting layout to tiled");
                        if wm_available {
                            state.dispatch(UserAction::SetLayoutOn {
                                layout: LayoutId(0),
                                monitor,
                            });
                        }
                        self.show_popup = false;
                    }

                    if Self::layout_button(ui, "▱", layout_type == "floating", colors::CYAN)
                        .clicked()
                    {
                        info!("Setting layout to floating");
                        if wm_available {
                            state.dispatch(UserAction::SetLayoutOn {
                                layout: LayoutId(1),
                                monitor,
                            });
                        }
                        self.show_popup = false;
                    }

                    if Self::layout_button(ui, "▣", layout_type == "monocle", colors::VIOLET)
                        .clicked()
                    {
                        info!("Setting layout to monocle");
                        if wm_available {
                            state.dispatch(UserAction::SetLayoutOn {
                                layout: LayoutId(2),
                                monitor,
                            });
                        }
                        self.show_popup = false;
                    }
                });
            });

        // Close popup when clicking outside
        if ctx.input(|i| i.pointer.any_click()) {
            let popup_rect = ctx.memory(|mem| mem.area_rect(popup_id));
            let pointer_pos = ctx.pointer_latest_pos();

            let is_on_button = pointer_pos.is_some_and(|p| button_rect.contains(p));
            let is_on_popup = popup_rect
                .is_some_and(|rect| pointer_pos.is_some_and(|point| rect.contains(point)));

            if !is_on_button && !is_on_popup {
                if state.snapshot.layout_selector_open {
                    state.dispatch(UserAction::ToggleLayoutSelector);
                }
                self.show_popup = false;
            }
        }
    }

    fn has_popup(&self) -> bool {
        self.show_popup
    }

    fn update(&mut self, state: &AppState) -> bool {
        let show_popup = state.snapshot.layout_selector_open;
        let changed = self.show_popup != show_popup;
        self.show_popup = show_popup;
        changed
    }
}
