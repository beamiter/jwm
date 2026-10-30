use super::{Action, Hit, Panel};
use anyhow::Result;
use cairo::{Context, LinearGradient};
use xbar_core::model::{BarSnapshot, ShellRoute, TagId, UserAction};
use xbar_core::shell_palette::{Color, ShellPalette};
pub(crate) struct Painter<'a> {
    pub(crate) ctx: &'a Context,
    pub(crate) panel: &'a mut Panel,
    pub(crate) snapshot: &'a BarSnapshot,
    pub(crate) tasks: &'a [crate::ipc::Task],
    pub(crate) dark: bool,
}

impl Painter<'_> {
    fn ink(&self) {
        color(self.ctx, ShellPalette::for_dark(self.dark).ink);
    }

    fn control_surface(
        &self,
        rect: [f64; 4],
        selected: bool,
        hovered: bool,
        pressed: bool,
    ) -> Result<()> {
        let [x, y, w, h] = rect;
        let palette = ShellPalette::for_dark(self.dark);
        rounded(self.ctx, x, y, w, h, 14.0);
        let gradient = LinearGradient::new(x, y, x, y + h);
        let (top, bottom) = if selected {
            (palette.accent, palette.selection)
        } else {
            (palette.control_top, palette.control_bottom)
        };
        gradient_stop(&gradient, 0.0, top);
        gradient_stop(&gradient, 1.0, bottom);
        self.ctx.set_source(&gradient)?;
        self.ctx.fill_preserve()?;
        // A wash changes luminance without moving the painted or input bounds.
        // Press wins over hover in both appearances, including selected cells.
        if pressed || hovered {
            color(
                self.ctx,
                if pressed {
                    palette.pressed
                } else if selected {
                    // Selected cells carry white ink: hover deepens the blue
                    // rather than bleaching the foreground's contrast away.
                    [0.02, 0.08, 0.18, 0.14]
                } else {
                    palette.hover
                },
            );
            self.ctx.fill_preserve()?;
        }
        color(self.ctx, palette.rim);
        self.ctx.set_line_width(0.8);
        self.ctx.stroke()?;
        Ok(())
    }
    fn button(
        &mut self,
        rect: [f64; 4],
        label: &str,
        action: Action,
        selected: bool,
    ) -> Result<()> {
        let [x, y, w, h] = rect;
        let hovered = self.panel.hovered == Some(self.panel.hits.len());
        let pressed = self
            .panel
            .pressed
            .as_ref()
            .is_some_and(|hit| hit.action == action && hit.rect == rect);
        // A layered soft shadow gives each glass control lift without moving
        // either its painted bounds or its input target during hover.
        for (spread, alpha) in [(2.0, 0.025), (1.0, 0.045)] {
            rounded(
                self.ctx,
                x - spread / 2.0,
                y + 1.5,
                w + spread,
                h + spread,
                14.0,
            );
            self.ctx.set_source_rgba(0.12, 0.34, 0.58, alpha);
            self.ctx.fill()?;
        }
        self.control_surface(rect, selected, hovered, pressed)?;
        if selected {
            self.ctx.set_source_rgb(1.0, 1.0, 1.0);
        } else {
            self.ink();
        }
        if !symbol(self.ctx, label, rect)? {
            text(
                self.ctx,
                label,
                x + 6.0,
                y + (h - 20.0) / 2.0,
                w - 12.0,
                13.0,
                true,
            )?;
        }
        self.panel.hits.push(Hit {
            rect,
            action,
            slider: None,
            enabled: true,
            slider_value: None,
        });
        Ok(())
    }

    fn flat_button(&mut self, rect: [f64; 4], label: &str, action: Action) -> Result<()> {
        let [x, y, w, h] = rect;
        let pressed = self
            .panel
            .pressed
            .as_ref()
            .is_some_and(|hit| hit.action == action && hit.rect == rect);
        if pressed || self.panel.hovered == Some(self.panel.hits.len()) {
            let palette = ShellPalette::for_dark(self.dark);
            rounded(self.ctx, x, y, w, h, 12.0);
            color(
                self.ctx,
                if pressed {
                    palette.pressed
                } else {
                    palette.hover
                },
            );
            self.ctx.fill()?;
        }
        self.ink();
        if !symbol(self.ctx, label, rect)? {
            text(self.ctx, label, x, y + (h - 20.0) / 2.0, w, 14.0, true)?;
        }
        self.panel.hits.push(Hit {
            rect,
            action,
            slider: None,
            enabled: true,
            slider_value: None,
        });
        Ok(())
    }

    fn rule(&self, x: f64, y: f64, w: f64) -> Result<()> {
        color(self.ctx, ShellPalette::for_dark(self.dark).divider);
        self.ctx.move_to(x, y);
        self.ctx.line_to(x + w, y);
        self.ctx.set_line_width(1.0);
        self.ctx.stroke()?;
        Ok(())
    }

    pub(crate) fn left(&mut self, x: f64, y: f64, w: f64, h: f64) -> Result<()> {
        let size = (w - 16.0).clamp(1.0, 44.0);
        let bx = x + (w - size) / 2.0;
        let gap = if h < 620.0 { 6.0 } else { 8.0 };
        let mut cy = y + 14.0;
        for (label, action) in [
            (
                "▦",
                Action::Model(UserAction::OpenShellHub(ShellRoute::Applications)),
            ),
            ("▱", Action::Files),
            (">_", Action::Terminal),
            ("◎", Action::Browser),
            (
                "clipboard",
                Action::Model(UserAction::OpenShellHub(ShellRoute::Clipboard)),
            ),
            (
                "♫",
                Action::Model(UserAction::OpenShellHub(ShellRoute::Hub)),
            ),
        ] {
            if cy + size + 76.0 > y + h {
                break;
            }
            self.button([bx, cy, size, size], label, action, false)?;
            cy += size + gap;
        }
        self.rule(x + 18.0, cy + 4.0, (w - 36.0).max(1.0))?;
        cy += 20.0;
        let count = self.snapshot.tags.len();
        let capacity = (((y + h - 70.0 - cy) / (size + gap)).max(0.0) as usize).min(4);
        let active = self.snapshot.active_tag.map(TagId::index).unwrap_or(0);
        let start = active.checked_div(capacity).unwrap_or(0) * capacity;
        for i in (start..count).take(capacity) {
            let Some(tag) = TagId::new(i) else {
                continue;
            };
            self.button(
                [bx, cy, size, size],
                &format!("{}", i + 1),
                Action::Model(UserAction::ViewTagOn {
                    tag,
                    monitor: self.snapshot.monitor,
                }),
                self.snapshot.active_tag == Some(tag),
            )?;
            // Occupancy is a small independent dot, never a changing text run.
            if self.snapshot.tags[i].occupied || self.snapshot.active_tag == Some(tag) {
                if self.snapshot.active_tag == Some(tag) {
                    self.ctx.set_source_rgba(1.0, 1.0, 1.0, 0.95);
                } else {
                    color(self.ctx, ShellPalette::for_dark(self.dark).ink);
                }
                self.ctx.arc(
                    bx + size - 10.0,
                    cy + size / 2.0,
                    2.0,
                    0.0,
                    std::f64::consts::TAU,
                );
                self.ctx.fill()?;
            }
            cy += size + gap;
        }
        if h >= 60.0 {
            let next = (start + capacity.max(1)) % count.max(1);
            let action = if count > capacity {
                Action::Model(UserAction::ViewTagOn {
                    tag: TagId::new(next).expect("bounded workspace index"),
                    monitor: self.snapshot.monitor,
                })
            } else {
                Action::Model(UserAction::OpenShellHub(ShellRoute::Applications))
            };
            self.rule(x + 18.0, y + h - size - 25.0, (w - 36.0).max(1.0))?;
            self.button([bx, y + h - size - 14.0, size, size], "+", action, false)?;
        }
        Ok(())
    }

    pub(crate) fn right(&mut self, x: f64, y: f64, w: f64, h: f64) -> Result<()> {
        let size = (w - 16.0).clamp(1.0, 44.0);
        let bx = x + (w - size) / 2.0;
        let mut cy = y + 10.0;
        self.flat_button(
            [bx, cy, size, 36.0],
            "♧",
            Action::Model(UserAction::OpenShellHub(ShellRoute::Notifications)),
        )?;
        cy += 48.0;
        self.rule(x + 18.0, cy, (w - 36.0).max(1.0))?;
        cy += 16.0;
        for (audio, label, value) in [
            (
                true,
                if self.snapshot.audio.muted {
                    "mute"
                } else {
                    "speaker"
                },
                self.snapshot.audio.volume_percent,
            ),
            (false, "☼", self.snapshot.brightness.percent),
        ] {
            let sh = if h >= 620.0 {
                130.0
            } else if h >= 540.0 {
                110.0
            } else {
                90.0
            };
            if cy + sh + 70.0 > y + h {
                break;
            }
            let palette = ShellPalette::for_dark(self.dark);
            let hovered = self.panel.hovered.is_some_and(|index| {
                index == self.panel.hits.len() || index == self.panel.hits.len() + 1
            });
            let pressed = self
                .panel
                .pressed
                .as_ref()
                .is_some_and(|hit| hit.slider.is_some_and(|(is_audio, ..)| is_audio == audio));
            self.control_surface([bx, cy, size, sh], false, hovered, pressed)?;
            self.ink();
            symbol(self.ctx, label, [bx, cy + 2.0, size, 32.0])?;
            self.panel.hits.push(Hit {
                rect: [bx, cy, size, 34.0],
                action: Action::Model(if audio && value.is_some() {
                    UserAction::ToggleMute
                } else {
                    UserAction::OpenShellHub(ShellRoute::Hub)
                }),
                slider: None,
                enabled: true,
                slider_value: None,
            });
            let top = cy + 40.0;
            let length = sh - 54.0;
            rounded(self.ctx, bx + (size - 12.0) / 2.0, top, 12.0, length, 6.0);
            color(self.ctx, palette.track);
            self.ctx.fill()?;
            if let Some(value) = value {
                let fill = length * value.as_f64() / 100.0;
                if fill > 0.0 {
                    rounded(
                        self.ctx,
                        bx + (size - 12.0) / 2.0,
                        top + length - fill,
                        12.0,
                        fill,
                        6.0,
                    );
                    color(self.ctx, palette.accent);
                    self.ctx.fill()?;
                }
                // A small handle makes the current level legible even at 0%.
                self.ctx.arc(
                    bx + size / 2.0,
                    top + length - fill,
                    3.5,
                    0.0,
                    std::f64::consts::TAU,
                );
                self.ctx.set_source_rgb(1.0, 1.0, 1.0);
                self.ctx.fill_preserve()?;
                color(self.ctx, palette.accent);
                self.ctx.set_line_width(1.2);
                self.ctx.stroke()?;
            } else {
                self.ink();
                text(
                    self.ctx,
                    "—",
                    bx,
                    top + length / 2.0 - 10.0,
                    size,
                    12.0,
                    true,
                )?;
            }
            self.panel.hits.push(Hit {
                rect: [bx, top, size, length],
                action: Action::Model(if audio {
                    UserAction::ToggleMute
                } else {
                    UserAction::OpenShellHub(ShellRoute::Hub)
                }),
                slider: Some((audio, top, length)),
                enabled: value.is_some(),
                slider_value: value,
            });
            cy += sh + 12.0;
        }
        self.rule(x + 18.0, cy + 3.0, (w - 36.0).max(1.0))?;
        cy += 18.0;
        let quick = (size - 4.0).max(1.0);
        let qx = x + (w - quick) / 2.0;
        for (label, route) in [
            ("⌁", ShellRoute::Hub),
            ("bluetooth", ShellRoute::Hub),
            ("◐", ShellRoute::Theme),
            ("▣", ShellRoute::Wallpaper),
            ("▤", ShellRoute::Calendar),
        ] {
            if cy + quick + 68.0 > y + h {
                break;
            }
            self.button(
                [qx, cy, quick, quick],
                label,
                Action::Model(UserAction::OpenShellHub(route)),
                false,
            )?;
            cy += quick + 8.0;
        }
        if h >= 60.0 {
            self.rule(x + 18.0, y + h - size - 25.0, (w - 36.0).max(1.0))?;
            self.flat_button(
                [bx, y + h - size - 14.0, size, size],
                "⚙",
                Action::Model(UserAction::OpenShellHub(ShellRoute::Hub)),
            )?;
        }
        Ok(())
    }

    pub(crate) fn bottom(&mut self, x: f64, y: f64, w: f64, h: f64) -> Result<()> {
        let size = (h - 20.0).clamp(1.0, 42.0);
        let cy = y + (h - size) / 2.0;
        let media_width = if w >= 1000.0 {
            306.0
        } else if w >= 800.0 {
            268.0
        } else if w >= 640.0 {
            218.0
        } else {
            0.0
        };
        if media_width > 0.0 {
            let art = 36.0_f64.min(size);
            let ax = x + 20.0;
            let ay = y + (h - art) / 2.0;
            // No fabricated album image: show an explicit music tile until
            // the shared now-playing model supplies artwork in a future API.
            rounded(self.ctx, ax, ay, art, art, 9.0);
            let gradient = LinearGradient::new(ax, ay, ax + art, ay + art);
            gradient.add_color_stop_rgb(0.0, 0.19, 0.48, 0.73);
            gradient.add_color_stop_rgb(1.0, 0.05, 0.16, 0.32);
            self.ctx.set_source(&gradient)?;
            self.ctx.fill()?;
            self.ctx.set_source_rgba(0.92, 0.97, 1.0, 0.96);
            symbol(self.ctx, "♫", [ax + 4.0, ay + 4.0, art - 8.0, art - 8.0])?;
            let tx = ax + art + 12.0;
            let controls_x = x + media_width - 104.0;
            let tw = (controls_x - tx - 10.0).max(1.0);
            self.ink();
            text(
                self.ctx,
                self.snapshot
                    .media
                    .title
                    .as_deref()
                    .unwrap_or("Nothing playing"),
                tx,
                cy + 2.0,
                tw,
                11.5,
                false,
            )?;
            color(self.ctx, ShellPalette::for_dark(self.dark).muted_ink);
            text(
                self.ctx,
                self.snapshot
                    .media
                    .artist
                    .as_deref()
                    .or(self.snapshot.media.player.as_deref())
                    .unwrap_or("JWM Media"),
                tx,
                cy + 22.0,
                tw,
                10.0,
                false,
            )?;
            self.flat_button(
                [controls_x, cy + 3.0, 30.0, size - 6.0],
                "previous",
                Action::MediaPrevious,
            )?;
            self.flat_button(
                [controls_x + 34.0, cy + 3.0, 30.0, size - 6.0],
                if self.snapshot.media.playback == xbar_core::model::MediaPlayback::Playing {
                    "Ⅱ"
                } else {
                    "▶"
                },
                Action::Model(UserAction::MediaPlayPause),
            )?;
            self.flat_button(
                [controls_x + 68.0, cy + 3.0, 30.0, size - 6.0],
                "next",
                Action::MediaNext,
            )?;
            color(self.ctx, ShellPalette::for_dark(self.dark).divider);
            self.ctx.move_to(x + media_width + 8.0, cy + 7.0);
            self.ctx.line_to(x + media_width + 8.0, cy + size - 7.0);
            self.ctx.stroke()?;
        }
        let trailing = if w > 480.0 {
            142.0
        } else if w > 320.0 {
            100.0
        } else if w > 130.0 {
            50.0
        } else {
            0.0
        };
        let start = x + media_width + if media_width > 0.0 { 24.0 } else { 14.0 };
        let end = x + w - trailing - 10.0;
        let available = (end - start).max(0.0);
        let inline_width = if available >= 64.0 { 46.0 } else { 0.0 };
        let capacity = (((available - inline_width) / 154.0).max(0.0) as usize)
            .max(usize::from(available >= 24.0));
        let stride = if capacity == 1 {
            (available - inline_width).min(154.0)
        } else {
            154.0
        };
        let task_width = (stride - 12.0).max(24.0).min(stride);
        let task_count = self.tasks.len().min(capacity);
        let fallback = self.tasks.is_empty() && available >= 210.0;
        let span = if fallback {
            154.0
        } else {
            task_count as f64 * stride
        } + inline_width;
        let mut cx = start + ((available - span) / 2.0).max(0.0);
        if fallback {
            self.button(
                [cx, cy, 142.0, size],
                if self.snapshot.client_name.is_empty() {
                    "Desktop"
                } else {
                    &self.snapshot.client_name
                },
                Action::Model(UserAction::OpenShellHub(ShellRoute::Applications)),
                false,
            )?;
            cx += 154.0;
        }
        for task in self
            .tasks
            .iter()
            .cycle()
            .skip(self.panel.task_offset % self.tasks.len().max(1))
            .take(task_count)
        {
            self.task_button([cx, cy, task_width, size], task)?;
            cx += stride;
        }
        // A typed overflow action rotates the live inventory. No window is
        // discarded merely because its title does not fit in this viewport.
        if inline_width > 0.0 {
            let overflow = self.tasks.len() > capacity;
            self.button(
                [cx, cy, 40.0, size],
                &if overflow {
                    format!("+{}", self.tasks.len().saturating_sub(capacity))
                } else {
                    "+".into()
                },
                if overflow {
                    Action::CycleTasks
                } else {
                    Action::Model(UserAction::OpenShellHub(ShellRoute::Applications))
                },
                false,
            )?;
        }
        if w > 480.0 {
            self.flat_button(
                [x + w - 132.0, cy, 34.0, size],
                "⌕",
                Action::Model(UserAction::OpenShellHub(ShellRoute::Applications)),
            )?;
            color(self.ctx, ShellPalette::for_dark(self.dark).divider);
            self.ctx.move_to(x + w - 88.0, cy + 7.0);
            self.ctx.line_to(x + w - 88.0, cy + size - 7.0);
            self.ctx.stroke()?;
            self.flat_button(
                [x + w - 78.0, cy, 34.0, size],
                "clipboard",
                Action::Model(UserAction::OpenShellHub(ShellRoute::Clipboard)),
            )?;
            self.flat_button(
                [x + w - 38.0, cy, 30.0, size],
                "▦",
                Action::Model(UserAction::OpenShellHub(ShellRoute::Hub)),
            )?;
        } else if w > 320.0 {
            self.flat_button(
                [x + w - 88.0, cy, 34.0, size],
                "⌕",
                Action::Model(UserAction::OpenShellHub(ShellRoute::Applications)),
            )?;
            self.flat_button(
                [x + w - 42.0, cy, 34.0, size],
                "▦",
                Action::Model(UserAction::OpenShellHub(ShellRoute::Hub)),
            )?;
        } else if w > 130.0 {
            self.flat_button(
                [x + w - 42.0, cy, 34.0, size],
                "▦",
                Action::Model(UserAction::OpenShellHub(ShellRoute::Hub)),
            )?;
        }
        Ok(())
    }

    fn task_button(&mut self, rect: [f64; 4], task: &crate::ipc::Task) -> Result<()> {
        let [x, y, w, h] = rect;
        let action = self
            .snapshot
            .minimized_windows
            .iter()
            .find(|window| window.token.get() == task.id && task.is_minimized)
            .map(|window| {
                Action::Model(UserAction::RestoreWindow {
                    window: window.token,
                    wm_session_id: self.snapshot.wm_session_id,
                    minimized_generation: self.snapshot.wm_sequence.unwrap_or_default(),
                    geometry: xbar_core::DockItemGeometry::new(
                        self.panel.placement.x.saturating_add(x.round() as i32),
                        self.panel.placement.y.saturating_add(y.round() as i32),
                        w.round().max(1.0) as u32,
                        h.round().max(1.0) as u32,
                    ),
                })
            })
            .unwrap_or(Action::FocusWindow {
                id: task.id,
                incarnation: task.incarnation,
            });
        self.button(rect, "", action, task.is_focused)?;
        let compact = w < 96.0;
        let icon_size = 24.0_f64.min((h - 8.0).max(1.0)).min((w - 8.0).max(1.0));
        let ix = x + if compact { (w - icon_size) / 2.0 } else { 10.0 };
        let iy = y + (h - icon_size) / 2.0;
        app_icon(
            self.ctx,
            &task.class,
            &task.name,
            [ix, iy, icon_size, icon_size],
            task.is_focused,
        )?;
        if task.is_focused {
            self.ctx.set_source_rgb(1.0, 1.0, 1.0);
        } else {
            self.ink();
        }
        if !compact {
            text(
                self.ctx,
                &task.name,
                x + icon_size + 20.0,
                y + (h - 18.0) / 2.0,
                w - icon_size - 32.0,
                11.0,
                false,
            )?;
        }
        if task.is_minimized {
            color(self.ctx, ShellPalette::for_dark(self.dark).muted_ink);
            self.ctx
                .arc(x + w - 9.0, y + h - 8.0, 1.8, 0.0, std::f64::consts::TAU);
            self.ctx.fill()?;
        }
        Ok(())
    }
}

