//! Entry point into JWM's own shell surface.
//!
//! DMS, Noctalia, Caelestia and end-4 all put one button on the bar that
//! reaches the launcher, notifications, clipboard, calendar and wallpaper
//! picker. JWM implements those pages natively, so this module only names the
//! page to open: it renders no shell content and keeps no shell state.

use egui::{Button, Color32, Vec2};
use xbar_core::{ShellRoute, UserAction};

use crate::state::AppState;
use crate::theme::{colors, with_alpha};

use super::BarModule;

/// Rows in the order they appear in the popup. The hub is deliberately first:
/// it is the page that lists everything else.
const ROUTES: [(ShellRoute, &str, &str, Color32); 6] = [
    (ShellRoute::Hub, "\u{f009}", "Shell Hub", colors::BLUE),
    (
        ShellRoute::Applications,
        "\u{f135}",
        "Applications",
        colors::CYAN,
    ),
    (
        ShellRoute::Notifications,
        "\u{f0f3}",
        "Notifications",
        colors::GOLD,
    ),
    (ShellRoute::Clipboard, "\u{f0ea}", "Clipboard", colors::GREEN),
    (ShellRoute::Calendar, "\u{f073}", "Calendar", colors::VIOLET),
    (
        ShellRoute::Wallpaper,
        "\u{f03e}",
        "Wallpaper",
        colors::MAGENTA,
    ),
];

pub struct ShellModule {
    show_popup: bool,
    button_rect: Option<egui::Rect>,
}

impl ShellModule {
    pub fn new() -> Self {
        Self {
            show_popup: false,
            button_rect: None,
        }
    }

    /// Send the request and close the popup. Dropping it while the window
    /// manager is gone matters: the shell lives there, so there is nothing
    /// local that could stand in for it.
    fn open(&mut self, state: &mut AppState, route: ShellRoute) {
        self.show_popup = false;
        if !state.snapshot.wm_available {
            log::warn!("Ignoring shell request for {}: no window manager", route.title());
            return;
        }
        log::info!("Opening shell route {}", route.title());
        state.dispatch(UserAction::OpenShellHub(route));
    }
}

impl BarModule for ShellModule {
    fn id(&self) -> &str {
        "shell"
    }

    fn name(&self) -> &str {
        "Shell"
    }

    fn render_bar(&mut self, ui: &mut egui::Ui, state: &mut AppState) {
        let available = state.snapshot.wm_available;
        let fill = if self.show_popup {
            with_alpha(colors::BLUE, 90)
        } else {
            Color32::TRANSPARENT
        };
        let tint = if available {
            colors::TEXT
        } else {
            colors::TEXT_SUBTLE
        };

        let response = ui
            .add_enabled(
                available,
                Button::new(egui::RichText::new(ROUTES[0].1).size(16.0).color(tint))
                    .min_size(Vec2::new(30.0, 26.0))
                    .fill(fill),
            )
            .on_hover_text("Shell Hub — click for the pages, right-click to open");

        self.button_rect = Some(response.rect);

        // No scroll-to-cycle binding here, unlike the single-cell Cairo bars:
        // the popup already lists every page, and egui's smoothed scroll delta
        // stays non-zero for several frames, so one wheel notch would dispatch
        // the request repeatedly.
        if response.clicked() {
            self.show_popup = !self.show_popup;
        }
        if response.secondary_clicked() {
            self.open(state, ShellRoute::Hub);
        }
    }

    fn render_popup(&mut self, ctx: &egui::Context, state: &mut AppState) {
        if !self.show_popup {
            return;
        }
        let Some(button_rect) = self.button_rect else {
            return;
        };

        // Hang the list under the trigger rather than over it, the same way
        // the layout selector stays beside its button.
        let popup_pos = egui::pos2(button_rect.left(), button_rect.bottom() + 4.0);
        let popup_id = egui::Id::new("shell_hub_popup");

        egui::Area::new(popup_id)
            .fixed_pos(popup_pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.vertical(|ui| {
                        for (route, icon, label, accent) in ROUTES {
                            let button = Button::new(
                                egui::RichText::new(format!("{icon}  {label}")).size(13.0),
                            )
                            .min_size(Vec2::new(150.0, 24.0))
                            .fill(with_alpha(accent, 40));
                            if ui.add(button).clicked() {
                                self.open(state, route);
                            }
                        }
                    });
                });
            });

        // Any click that lands on neither the trigger nor the list dismisses
        // it, so the popup never outlives the interaction that opened it.
        if ctx.input(|i| i.pointer.any_click()) {
            let popup_rect = ctx.memory(|mem| mem.area_rect(popup_id));
            let pointer = ctx.pointer_latest_pos();
            let on_button = pointer.is_some_and(|point| button_rect.contains(point));
            let on_popup = popup_rect
                .is_some_and(|rect| pointer.is_some_and(|point| rect.contains(point)));
            if !on_button && !on_popup {
                self.show_popup = false;
            }
        }
    }

    fn has_popup(&self) -> bool {
        self.show_popup
    }

    fn update(&mut self, state: &AppState) -> bool {
        // A window manager that went away cannot serve any page, so the list
        // must not stay open offering rows that would all be dropped.
        if !state.snapshot.wm_available && self.show_popup {
            self.show_popup = false;
            return true;
        }
        false
    }
}
