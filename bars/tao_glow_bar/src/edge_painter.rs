use super::{Action, Hit, Panel};
use anyhow::Result;
use cairo::Context;
use xbar_core::model::{BarSnapshot, DockItemGeometry, ShellRoute, TagId, UserAction};
pub(super) struct Painter<'a> {
    pub(super) ctx: &'a Context,
    pub(super) panel: &'a mut Panel,
    pub(super) snapshot: &'a BarSnapshot,
    pub(super) tasks: &'a [crate::shell_ipc::Task],
    pub(super) dark: bool,
}

impl Painter<'_> {
    fn ink(&self) {
        if self.dark {
            self.ctx.set_source_rgb(0.89, 0.95, 1.0);
        } else {
            self.ctx.set_source_rgb(0.06, 0.16, 0.29);
        }
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
        rounded(self.ctx, x, y, w, h, 14.0);
        if selected {
            self.ctx
                .set_source_rgb(10.0 / 255.0, 103.0 / 255.0, 227.0 / 255.0);
        } else {
            self.ctx.set_source_rgba(
                1.0,
                1.0,
                1.0,
                if hovered {
                    0.57
                } else if self.dark {
                    0.10
                } else {
                    0.30
                },
            );
        }
        self.ctx.fill()?;
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
                14.0,
                true,
            )?;
        }
        self.panel.hits.push(Hit {
            rect,
            action,
            slider: None,
        });
        Ok(())
    }

    fn rule(&self, x: f64, y: f64, w: f64) -> Result<()> {
        self.ctx.set_source_rgba(0.14, 0.38, 0.64, 0.22);
        self.ctx.move_to(x, y);
        self.ctx.line_to(x + w, y);
        self.ctx.set_line_width(1.0);
        self.ctx.stroke()?;
        Ok(())
    }

    pub(super) fn left(&mut self, x: f64, y: f64, w: f64, h: f64) -> Result<()> {
        let size = (w - 16.0).clamp(1.0, 44.0);
        let bx = x + (w - size) / 2.0;
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
                "♫",
                Action::Model(UserAction::OpenShellHub(ShellRoute::Hub)),
            ),
        ] {
            if cy + size + 64.0 > y + h {
                break;
            }
            self.button([bx, cy, size, size], label, action, false)?;
            cy += size + 8.0;
        }
        self.rule(x + 16.0, cy + 3.0, (w - 32.0).max(1.0))?;
        cy += 16.0;
        let count = self.snapshot.tags.len();
        let capacity = ((y + h - 64.0 - cy) / (size + 8.0)).max(0.0) as usize;
        // Keep the actual active workspace reachable even on small outputs.
        let active = self.snapshot.active_tag.map(TagId::index).unwrap_or(0);
        let start = if active >= capacity {
            active.saturating_sub(capacity.saturating_sub(1))
        } else {
            0
        };
        for i in (start..count).take(capacity) {
            let Some(tag) = TagId::new(i) else {
                continue;
            };
            let label = format!(
                "{}{}",
                i + 1,
                if self.snapshot.tags[i].occupied {
                    " •"
                } else {
                    ""
                }
            );
            self.button(
                [bx, cy, size, size],
                &label,
                Action::Model(UserAction::ViewTagOn {
                    tag,
                    monitor: self.snapshot.monitor,
                }),
                self.snapshot.active_tag == Some(tag),
            )?;
            cy += size + 8.0;
        }
        if h >= 60.0 {
            self.button(
                [bx, y + h - size - 14.0, size, size],
                "+",
                Action::Model(UserAction::OpenShellHub(ShellRoute::Applications)),
                false,
            )?;
        }
        Ok(())
    }

    pub(super) fn right(&mut self, x: f64, y: f64, w: f64, h: f64) -> Result<()> {
        let size = (w - 16.0).clamp(1.0, 44.0);
        let bx = x + (w - size) / 2.0;
        let mut cy = y + 14.0;
        self.button(
            [bx, cy, size, size],
            "♧",
            Action::Model(UserAction::OpenShellHub(ShellRoute::Notifications)),
            false,
        )?;
        cy += size + 18.0;
        for (audio, label, value) in [
            (true, "♪", self.snapshot.audio.volume_percent),
            (false, "☼", self.snapshot.brightness.percent),
        ] {
            let sh = if h < 520.0 { 90.0 } else { 142.0 };
            if cy + sh + 80.0 > y + h {
                break;
            }
            rounded(self.ctx, bx, cy, size, sh, 18.0);
            self.ctx
                .set_source_rgba(1.0, 1.0, 1.0, if self.dark { 0.10 } else { 0.30 });
            self.ctx.fill()?;
            self.ink();
            if !symbol(
                self.ctx,
                if audio { "♫" } else { label },
                [bx, cy + 2.0, size, 32.0],
            )? {
                text(self.ctx, label, bx, cy + 8.0, size, 18.0, true)?;
            }
            if value.is_some() {
                self.panel.hits.push(Hit {
                    rect: [bx, cy, size, 36.0],
                    action: Action::Model(if audio {
                        UserAction::ToggleMute
                    } else {
                        UserAction::OpenShellHub(ShellRoute::Hub)
                    }),
                    slider: None,
                });
            }
            let top = cy + 38.0;
            let length = sh - 52.0;
            rounded(self.ctx, bx + (size - 12.0) / 2.0, top, 12.0, length, 6.0);
            self.ctx.set_source_rgba(1.0, 1.0, 1.0, 0.60);
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
                    self.ctx.set_source_rgb(0.10, 0.48, 1.0);
                    self.ctx.fill()?;
                }
                self.panel.hits.push(Hit {
                    rect: [bx, top, size, length],
                    action: Action::Model(if audio {
                        UserAction::ToggleMute
                    } else {
                        UserAction::OpenShellHub(ShellRoute::Hub)
                    }),
                    slider: Some((audio, top, length)),
                });
            } else {
                self.ink();
                text(
                    self.ctx,
                    "—",
                    bx,
                    top + length / 2.0 - 10.0,
                    size,
                    14.0,
                    true,
                )?;
            }
            cy += sh + 14.0;
        }
        self.rule(x + 16.0, cy, (w - 32.0).max(1.0))?;
        cy += 16.0;
        for (label, route) in [
            ("⌁", ShellRoute::Hub),
            ("◐", ShellRoute::Theme),
            ("▣", ShellRoute::Wallpaper),
            ("▤", ShellRoute::Calendar),
        ] {
            if cy + size + 64.0 > y + h {
                break;
            }
            self.button(
                [bx, cy, size, size],
                label,
                Action::Model(UserAction::OpenShellHub(route)),
                false,
            )?;
            cy += size + 8.0;
        }
        if h >= 60.0 {
            self.button(
                [bx, y + h - size - 14.0, size, size],
                "⚙",
                Action::Model(UserAction::OpenShellHub(ShellRoute::Hub)),
                false,
            )?;
        }
        Ok(())
    }

    pub(super) fn bottom(&mut self, x: f64, y: f64, w: f64, h: f64) -> Result<()> {
        let size = (h - 20.0).clamp(1.0, 52.0);
        let cy = y + (h - size) / 2.0;
        let mut cx = x + 18.0;
        if w > 700.0 {
            self.ink();
            text(
                self.ctx,
                self.snapshot
                    .media
                    .title
                    .as_deref()
                    .unwrap_or("Nothing playing"),
                cx,
                cy + 4.0,
                160.0,
                12.0,
                false,
            )?;
            text(
                self.ctx,
                self.snapshot.media.artist.as_deref().unwrap_or("JWM Media"),
                cx,
                cy + 25.0,
                160.0,
                10.0,
                false,
            )?;
            cx += 172.0;
            self.button(
                [cx, cy, 44.0, size],
                if self.snapshot.media.playback == xbar_core::model::MediaPlayback::Playing {
                    "Ⅱ"
                } else {
                    "▶"
                },
                Action::Model(UserAction::MediaPlayPause),
                false,
            )?;
            cx += 60.0;
        }
        let room = ((w - (cx - x) - 104.0) / 154.0).max(0.0) as usize;
        let overflow = self.tasks.len() > room;
        let trailing = if overflow { 152.0 } else { 104.0 };
        let capacity = ((w - (cx - x) - trailing) / 154.0).max(0.0) as usize;
        if self.tasks.is_empty() && w - cx + x > 220.0 {
            self.button(
                [cx, cy, 154.0, size],
                if self.snapshot.client_name.is_empty() {
                    "Desktop"
                } else {
                    &self.snapshot.client_name
                },
                Action::Model(UserAction::OpenShellHub(ShellRoute::Applications)),
                true,
            )?;
            cx += 166.0;
        }
        for task in self
            .tasks
            .iter()
            .cycle()
            .skip(self.panel.task_offset % self.tasks.len().max(1))
            .take(self.tasks.len())
        {
            if cx + 154.0 > x + w - trailing {
                break;
            }
            let label = if task.is_minimized {
                format!("▱ {}", task.name)
            } else {
                task.name.clone()
            };
            self.button(
                [cx, cy, 142.0, size],
                &label,
                Action::FocusWindow(task.id),
                task.is_focused,
            )?;
            cx += 154.0;
        }
        for window in self
            .snapshot
            .minimized_windows
            .iter()
            .filter(|window| !self.tasks.iter().any(|task| task.id == window.token.get()))
            .take(16)
        {
            if cx + 154.0 > x + w - trailing {
                break;
            }
            let geometry = DockItemGeometry::new(
                self.panel.placement.x.saturating_add(cx as i32),
                self.panel.placement.y.saturating_add(cy as i32),
                142,
                size as u32,
            );
            self.button(
                [cx, cy, 142.0, size],
                &window.title,
                Action::Model(UserAction::RestoreWindow {
                    window: window.token,
                    wm_session_id: self.snapshot.wm_session_id,
                    minimized_generation: self.snapshot.wm_sequence.unwrap_or(0),
                    geometry,
                }),
                false,
            )?;
            cx += 154.0;
        }
        if w > 110.0 {
            if overflow && w > 170.0 {
                self.button(
                    [x + w - 146.0, cy, 40.0, size],
                    &format!("+{}", self.tasks.len().saturating_sub(capacity)),
                    Action::CycleTasks,
                    false,
                )?;
            }
            self.button(
                [x + w - 98.0, cy, 40.0, size],
                "⌕",
                Action::Model(UserAction::OpenShellHub(ShellRoute::Applications)),
                false,
            )?;
            self.button(
                [x + w - 50.0, cy, 40.0, size],
                "▦",
                Action::Model(UserAction::OpenShellHub(ShellRoute::Hub)),
                false,
            )?;
        }
        Ok(())
    }
}