fn color(ctx: &Context, [r, g, b, a]: Color) {
    ctx.set_source_rgba(r.into(), g.into(), b.into(), a.into());
}

fn gradient_stop(gradient: &LinearGradient, offset: f64, [r, g, b, a]: Color) {
    gradient.add_color_stop_rgba(offset, r.into(), g.into(), b.into(), a.into());
}

pub(crate) fn rounded(ctx: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w.max(0.0) / 2.0).min(h.max(0.0) / 2.0);
    ctx.new_sub_path();
    ctx.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    ctx.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    ctx.arc(
        x + r,
        y + h - r,
        r,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    ctx.arc(
        x + r,
        y + r,
        r,
        std::f64::consts::PI,
        std::f64::consts::PI * 1.5,
    );
    ctx.close_path();
}

fn text(
    ctx: &Context,
    value: &str,
    x: f64,
    y: f64,
    width: f64,
    size: f64,
    center: bool,
) -> Result<()> {
    if width <= 0.0 {
        return Ok(());
    }
    // Pango keeps translated labels and non-Latin window titles shaped and
    // ellipsized. Text is never allowed to grow the native surface.
    let layout = pangocairo::functions::create_layout(ctx);
    let mut font = pango::FontDescription::from_string("Sans");
    font.set_absolute_size(size * f64::from(pango::SCALE));
    layout.set_font_description(Some(&font));
    layout.set_text(value);
    layout.set_width((width * f64::from(pango::SCALE)).min(f64::from(i32::MAX)) as i32);
    layout.set_ellipsize(pango::EllipsizeMode::End);
    layout.set_single_paragraph_mode(true);
    layout.set_alignment(if center {
        pango::Alignment::Center
    } else {
        pango::Alignment::Left
    });
    ctx.move_to(x, y);
    // Use the shared renderer's Cairo/Pango re-export rather than a second
    // text stack or a dependency on private-use icon glyphs.
    pangocairo::functions::show_layout(ctx, &layout);
    Ok(())
}