pub(super) fn rounded(ctx: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
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
        "☼" | "⚙" => {
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
    fn overflow_keeps_every_task_reachable_without_overlapping_trailing_controls() {
        let snapshot = xbar_core::model::BarModel::default().snapshot();
        let tasks: Vec<_> = (0..12)
            .map(|id| crate::shell_ipc::Task {
                id,
                name: format!("Task {id}"),
                monitor: 0,
                is_focused: false,
                is_minimized: false,
                is_dock: false,
                is_desktop: false,
                is_swallowed: false,
            })
            .collect();
        let mut seen = std::collections::BTreeSet::new();
        let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, 800, 88).unwrap();
        let ctx = Context::new(&surface).unwrap();
        for offset in 0..tasks.len() {
            let mut panel = Panel {
                task_offset: offset,
                edge: BarEdge::Bottom,
                window: 0,
                gc: 0,
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
            };
            Painter {
                ctx: &ctx,
                panel: &mut panel,
                snapshot: &snapshot,
                tasks: &tasks,
                dark: false,
            }
            .bottom(0.0, 0.0, 800.0, 76.0)
            .unwrap();
            assert!(
                panel
                    .hits
                    .iter()
                    .any(|hit| hit.action == Action::CycleTasks)
            );
            for hit in panel.hits {
                if let Action::FocusWindow(id) = hit.action {
                    seen.insert(id);
                    assert!(hit.rect[0] + hit.rect[2] <= 800.0 - 152.0);
                }
            }
        }
        assert_eq!(seen.len(), tasks.len());
    }
}