/// Shared capsule material: a cool translucent tint with a restrained upper
/// highlight. The host supplies the rounded clip and the real desktop backdrop.
pub(crate) fn material(ctx: &Context, rect: [f64; 4], dark: bool) -> Result<()> {
    let [x, y, _, h] = rect;
    let palette = ShellPalette::for_dark(dark);
    let tint = LinearGradient::new(x, y, x, y + h);
    gradient_stop(&tint, 0.0, palette.sheet_top);
    gradient_stop(&tint, 1.0, palette.sheet_bottom);
    ctx.set_source(&tint)?;
    ctx.paint()?;
    let sheen = LinearGradient::new(x, y, x, y + h.min(120.0));
    sheen.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, if dark { 0.06 } else { 0.22 });
    sheen.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.0);
    ctx.set_source(&sheen)?;
    ctx.paint()?;
    Ok(())
}

fn app_icon(ctx: &Context, class: &str, title: &str, rect: [f64; 4], selected: bool) -> Result<()> {
    let [x, y, w, h] = rect;
    rounded(ctx, x, y, w, h, 7.0);
    ctx.set_source_rgba(1.0, 1.0, 1.0, if selected { 0.88 } else { 0.72 });
    ctx.fill()?;
    ctx.set_source_rgb(0.08, 0.39, 0.80);
    let initial = class
        .chars()
        .chain(title.chars())
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "□".into());
    text(ctx, &initial, x, y + (h - 18.0) / 2.0, w, 13.0, true)?;
    if class.is_empty() {
        return Ok(());
    }
    use xbar_core::presentation::{ImageSource, NodeId, Rect, Scene, SceneNode, Size, VisualState};
    thread_local! {
        static ICONS: std::cell::RefCell<(xbar_core::app_icon::AppIconResolver,xbar_core::render::cairo::CairoRenderer)> = std::cell::RefCell::new((
            xbar_core::app_icon::AppIconResolver::new(24),
            xbar_core::render::cairo::CairoRenderer::new(pango::FontDescription::from_string("Sans 11"))));
    }
    ICONS.with(|cache| -> Result<()> {
        let mut cache = cache.borrow_mut();
        let icon = cache.0.resolve(class).cloned();
        if let Some(icon) = icon {
            let bounds = Rect::new(x as f32, y as f32, w as f32, h as f32);
            let scene = Scene {
                viewport: Size::new((x + w) as f32, (y + h) as f32),
                clip: bounds,
                hits: vec![],
                nodes: vec![SceneNode::Image {
                    id: NodeId::ClientIcon,
                    bounds,
                    source: ImageSource::from(&icon),
                    state: VisualState::default(),
                }],
            };
            ctx.save()?;
            rounded(ctx, x, y, w, h, 7.0);
            ctx.clip();
            let result = cache.1.render(ctx, &scene);
            ctx.restore()?;
            result?;
        }
        Ok(())
    })
}

/// Small vector symbols stay crisp at any scale without private-use fonts.
fn symbol(ctx: &Context, label: &str, rect: [f64; 4]) -> Result<bool> {
    if !matches!(
        label,
        "▦" | "▱"
            | ">_"
            | "◎"
            | "♫"
            | "♧"
            | "☼"
            | "⚙"
            | "▣"
            | "▤"
            | "◐"
            | "⌁"
            | "⌕"
            | "▶"
            | "Ⅱ"
            | "+"
            | "speaker"
            | "mute"
            | "bluetooth"
            | "clipboard"
            | "previous"
            | "next"
    ) {
        return Ok(false);
    }
    let [x, y, w, h] = rect;
    ctx.save()?;
    ctx.translate(x + w / 2.0, y + h / 2.0);
    let scale = (w.min(h) / 44.0).min(1.0);
    ctx.scale(scale, scale);
    ctx.set_line_width(1.8);
    ctx.set_line_cap(cairo::LineCap::Round);
    ctx.set_line_join(cairo::LineJoin::Round);
    match label {
        "▦" => {
            for (x, y) in [(-8.0, -8.0), (2.0, -8.0), (-8.0, 2.0), (2.0, 2.0)] {
                rounded(ctx, x, y, 6.0, 6.0, 1.6);
                ctx.stroke()?;
            }
        }
        "▱" => {
            ctx.set_source_rgb(0.08, 0.49, 1.0);
            rounded(ctx, -11.0, -6.0, 22.0, 16.0, 3.0);
            ctx.fill()?;
            rounded(ctx, -11.0, -9.0, 10.0, 7.0, 2.0);
            ctx.fill()?;
            ctx.set_source_rgba(0.66, 0.86, 1.0, 0.85);
            ctx.move_to(-8.0, -3.0);
            ctx.line_to(8.0, -3.0);
            ctx.stroke()?;
        }
        ">_" => {
            ctx.set_source_rgb(0.05, 0.11, 0.23);
            rounded(ctx, -12.0, -11.0, 24.0, 22.0, 5.0);
            ctx.fill()?;
            ctx.set_source_rgb(1.0, 1.0, 1.0);
            ctx.move_to(-7.0, -5.0);
            ctx.line_to(-2.0, 0.0);
            ctx.line_to(-7.0, 5.0);
            ctx.move_to(1.0, 5.0);
            ctx.line_to(7.0, 5.0);
            ctx.stroke()?;
        }
        "◎" => {
            ctx.set_source_rgb(0.08, 0.48, 1.0);
            ctx.arc(0.0, 0.0, 9.0, 0.0, std::f64::consts::TAU);
            ctx.fill()?;
            ctx.set_source_rgb(1.0, 0.48, 0.12);
            ctx.set_line_width(5.0);
            ctx.arc(0.0, 0.0, 9.0, -2.5, 2.5);
            ctx.stroke()?;
        }
        "♫" => {
            ctx.move_to(-5.0, 6.0);
            ctx.line_to(-5.0, -7.0);
            ctx.line_to(7.0, -10.0);
            ctx.line_to(7.0, 3.0);
            ctx.stroke()?;
            ctx.arc(-8.0, 7.0, 3.5, 0.0, std::f64::consts::TAU);
            ctx.fill()?;
            ctx.arc(4.0, 4.0, 3.5, 0.0, std::f64::consts::TAU);
            ctx.fill()?;
        }
        "♧" => {
            ctx.move_to(-9.0, 6.0);
            ctx.line_to(-7.0, 3.0);
            ctx.line_to(-7.0, -2.0);
            ctx.curve_to(-7.0, -12.0, 7.0, -12.0, 7.0, -2.0);
            ctx.line_to(7.0, 3.0);
            ctx.line_to(9.0, 6.0);
            ctx.close_path();
            ctx.stroke()?;
            ctx.arc(0.0, 9.0, 2.0, 0.0, std::f64::consts::PI);
            ctx.stroke()?;
        }
        "speaker" | "mute" => {
            ctx.move_to(-10.0, -4.0);
            ctx.line_to(-4.0, -4.0);
            ctx.line_to(2.0, -9.0);
            ctx.line_to(2.0, 9.0);
            ctx.line_to(-4.0, 4.0);
            ctx.line_to(-10.0, 4.0);
            ctx.close_path();
            ctx.stroke()?;
            if label == "mute" {
                ctx.move_to(7.0, -4.0);
                ctx.line_to(13.0, 4.0);
                ctx.move_to(13.0, -4.0);
                ctx.line_to(7.0, 4.0);
                ctx.stroke()?;
            } else {
                for r in [7.0, 11.0] {
                    ctx.arc(1.0, 0.0, r, -0.8, 0.8);
                    ctx.stroke()?;
                }
            }
        }
        "⚙" => {
            for n in 0..32 {
                let angle = f64::from(n) * std::f64::consts::TAU / 32.0;
                let radius = if n % 4 == 1 || n % 4 == 2 { 11.0 } else { 8.5 };
                let (x, y) = (radius * angle.cos(), radius * angle.sin());
                if n == 0 {
                    ctx.move_to(x, y);
                } else {
                    ctx.line_to(x, y);
                }
            }
            ctx.close_path();
            ctx.stroke()?;
            ctx.arc(0.0, 0.0, 3.5, 0.0, std::f64::consts::TAU);
            ctx.stroke()?;
        }
        "clipboard" => {
            rounded(ctx, -8.0, -8.0, 16.0, 19.0, 2.5);
            ctx.stroke()?;
            rounded(ctx, -4.0, -11.0, 8.0, 5.0, 1.5);
            ctx.fill()?;
            ctx.move_to(-4.0, -1.0);
            ctx.line_to(4.0, -1.0);
            ctx.move_to(-4.0, 4.0);
            ctx.line_to(2.0, 4.0);
            ctx.stroke()?;
        }
        "bluetooth" => {
            ctx.move_to(-2.0, -12.0);
            ctx.line_to(7.0, -5.0);
            ctx.line_to(-8.0, 7.0);
            ctx.move_to(-2.0, 12.0);
            ctx.line_to(7.0, 5.0);
            ctx.line_to(-8.0, -7.0);
            ctx.move_to(-2.0, -12.0);
            ctx.line_to(-2.0, 12.0);
            ctx.stroke()?;
        }
        "previous" | "next" => {
            if label == "next" {
                ctx.scale(-1.0, 1.0);
            }
            ctx.move_to(5.0, -7.0);
            ctx.line_to(-4.0, 0.0);
            ctx.line_to(5.0, 7.0);
            ctx.close_path();
            ctx.fill()?;
            ctx.move_to(-8.0, -7.0);
            ctx.line_to(-8.0, 7.0);
            ctx.stroke()?;
        }
        "☼" => {
            ctx.arc(0.0, 0.0, 4.0, 0.0, std::f64::consts::TAU);
            ctx.stroke()?;
            for n in 0..8 {
                let angle = f64::from(n) * std::f64::consts::FRAC_PI_4;
                ctx.move_to(angle.cos() * 7.0, angle.sin() * 7.0);
                ctx.line_to(angle.cos() * 10.0, angle.sin() * 10.0);
                ctx.stroke()?;
            }
        }
        "▣" => {
            rounded(ctx, -10.0, -8.0, 20.0, 14.0, 2.0);
            ctx.stroke()?;
            ctx.move_to(0.0, 6.0);
            ctx.line_to(0.0, 10.0);
            ctx.move_to(-5.0, 10.0);
            ctx.line_to(5.0, 10.0);
            ctx.stroke()?;
        }
        "▤" => {
            rounded(ctx, -9.0, -9.0, 18.0, 19.0, 3.0);
            ctx.stroke()?;
            ctx.move_to(-9.0, -3.0);
            ctx.line_to(9.0, -3.0);
            ctx.move_to(-4.0, -12.0);
            ctx.line_to(-4.0, -6.0);
            ctx.move_to(4.0, -12.0);
            ctx.line_to(4.0, -6.0);
            ctx.stroke()?;
            for (x, y) in [(-4.0, 1.0), (3.0, 1.0), (-4.0, 6.0), (3.0, 6.0)] {
                ctx.rectangle(x, y, 2.0, 2.0);
                ctx.fill()?;
            }
        }
        "◐" => {
            ctx.arc(0.0, 0.0, 9.0, 0.0, std::f64::consts::TAU);
            ctx.stroke()?;
            ctx.arc(
                0.0,
                0.0,
                8.0,
                std::f64::consts::FRAC_PI_2,
                std::f64::consts::PI * 1.5,
            );
            ctx.close_path();
            ctx.fill()?;
        }
        "⌁" => {
            for radius in [5.0, 9.0, 13.0] {
                ctx.arc(0.0, 8.0, radius, -2.35, -0.79);
                ctx.stroke()?;
            }
            ctx.arc(0.0, 7.0, 1.8, 0.0, std::f64::consts::TAU);
            ctx.fill()?;
        }
        "⌕" => {
            ctx.arc(-2.0, -2.0, 6.5, 0.0, std::f64::consts::TAU);
            ctx.stroke()?;
            ctx.move_to(3.0, 3.0);
            ctx.line_to(10.0, 10.0);
            ctx.stroke()?;
        }
        "▶" => {
            ctx.move_to(-5.0, -9.0);
            ctx.line_to(9.0, 0.0);
            ctx.line_to(-5.0, 9.0);
            ctx.close_path();
            ctx.fill()?;
        }
        "Ⅱ" => {
            rounded(ctx, -7.0, -8.0, 4.0, 16.0, 1.5);
            ctx.fill()?;
            rounded(ctx, 3.0, -8.0, 4.0, 16.0, 1.5);
            ctx.fill()?;
        }
        _ => {
            ctx.move_to(-8.0, 0.0);
            ctx.line_to(8.0, 0.0);
            ctx.move_to(0.0, -8.0);
            ctx.line_to(0.0, 8.0);
            ctx.stroke()?;
        }
    }
    ctx.restore()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use xbar_core::edge_shell::{EdgeReveal, RevealMode};
    use xbar_core::{BarEdge, BarPlacement};

    #[test]
    fn control_feedback_is_visible_without_moving_the_click_target() {
        let snapshot = xbar_core::model::BarModel::default().snapshot();
        for dark in [false, true] {
            for selected in [false, true] {
                let mut frames = Vec::new();
                for state in 0..3 {
                    let mut surface =
                        cairo::ImageSurface::create(cairo::Format::ARgb32, 64, 64).unwrap();
                    let ctx = Context::new(&surface).unwrap();
                    let mut panel = panel(BarEdge::Left, 0);
                    panel.hovered = (state > 0).then_some(0);
                    let action = Action::Files;
                    let rect = [8.0, 8.0, 44.0, 44.0];
                    if state == 2 {
                        panel.pressed = Some(Hit {
                            rect,
                            action: action.clone(),
                            slider: None,
                            enabled: true,
                            slider_value: None,
                        });
                    }
                    Painter {
                        ctx: &ctx,
                        panel: &mut panel,
                        snapshot: &snapshot,
                        tasks: &[],
                        dark,
                    }
                    .button(rect, "", action.clone(), selected)
                    .unwrap();
                    assert_eq!(panel.hits.len(), 1);
                    assert_eq!(panel.hits[0].rect, rect);
                    assert_eq!(panel.hits[0].action, action);
                    drop(ctx);
                    surface.flush();
                    frames.push(surface.data().unwrap().to_vec());
                }
                assert_ne!(
                    frames[0], frames[1],
                    "missing hover: dark={dark}, selected={selected}"
                );
                assert_ne!(
                    frames[1], frames[2],
                    "missing press: dark={dark}, selected={selected}"
                );
            }
        }
    }

    fn panel(edge: BarEdge, offset: usize) -> Panel {
        Panel {
            task_offset: offset,
            edge,
            window: 0,
            sensor: 0,
            sensor_rect: BarPlacement {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            mapped: true,
            sensor_mapped: false,
            applied_mode: None,
            gc: 0,
            back_buffer: None,
            reveal: EdgeReveal::new(RevealMode::Always),
            placement: BarPlacement {
                x: 0,
                y: 0,
                width: 800,
                height: 88,
            },
            hits: vec![],
            hovered: None,
            pressed: None,
        }
    }

    #[test]
    fn unavailable_sliders_remain_visible_diagnostics_without_input_targets() {
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 90, 656).unwrap();
        let ctx = Context::new(&surface).unwrap();
        for available in [false, true] {
            let mut snapshot = xbar_core::model::BarModel::default().snapshot();
            let value = available.then(|| xbar_core::Percent::from_whole(42).unwrap());
            snapshot.audio.volume_percent = value;
            snapshot.brightness.percent = value;
            let mut panel = panel(BarEdge::Right, 0);
            Painter {
                ctx: &ctx,
                panel: &mut panel,
                snapshot: &snapshot,
                tasks: &[],
                dark: false,
            }
            .right(12.0, 0.0, 66.0, 656.0)
            .unwrap();
            let sliders: Vec<_> = panel
                .hits
                .iter()
                .filter(|hit| hit.slider.is_some())
                .collect();
            assert_eq!(sliders.len(), 2);
            for hit in sliders {
                assert_eq!(hit.enabled, available);
                assert_eq!(hit.slider_value, value);
                let [x, y, w, h] = hit.rect;
                assert_eq!(hit.contains(x + w / 2.0, y + h / 2.0), available);
            }
        }
    }

    #[test]
    fn four_row_workspace_pages_keep_all_tags_reachable() {
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 84, 680).unwrap();
        let ctx = Context::new(&surface).unwrap();
        for count in [1, 4, 9, 32] {
            let mut snapshot = xbar_core::model::BarModel::new(xbar_core::model::ModelConfig {
                tag_count: count,
                ..Default::default()
            })
            .unwrap()
            .snapshot();
            snapshot.active_tag = TagId::new(0);
            let mut seen = std::collections::BTreeSet::new();
            for _ in 0..32 {
                let mut panel = panel(BarEdge::Left, 0);
                Painter {
                    ctx: &ctx,
                    panel: &mut panel,
                    snapshot: &snapshot,
                    tasks: &[],
                    dark: false,
                }
                .left(12.0, 0.0, 60.0, 680.0)
                .unwrap();
                let rows: Vec<_> = panel
                    .hits
                    .iter()
                    .take(panel.hits.len() - 1)
                    .filter_map(|hit| match &hit.action {
                        Action::Model(UserAction::ViewTagOn { tag, .. }) => Some(tag.index()),
                        _ => None,
                    })
                    .collect();
                assert!(rows.len() <= 4);
                seen.extend(rows);
                if let Action::Model(UserAction::ViewTagOn { tag, .. }) =
                    panel.hits.last().unwrap().action
                {
                    snapshot.active_tag = Some(tag);
                } else {
                    break;
                }
            }
            assert_eq!(seen.len(), count);
        }
    }

    #[test]
    fn bottom_layout_has_real_transport_actions_and_fits_short_capsule() {
        let snapshot = xbar_core::model::BarModel::default().snapshot();
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 1024, 64).unwrap();
        let ctx = Context::new(&surface).unwrap();
        let mut panel = panel(BarEdge::Bottom, 0);
        Painter {
            ctx: &ctx,
            panel: &mut panel,
            snapshot: &snapshot,
            tasks: &[],
            dark: false,
        }
        .bottom(0.0, 0.0, 1024.0, 64.0)
        .unwrap();
        assert!(
            panel
                .hits
                .iter()
                .any(|hit| hit.action == Action::MediaPrevious)
        );
        assert!(panel.hits.iter().any(|hit| hit.action == Action::MediaNext));
        assert!(
            panel
                .hits
                .iter()
                .any(|hit| hit.action == Action::Model(UserAction::MediaPlayPause))
        );
        for hit in panel.hits {
            let [x, y, w, h] = hit.rect;
            assert!(x >= 0.0 && y >= 0.0 && x + w <= 1024.0 && y + h <= 64.0);
        }
    }

    #[test]
    fn overflow_keeps_every_task_reachable_without_overlapping_trailing_controls() {
        let snapshot = xbar_core::model::BarModel::default().snapshot();
        let tasks: Vec<_> = (0..12)
            .map(|id| crate::ipc::Task {
                id,
                incarnation: 0,
                name: format!("Task {id}"),
                class: String::new(),
                monitor: 0,
                is_focused: false,
                is_minimized: false,
                is_dock: false,
                is_desktop: false,
                is_swallowed: false,
            })
            .collect();
        for width in [
            80.0, 120.0, 200.0, 280.0, 384.0, 480.0, 640.0, 800.0, 1024.0,
        ] {
            let mut seen = std::collections::BTreeSet::new();
            let surface =
                cairo::ImageSurface::create(cairo::Format::ARgb32, width as i32, 64).unwrap();
            let ctx = Context::new(&surface).unwrap();
            for offset in 0..tasks.len() {
                let mut panel = panel(BarEdge::Bottom, offset);
                Painter {
                    ctx: &ctx,
                    panel: &mut panel,
                    snapshot: &snapshot,
                    tasks: &tasks,
                    dark: false,
                }
                .bottom(0.0, 0.0, width, 64.0)
                .unwrap();
                if width >= 120.0 {
                    assert!(
                        panel
                            .hits
                            .iter()
                            .any(|hit| hit.action == Action::CycleTasks),
                        "{width}"
                    );
                }
                for (i, hit) in panel.hits.iter().enumerate() {
                    let [x, y, w, h] = hit.rect;
                    assert!(
                        x >= 0.0 && y >= 0.0 && x + w <= width && y + h <= 64.0,
                        "{width}: {:?}",
                        hit.rect
                    );
                    if let Action::FocusWindow { id, .. } = hit.action {
                        seen.insert(id);
                    }
                    for other in &panel.hits[i + 1..] {
                        let [ox, oy, ow, oh] = other.rect;
                        assert!(
                            x + w <= ox || ox + ow <= x || y + h <= oy || oy + oh <= y,
                            "{width}: overlapping {:?} and {:?}",
                            hit.rect,
                            other.rect
                        );
                    }
                }
            }
            assert_eq!(
                seen.len(),
                tasks.len(),
                "{width}: compact task list lost reachability"
            );
        }
    }
}
