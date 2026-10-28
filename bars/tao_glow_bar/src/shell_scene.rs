//! The four-surface desktop shell. Geometry and actions are produced together:
//! the pixels shown here are also the native windows' pointer targets.

use anyhow::Result;
use cairo::{Context, ImageSurface, LineCap, LineJoin, LinearGradient, Operator};
use pango::{Alignment, EllipsizeMode, FontDescription, Weight};
use std::f64::consts::{FRAC_PI_2, PI};
use xbar_core::{
    BarEdge, BarSnapshot, MediaPlayback, Percent, ShellRoute, TagId, ThemeMode, UserAction,
};

use crate::shell_actions::{LaunchTarget, ShellAction, SliderKind};

const INK: Color = Color(0.09, 0.16, 0.25, 1.0);
const MUTED: Color = Color(0.12, 0.20, 0.29, 0.78);
const WHITE: Color = Color(1.0, 1.0, 1.0, 0.97);
const BLUE: Color = Color(0.035, 0.39, 0.87, 1.0);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShellRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl ShellRect {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    fn inset(self, x: f64, y: f64) -> Self {
        Self::new(
            self.x + x,
            self.y + y,
            (self.width - 2.0 * x).max(0.0),
            (self.height - 2.0 * y).max(0.0),
        )
    }

    fn intersect(self, viewport: Self) -> Option<Self> {
        let x = self.x.max(viewport.x);
        let y = self.y.max(viewport.y);
        let right = (self.x + self.width).min(viewport.x + viewport.width);
        let bottom = (self.y + self.height).min(viewport.y + viewport.height);
        (right > x && bottom > y).then(|| Self::new(x, y, right - x, bottom - y))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HitTarget {
    pub rect: ShellRect,
    pub action: ShellAction,
}

#[derive(Debug, Default)]
pub struct ShellScene {
    pub targets: Vec<HitTarget>,
}

impl ShellScene {
    pub fn hit(&self, x: f64, y: f64) -> Option<&HitTarget> {
        self.targets
            .iter()
            .rev()
            .find(|target| target.rect.contains(x, y))
    }
}

/// Also used for the native bounding/input shape, so the rounded corners do
/// not steal clicks from whatever is visible underneath them.
pub fn panel_radius(edge: BarEdge, width: f64, height: f64) -> f64 {
    let radius: f64 = if edge == BarEdge::Bottom { 26.0 } else { 30.0 };
    radius.min(width.max(0.0) * 0.5).min(height.max(0.0) * 0.5)
}

/// Paint one bar-local logical viewport. The backdrop is already sampled and
/// blurred at the native window's global position; it may have physical-pixel
/// dimensions at HiDPI. Everything outside the rounded panel stays transparent.
#[allow(clippy::too_many_arguments)]
pub fn render(
    context: &Context,
    edge: BarEdge,
    width: f64,
    height: f64,
    snapshot: &BarSnapshot,
    backdrop: Option<&ImageSurface>,
    hovered: Option<ShellAction>,
    pressed: Option<ShellAction>,
) -> Result<ShellScene> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Ok(ShellScene::default());
    }
    context.save()?;
    context.set_operator(Operator::Clear);
    context.paint()?;
    context.set_operator(Operator::Over);
    let bounds = ShellRect::new(0.0, 0.0, width, height);
    let radius = panel_radius(edge, width, height);
    rounded(context, bounds.inset(0.55, 0.55), (radius - 0.55).max(0.0));
    context.clip();
    if let Some(backdrop) = backdrop.filter(|image| image.width() > 0 && image.height() > 0) {
        context.save()?;
        context.scale(
            width / f64::from(backdrop.width()),
            height / f64::from(backdrop.height()),
        );
        context.set_source_surface(backdrop, 0.0, 0.0)?;
        context.paint_with_alpha(0.86)?;
        context.restore()?;
    } else {
        Color(0.64, 0.76, 0.85, 0.78).apply(context);
        context.paint()?;
    }
    let dark = snapshot.theme == ThemeMode::Dark;
    let glass = LinearGradient::new(0.0, 0.0, width * 0.22, height);
    if dark {
        glass.add_color_stop_rgba(0.0, 0.08, 0.13, 0.21, 0.76);
        glass.add_color_stop_rgba(0.5, 0.05, 0.11, 0.19, 0.68);
        glass.add_color_stop_rgba(1.0, 0.04, 0.10, 0.18, 0.74);
    } else if edge == BarEdge::Top {
        glass.add_color_stop_rgba(0.0, 0.90, 0.96, 1.0, 0.66);
        glass.add_color_stop_rgba(0.5, 0.87, 0.94, 1.0, 0.65);
        glass.add_color_stop_rgba(1.0, 0.84, 0.92, 1.0, 0.65);
    } else if edge == BarEdge::Bottom {
        glass.add_color_stop_rgba(0.0, 0.86, 0.94, 1.0, 0.62);
        glass.add_color_stop_rgba(0.5, 0.83, 0.92, 1.0, 0.61);
        glass.add_color_stop_rgba(1.0, 0.80, 0.90, 1.0, 0.60);
    } else {
        glass.add_color_stop_rgba(0.0, 0.97, 0.98, 1.0, 0.66);
        glass.add_color_stop_rgba(0.5, 0.76, 0.89, 1.0, 0.65);
        glass.add_color_stop_rgba(1.0, 0.72, 0.86, 0.96, 0.65);
    }
    context.set_source(&glass)?;
    context.paint()?;
    let mut painter = Painter {
        context,
        snapshot,
        viewport: bounds,
        hovered,
        pressed,
        dark,
        scene: ShellScene::default(),
    };
    match edge {
        BarEdge::Top => painter.top()?,
        BarEdge::Left => painter.left()?,
        BarEdge::Right => painter.right()?,
        BarEdge::Bottom => painter.bottom()?,
    }
    context.reset_clip();
    rounded(context, bounds.inset(0.55, 0.55), (radius - 0.55).max(0.0));
    context.set_line_width(1.05);
    Color(1.0, 1.0, 1.0, if dark { 0.32 } else { 0.65 }).apply(context);
    context.stroke()?;
    context.restore()?;
    Ok(painter.scene)
}

#[derive(Clone, Copy)]
struct Color(f64, f64, f64, f64);

impl Color {
    fn apply(self, context: &Context) {
        context.set_source_rgba(self.0, self.1, self.2, self.3);
    }
}

struct Painter<'a> {
    context: &'a Context,
    snapshot: &'a BarSnapshot,
    viewport: ShellRect,
    hovered: Option<ShellAction>,
    pressed: Option<ShellAction>,
    dark: bool,
    scene: ShellScene,
}

#[derive(Clone, Copy)]
enum Icon {
    Grid,
    Folder,
    Terminal,
    Browser,
    Music,
    Bell,
    Sun,
    Wifi,
    Theme,
    Display,
    Search,
    Plus,
    Play,
    Pause,
    Screenshot,
}

#[derive(Clone)]
struct StatusItem {
    width: f64,
    priority: u8,
    action: ShellAction,
    label: String,
    value: Option<String>,
    icon: Option<Icon>,
    reading: Option<Percent>,
}

impl Painter<'_> {
    fn ink(&self) -> Color {
        if self.dark {
            Color(0.91, 0.96, 1.0, 1.0)
        } else {
            INK
        }
    }

    fn muted(&self) -> Color {
        if self.dark {
            Color(0.83, 0.90, 0.98, 0.82)
        } else {
            MUTED
        }
    }

    fn target(&mut self, rect: ShellRect, action: ShellAction) {
        if let Some(rect) = rect.intersect(self.viewport) {
            self.scene.targets.push(HitTarget { rect, action });
        }
    }

    fn pill(
        &mut self,
        rect: ShellRect,
        action: ShellAction,
        selected: bool,
        radius: f64,
    ) -> Result<()> {
        let color = if selected {
            if self.pressed == Some(action) {
                Color(0.025, 0.31, 0.72, 1.0)
            } else {
                BLUE
            }
        } else if self.pressed == Some(action) {
            Color(1.0, 1.0, 1.0, if self.dark { 0.28 } else { 0.62 })
        } else if self.hovered == Some(action) {
            Color(1.0, 1.0, 1.0, if self.dark { 0.21 } else { 0.46 })
        } else {
            Color(1.0, 1.0, 1.0, if self.dark { 0.11 } else { 0.27 })
        };
        rounded(self.context, rect, radius);
        color.apply(self.context);
        self.context.fill()?;
        if self.pressed == Some(action) {
            rounded(
                self.context,
                rect.inset(0.65, 0.65),
                (radius - 0.65).max(0.0),
            );
            self.context.set_line_width(1.3);
            if self.dark {
                Color(0.75, 0.90, 1.0, 0.60)
            } else {
                Color(0.09, 0.27, 0.49, 0.52)
            }
            .apply(self.context);
            self.context.stroke()?;
        }
        self.target(rect, action);
        Ok(())
    }

    fn icon_button(&mut self, rect: ShellRect, icon: Icon, action: ShellAction) -> Result<()> {
        self.pill(rect, action, false, rect.width.min(rect.height) * 0.32)?;
        draw_icon(
            self.context,
            icon,
            rect,
            rect.width.min(rect.height) * 0.60,
            self.ink(),
        )
    }

    fn tag(&mut self, rect: ShellRect, index: usize, label: &str, side: bool) -> Result<()> {
        let Some(tag) = TagId::new(index) else {
            return Ok(());
        };
        let Some(state) = self.snapshot.tags.get(index) else {
            return Ok(());
        };
        let selected = state.selected || self.snapshot.active_tag == Some(tag);
        let action = ShellAction::User(UserAction::ViewTagOn {
            tag,
            monitor: self.snapshot.monitor,
        });
        self.pill(rect, action, selected, if side { 15.0 } else { 13.0 })?;
        text(
            self.context,
            label,
            rect.inset(3.0, 0.0),
            13.0,
            if selected { WHITE } else { self.ink() },
            Alignment::Center,
            false,
        )?;
        if state.urgent || state.occupied {
            let color = if state.urgent {
                Color(1.0, 0.31, 0.26, 1.0)
            } else if selected {
                WHITE
            } else {
                Color(0.18, 0.37, 0.57, 0.75)
            };
            color.apply(self.context);
            let x = if side {
                rect.x + rect.width * 0.77
            } else {
                rect.x + rect.width * 0.5
            };
            let y = if side {
                rect.y + rect.height * 0.5
            } else {
                rect.y + rect.height - 3.0
            };
            self.context.arc(x, y, 1.4, 0.0, PI * 2.0);
            self.context.fill()?;
        }
        Ok(())
    }

    fn top(&mut self) -> Result<()> {
        let width = self.viewport.width;
        let height = self.viewport.height;
        let pad = 10.0_f64.min(width * 0.04);
        let y = 5.0_f64.min(height * 0.12);
        let h = (height - y * 2.0).max(1.0);
        let lead_end = (width * 0.42).min(540.0);
        let layout_width = 32.0_f64.min((lead_end - pad).max(0.0) * 0.3);
        let tag_space = (lead_end - pad - layout_width - 18.0).max(0.0);
        let full_width: f64 = (0..self.snapshot.tags.len())
            .map(|index| tag_width(index) + 6.0)
            .sum::<f64>()
            - 6.0;
        let compact = full_width > tag_space;
        let capacity = if compact {
            ((tag_space + 6.0) / 38.0).floor().max(0.0) as usize
        } else {
            self.snapshot.tags.len()
        };
        let visible = visible_tags(
            self.snapshot.tags.len(),
            capacity,
            self.snapshot.active_tag.map(TagId::index),
        );
        let mut x = pad;
        for index in visible {
            let w = if compact { 32.0 } else { tag_width(index) };
            let label = if compact {
                (index + 1).to_string()
            } else {
                tag_label(index)
            };
            self.tag(ShellRect::new(x, y, w, h), index, &label, false)?;
            x += w + 6.0;
        }
        if layout_width >= 18.0 && x + layout_width <= lead_end {
            let rect = ShellRect::new(x, y, layout_width, h);
            self.pill(rect, ShellAction::CycleLayout, false, 11.0)?;
            let label = if self.snapshot.layout_symbol.is_empty() {
                "[ ]"
            } else {
                &self.snapshot.layout_symbol
            };
            text(
                self.context,
                label,
                rect.inset(2.0, 0.0),
                12.0,
                self.ink(),
                Alignment::Center,
                false,
            )?;
        }
        let divider_x = lead_end + 1.0;
        line(
            self.context,
            divider_x,
            2.0,
            divider_x,
            height - 2.0,
            Color(0.16, 0.26, 0.37, 0.3),
            1.0,
        )?;
        line(
            self.context,
            divider_x + 1.0,
            2.0,
            divider_x + 1.0,
            height - 2.0,
            Color(1.0, 1.0, 1.0, 0.65),
            1.0,
        )?;

        let mut items = Vec::new();
        for window in self.snapshot.minimized_windows.iter().take(2) {
            items.push(StatusItem {
                width: 32.0,
                priority: 20,
                action: restore_action(self.snapshot, window.token),
                label: window.initial().to_string(),
                value: None,
                icon: None,
                reading: None,
            });
        }
        let hub = ShellAction::User(UserAction::OpenShellHub(ShellRoute::Hub));
        items.extend([
            StatusItem {
                width: 32.0,
                priority: 95,
                action: ShellAction::User(UserAction::OpenShellHub(ShellRoute::Applications)),
                label: String::new(),
                value: None,
                icon: Some(Icon::Grid),
                reading: None,
            },
            StatusItem {
                width: 86.0,
                priority: 65,
                action: hub,
                label: "CPU".into(),
                value: Some(percent(self.snapshot.system.cpu_percent)),
                icon: None,
                reading: self.snapshot.system.cpu_percent,
            },
            StatusItem {
                width: 92.0,
                priority: 60,
                action: hub,
                label: "MEM".into(),
                value: Some(percent(self.snapshot.system.memory_percent)),
                icon: None,
                reading: self.snapshot.system.memory_percent,
            },
            StatusItem {
                width: 34.0,
                priority: 55,
                action: ShellAction::Network,
                label: "NET".into(),
                value: None,
                icon: None,
                reading: None,
            },
            StatusItem {
                width: 70.0,
                priority: 90,
                action: ShellAction::User(UserAction::OpenAudioControl),
                label: String::new(),
                value: Some(if self.snapshot.audio.muted {
                    "Mute".into()
                } else {
                    percent(self.snapshot.audio.volume_percent)
                }),
                icon: Some(Icon::Music),
                reading: self.snapshot.audio.volume_percent,
            },
            StatusItem {
                width: 70.0,
                priority: 75,
                action: hub,
                label: String::new(),
                value: Some(percent(self.snapshot.brightness.percent)),
                icon: Some(Icon::Sun),
                reading: self.snapshot.brightness.percent,
            },
            StatusItem {
                width: 84.0,
                priority: if self.snapshot.battery.present {
                    85
                } else {
                    40
                },
                action: ShellAction::User(UserAction::RefreshBattery),
                label: "BAT".into(),
                value: Some(percent(self.snapshot.battery.percent)),
                icon: None,
                reading: self.snapshot.battery.percent,
            },
            StatusItem {
                width: 32.0,
                priority: 50,
                action: ShellAction::User(UserAction::ToggleTheme),
                label: String::new(),
                value: None,
                icon: Some(Icon::Sun),
                reading: None,
            },
            StatusItem {
                width: 32.0,
                priority: 45,
                action: ShellAction::User(UserAction::Screenshot),
                label: String::new(),
                value: None,
                icon: Some(Icon::Screenshot),
                reading: None,
            },
            StatusItem {
                width: 45.0,
                priority: 100,
                action: ShellAction::User(UserAction::OpenShellHub(ShellRoute::Calendar)),
                label: clock(&self.snapshot.time),
                value: None,
                icon: None,
                reading: None,
            },
        ]);
        let available = (width - pad - lead_end - 10.0).max(0.0);
        let keep = status_allocation(&items, available);
        let total = items
            .iter()
            .zip(&keep)
            .filter(|(_, keep)| **keep)
            .map(|(item, _)| item.width + 6.0)
            .sum::<f64>()
            - 6.0;
        let mut x = (width - pad - total.max(0.0)).max(lead_end + 8.0);
        for (item, keep) in items.into_iter().zip(keep) {
            if !keep {
                continue;
            }
            let rect = ShellRect::new(x, y, item.width, h);
            self.pill(rect, item.action, false, 12.0)?;
            if let Some(value) = item.value {
                if let Some(icon) = item.icon {
                    draw_icon(
                        self.context,
                        icon,
                        ShellRect::new(x + 5.0, y, 22.0, h),
                        13.0,
                        self.muted(),
                    )?;
                } else {
                    text(
                        self.context,
                        &item.label,
                        ShellRect::new(x + 9.0, y, item.width * 0.5, h),
                        12.0,
                        self.ink(),
                        Alignment::Left,
                        false,
                    )?;
                }
                text(
                    self.context,
                    &value,
                    ShellRect::new(x + item.width * 0.48, y, item.width * 0.45, h),
                    12.0,
                    self.ink(),
                    Alignment::Right,
                    false,
                )?;
                if let Some(reading) = item.reading {
                    line(
                        self.context,
                        x + 4.0,
                        y + h - 2.2,
                        x + 4.0 + (item.width - 8.0) * reading.as_f64() / 100.0,
                        y + h - 2.2,
                        Color(0.08, 0.48, 0.86, 0.55),
                        1.2,
                    )?;
                }
            } else if let Some(icon) = item.icon {
                draw_icon(self.context, icon, rect, 14.0, self.ink())?;
            } else {
                text(
                    self.context,
                    &item.label,
                    rect.inset(3.0, 0.0),
                    12.0,
                    self.ink(),
                    Alignment::Center,
                    false,
                )?;
            }
            x += item.width + 6.0;
        }
        Ok(())
    }

    fn left(&mut self) -> Result<()> {
        let w = self.viewport.width;
        let h = self.viewport.height;
        let s = (h / 656.0).min(1.0);
        let pad = 8.0_f64.min(w * 0.15);
        let cell_w = (w - pad * 2.0).max(1.0);
        let cell_h = (44.0 * s).max(18.0).min(h * 0.2);
        let gap = (8.0 * s).max(2.0);
        let plus = ShellRect::new(pad, (h - 14.0 * s - cell_h).max(0.0), cell_w, cell_h);
        let launchers = [
            (
                Icon::Grid,
                ShellAction::User(UserAction::OpenShellHub(ShellRoute::Applications)),
            ),
            (Icon::Folder, ShellAction::Launch(LaunchTarget::Files)),
            (Icon::Terminal, ShellAction::Launch(LaunchTarget::Terminal)),
            (Icon::Browser, ShellAction::Launch(LaunchTarget::Browser)),
            (Icon::Music, ShellAction::Launch(LaunchTarget::Music)),
        ];
        let mut y = 14.0 * s;
        for (icon, action) in launchers {
            if y + cell_h + gap > plus.y {
                break;
            }
            self.icon_button(ShellRect::new(pad, y, cell_w, cell_h), icon, action)?;
            y += cell_h + gap;
        }
        let divider_y = y + 4.0 * s;
        if divider_y + 12.0 * s + cell_h + gap < plus.y {
            line(
                self.context,
                pad + 6.0,
                divider_y,
                w - pad - 6.0,
                divider_y,
                Color(0.25, 0.37, 0.49, 0.17),
                1.0,
            )?;
            y = divider_y + 12.0 * s;
            let capacity = (((plus.y - gap - y) + gap) / (cell_h + gap))
                .floor()
                .max(0.0) as usize;
            let visible = visible_tags(
                self.snapshot.tags.len(),
                capacity.min(5),
                self.snapshot.active_tag.map(TagId::index),
            );
            for index in visible {
                self.tag(
                    ShellRect::new(pad, y, cell_w, cell_h),
                    index,
                    &(index + 1).to_string(),
                    true,
                )?;
                y += cell_h + gap;
            }
        }
        self.icon_button(plus, Icon::Plus, ShellAction::NextWorkspace)?;
        Ok(())
    }

    fn right(&mut self) -> Result<()> {
        let w = self.viewport.width;
        let h = self.viewport.height;
        let s = (h / 656.0).min(1.0);
        let pad = 8.0_f64.min(w * 0.15);
        let cw = (w - pad * 2.0).max(1.0);
        let button = |y: f64| ShellRect::new(pad, y * s, cw, 44.0 * s);
        self.icon_button(
            button(14.0),
            Icon::Bell,
            ShellAction::User(UserAction::OpenShellHub(ShellRoute::Notifications)),
        )?;
        self.slider_group(
            ShellRect::new(pad, 76.0 * s, cw, 142.0 * s),
            SliderKind::Volume,
            self.snapshot.audio.volume_percent,
            s,
        )?;
        self.slider_group(
            ShellRect::new(pad, 232.0 * s, cw, 142.0 * s),
            SliderKind::Brightness,
            self.snapshot.brightness.percent,
            s,
        )?;
        line(
            self.context,
            pad + 7.0,
            388.0 * s,
            w - pad - 7.0,
            388.0 * s,
            Color(0.25, 0.37, 0.49, 0.18),
            1.0,
        )?;
        self.icon_button(button(404.0), Icon::Wifi, ShellAction::Network)?;
        self.icon_button(
            button(454.0),
            Icon::Theme,
            ShellAction::User(UserAction::ToggleTheme),
        )?;
        self.icon_button(button(506.0), Icon::Display, ShellAction::Display)?;
        let bottom = ShellRect::new(pad, (h - 58.0 * s).max(0.0), cw, 44.0 * s);
        self.icon_button(
            bottom,
            Icon::Sun,
            ShellAction::User(UserAction::OpenShellHub(ShellRoute::Wallpaper)),
        )?;
        Ok(())
    }

    fn slider_group(
        &mut self,
        rect: ShellRect,
        kind: SliderKind,
        reading: Option<Percent>,
        scale: f64,
    ) -> Result<()> {
        rounded(self.context, rect, 20.0);
        Color(1.0, 1.0, 1.0, if self.dark { 0.11 } else { 0.27 }).apply(self.context);
        self.context.fill()?;
        let action = if kind == SliderKind::Volume {
            ShellAction::User(UserAction::OpenAudioControl)
        } else {
            ShellAction::Display
        };
        let icon_rect = ShellRect::new(rect.x, rect.y + 2.0 * scale, rect.width, 32.0 * scale);
        draw_icon(
            self.context,
            if kind == SliderKind::Volume {
                Icon::Music
            } else {
                Icon::Sun
            },
            icon_rect,
            20.0 * scale,
            self.ink(),
        )?;
        self.target(icon_rect, action);
        let track = ShellRect::new(
            rect.x + (rect.width - 12.0_f64.min(rect.width * 0.4)) * 0.5,
            rect.y + 38.0 * scale,
            12.0_f64.min(rect.width * 0.4),
            90.0 * scale,
        );
        rounded(self.context, track, track.width * 0.5);
        Color(
            1.0,
            1.0,
            1.0,
            if self.dark {
                if reading.is_some() { 0.42 } else { 0.14 }
            } else if reading.is_some() {
                0.66
            } else {
                0.3
            },
        )
        .apply(self.context);
        self.context.fill()?;
        if let Some(reading) = reading {
            let y = track.y + track.height * (1.0 - reading.as_f64() / 100.0);
            line(
                self.context,
                track.x,
                y,
                track.x + track.width,
                y,
                self.ink(),
                1.0,
            )?;
            // A forgiving horizontal target retains the track's exact travel.
            self.target(
                ShellRect::new(
                    rect.x + 3.0,
                    track.y,
                    (rect.width - 6.0).max(0.0),
                    track.height,
                ),
                ShellAction::Slider(kind),
            );
        } else {
            text(
                self.context,
                "--",
                track,
                10.0,
                self.muted(),
                Alignment::Center,
                false,
            )?;
        }
        Ok(())
    }

    fn bottom(&mut self) -> Result<()> {
        let w = self.viewport.width;
        let h = self.viewport.height;
        let y = 12.0_f64.min(h * 0.18);
        let ch = (h - 2.0 * y).max(1.0);
        let media_width = if w >= 700.0 {
            230.0
        } else if w >= 420.0 {
            180.0
        } else {
            0.0
        };
        let small = w < 260.0;
        let icon_w = 44.0_f64.min((w - 20.0).max(0.0) * 0.25);
        let grid = ShellRect::new(w - 10.0 - icon_w, y, icon_w, ch);
        self.icon_button(
            grid,
            Icon::Grid,
            ShellAction::User(UserAction::OpenShellHub(ShellRoute::Applications)),
        )?;
        let search = ShellRect::new(grid.x - 6.0 - icon_w, y, icon_w, ch);
        if !small {
            self.icon_button(
                search,
                Icon::Search,
                ShellAction::User(UserAction::OpenShellHub(ShellRoute::Applications)),
            )?;
        }
        if media_width > 0.0 {
            let title = self
                .snapshot
                .media
                .title
                .as_deref()
                .unwrap_or("Nothing playing");
            let subtitle = self
                .snapshot
                .media
                .artist
                .as_deref()
                .or(self.snapshot.media.player.as_deref())
                .unwrap_or("JWM Media");
            let text_rect = ShellRect::new(18.0, y + 1.0, media_width - 74.0, ch * 0.5);
            text(
                self.context,
                title,
                text_rect,
                12.0,
                self.ink(),
                Alignment::Left,
                false,
            )?;
            text(
                self.context,
                subtitle,
                ShellRect::new(
                    text_rect.x,
                    text_rect.y + ch * 0.43,
                    text_rect.width,
                    ch * 0.45,
                ),
                10.5,
                self.muted(),
                Alignment::Left,
                false,
            )?;
            self.target(
                ShellRect::new(14.0, y, media_width - 60.0, ch),
                ShellAction::Launch(LaunchTarget::Music),
            );
        }
        let play = ShellRect::new(
            if media_width > 0.0 {
                media_width - 40.0
            } else {
                12.0
            },
            y,
            icon_w,
            ch,
        );
        self.icon_button(
            play,
            if self.snapshot.media.playback == MediaPlayback::Playing {
                Icon::Pause
            } else {
                Icon::Play
            },
            ShellAction::User(UserAction::MediaPlayPause),
        )?;
        let start = if media_width > 0.0 {
            media_width + 20.0
        } else {
            play.x + play.width + 12.0
        };
        let end = if small {
            grid.x - 10.0
        } else {
            search.x - 12.0
        };
        let space = (end - start).max(0.0);
        let minimum = if w >= 700.0 { 110.0 } else { 64.0 };
        let capacity = ((space + 12.0) / (minimum + 12.0)).floor().max(0.0) as usize;
        let windows = &self.snapshot.minimized_windows;
        let has_overflow = windows.len() > capacity;
        let shown = if has_overflow {
            capacity.saturating_sub(1)
        } else {
            windows.len().min(capacity)
        };
        let card_width = if shown == 0 {
            142.0
        } else {
            ((space - 12.0 * shown.saturating_sub(1) as f64) / shown as f64).clamp(minimum, 142.0)
        };
        let mut x = start;
        for (index, window) in windows.iter().take(shown).enumerate() {
            let rect = ShellRect::new(x, y, card_width, ch);
            let action = restore_action(self.snapshot, window.token);
            // The first card is the shelf's primary restore action; its blue
            // treatment is not an assertion that a hidden window has focus.
            let accent = index == 0;
            self.pill(rect, action, accent, 16.0)?;
            text(
                self.context,
                &window.title,
                rect.inset(14.0, 0.0),
                13.0,
                if accent { WHITE } else { self.ink() },
                Alignment::Center,
                false,
            )?;
            x += card_width + 12.0;
        }
        if has_overflow
            && capacity > 0
            && let Some(window) = windows.last()
        {
            let rect = ShellRect::new(x, y, (end - x).clamp(0.0, 64.0), ch);
            let action = restore_action(self.snapshot, window.token);
            self.pill(rect, action, false, 16.0)?;
            text(
                self.context,
                &format!("+{}", windows.len() - shown),
                rect,
                13.0,
                self.ink(),
                Alignment::Center,
                false,
            )?;
        }
        Ok(())
    }
}

fn restore_action(snapshot: &BarSnapshot, window: xbar_core::WindowToken) -> ShellAction {
    ShellAction::Restore {
        window,
        wm_session_id: snapshot.wm_session_id,
        minimized_generation: snapshot.wm_sequence.unwrap_or(0),
    }
}

fn percent(value: Option<Percent>) -> String {
    value.map_or_else(|| "--".to_owned(), |value| format!("{}%", value.rounded()))
}

fn clock(value: &str) -> String {
    value
        .split_whitespace()
        .rev()
        .find(|part| part.contains(':'))
        .unwrap_or(value)
        .to_owned()
}

fn tag_label(index: usize) -> String {
    match index {
        0 => "1 Dev".into(),
        1 => "2 Web".into(),
        2 => "3 Media".into(),
        3 => "4 Chat".into(),
        _ => (index + 1).to_string(),
    }
}

fn tag_width(index: usize) -> f64 {
    match index {
        0 => 54.0,
        1 => 57.0,
        2 => 68.0,
        3 => 62.0,
        _ => 32.0,
    }
}

fn visible_tags(count: usize, capacity: usize, active: Option<usize>) -> Vec<usize> {
    let mut visible: Vec<_> = (0..count.min(capacity)).collect();
    if let Some(active) = active.filter(|&active| active < count && !visible.contains(&active))
        && let Some(last) = visible.last_mut()
    {
        *last = active;
    }
    visible
}

fn status_allocation(items: &[StatusItem], available: f64) -> Vec<bool> {
    let mut ordered: Vec<_> = (0..items.len()).collect();
    ordered.sort_by_key(|&index| std::cmp::Reverse(items[index].priority));
    let mut keep = vec![false; items.len()];
    let mut used = 0.0;
    let mut count = 0;
    for index in ordered {
        let needed = items[index].width + if count == 0 { 0.0 } else { 6.0 };
        if used + needed <= available {
            keep[index] = true;
            used += needed;
            count += 1;
        }
    }
    keep
}

fn rounded(context: &Context, rect: ShellRect, radius: f64) {
    let r = radius.max(0.0).min(rect.width * 0.5).min(rect.height * 0.5);
    context.new_sub_path();
    context.arc(rect.x + rect.width - r, rect.y + r, r, -FRAC_PI_2, 0.0);
    context.arc(
        rect.x + rect.width - r,
        rect.y + rect.height - r,
        r,
        0.0,
        FRAC_PI_2,
    );
    context.arc(rect.x + r, rect.y + rect.height - r, r, FRAC_PI_2, PI);
    context.arc(rect.x + r, rect.y + r, r, PI, PI + FRAC_PI_2);
    context.close_path();
}

#[allow(clippy::too_many_arguments)]
fn text(
    context: &Context,
    value: &str,
    rect: ShellRect,
    size: f64,
    color: Color,
    align: Alignment,
    bold: bool,
) -> Result<()> {
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return Ok(());
    }
    context.save()?;
    context.rectangle(rect.x, rect.y, rect.width, rect.height);
    context.clip();
    let layout = pangocairo::functions::create_layout(context);
    let mut font = FontDescription::from_string("DejaVu Sans");
    font.set_absolute_size(size * f64::from(pango::SCALE));
    if bold {
        font.set_weight(Weight::Medium);
    }
    layout.set_font_description(Some(&font));
    layout.set_text(value);
    layout.set_single_paragraph_mode(true);
    layout.set_ellipsize(EllipsizeMode::End);
    layout.set_width((rect.width * f64::from(pango::SCALE)).min(f64::from(i32::MAX)) as i32);
    layout.set_alignment(align);
    let (_, text_height) = layout.pixel_size();
    context.move_to(
        rect.x,
        rect.y + (rect.height - f64::from(text_height)) * 0.5,
    );
    color.apply(context);
    pangocairo::functions::show_layout(context, &layout);
    context.restore()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn line(
    context: &Context,
    x: f64,
    y: f64,
    x2: f64,
    y2: f64,
    color: Color,
    width: f64,
) -> Result<()> {
    context.new_path();
    context.move_to(x, y);
    context.line_to(x2, y2);
    context.set_line_width(width);
    color.apply(context);
    context.stroke()?;
    Ok(())
}

/// Icons use a 24-unit grid and ordinary vector strokes. No private-use font
/// glyphs or desktop theme packages are needed for the shell to remain legible.
fn draw_icon(
    context: &Context,
    icon: Icon,
    rect: ShellRect,
    size: f64,
    color: Color,
) -> Result<()> {
    if size <= 0.0 {
        return Ok(());
    }
    context.save()?;
    context.translate(
        rect.x + (rect.width - size) * 0.5,
        rect.y + (rect.height - size) * 0.5,
    );
    context.scale(size / 24.0, size / 24.0);
    context.set_line_width(1.8);
    context.set_line_cap(LineCap::Round);
    context.set_line_join(LineJoin::Round);
    color.apply(context);
    context.new_path();
    match icon {
        Icon::Grid => {
            for (x, y) in [(4.0, 4.0), (14.0, 4.0), (4.0, 14.0), (14.0, 14.0)] {
                rounded(context, ShellRect::new(x, y, 6.0, 6.0), 1.5);
            }
            context.stroke()?;
        }
        Icon::Folder => {
            Color(0.02, 0.47, 0.98, 1.0).apply(context);
            rounded(context, ShellRect::new(2.0, 7.0, 20.0, 14.0), 2.5);
            context.fill()?;
            rounded(context, ShellRect::new(3.0, 3.0, 9.0, 6.0), 1.8);
            context.fill()?;
            line(
                context,
                4.0,
                8.0,
                20.0,
                8.0,
                Color(0.85, 0.94, 1.0, 0.9),
                1.1,
            )?;
        }
        Icon::Terminal => {
            rounded(context, ShellRect::new(1.0, 2.0, 22.0, 20.0), 4.0);
            context.fill()?;
            WHITE.apply(context);
            context.move_to(5.0, 7.0);
            context.line_to(9.0, 11.0);
            context.line_to(5.0, 15.0);
            context.move_to(12.0, 16.0);
            context.line_to(18.0, 16.0);
            context.stroke()?;
        }
        Icon::Browser => {
            Color(0.09, 0.58, 0.98, 1.0).apply(context);
            context.arc(12.0, 12.0, 9.5, 0.0, PI * 2.0);
            context.fill()?;
            Color(1.0, 0.42, 0.02, 1.0).apply(context);
            context.set_line_width(5.3);
            context.arc(12.0, 12.0, 8.1, -PI * 0.65, PI * 0.75);
            context.stroke()?;
            context.move_to(3.0, 3.5);
            context.line_to(11.0, 5.0);
            context.line_to(4.5, 10.0);
            context.close_path();
            context.fill()?;
        }
        Icon::Music => {
            context.move_to(8.0, 17.0);
            context.line_to(8.0, 5.0);
            context.line_to(19.0, 2.5);
            context.line_to(19.0, 14.5);
            context.stroke()?;
            context.save()?;
            context.translate(5.3, 17.2);
            context.scale(1.4, 1.0);
            context.arc(0.0, 0.0, 2.6, 0.0, PI * 2.0);
            context.fill()?;
            context.restore()?;
            context.save()?;
            context.translate(16.3, 14.7);
            context.scale(1.4, 1.0);
            context.arc(0.0, 0.0, 2.6, 0.0, PI * 2.0);
            context.fill()?;
            context.restore()?;
        }
        Icon::Bell => {
            context.move_to(5.0, 15.0);
            context.line_to(6.0, 8.0);
            context.curve_to(6.5, 1.0, 17.5, 1.0, 18.0, 8.0);
            context.line_to(19.0, 15.0);
            context.line_to(21.0, 18.0);
            context.line_to(3.0, 18.0);
            context.close_path();
            context.stroke()?;
            context.arc(12.0, 19.5, 2.3, 0.0, PI);
            context.stroke()?;
        }
        Icon::Sun => {
            context.arc(12.0, 12.0, 4.0, 0.0, PI * 2.0);
            context.stroke()?;
            for index in 0..8 {
                let a = f64::from(index) * PI / 4.0;
                context.move_to(12.0 + a.cos() * 7.5, 12.0 + a.sin() * 7.5);
                context.line_to(12.0 + a.cos() * 10.0, 12.0 + a.sin() * 10.0);
            }
            context.stroke()?;
        }
        Icon::Wifi => {
            for r in [9.0, 6.0, 3.0] {
                context.new_sub_path();
                context.arc(12.0, 19.0, r, -PI * 0.79, -PI * 0.21);
            }
            context.stroke()?;
            context.arc(12.0, 19.0, 1.2, 0.0, PI * 2.0);
            context.fill()?;
        }
        Icon::Theme => {
            context.arc(12.0, 12.0, 9.0, 0.0, PI * 2.0);
            context.stroke()?;
            context.arc(12.0, 12.0, 7.0, -FRAC_PI_2, FRAC_PI_2);
            context.close_path();
            context.fill()?;
        }
        Icon::Display => {
            rounded(context, ShellRect::new(2.0, 3.0, 20.0, 14.0), 2.0);
            context.stroke()?;
            context.move_to(12.0, 17.0);
            context.line_to(12.0, 21.0);
            context.move_to(7.0, 21.0);
            context.line_to(17.0, 21.0);
            context.stroke()?;
        }
        Icon::Search => {
            context.arc(10.5, 10.5, 6.5, 0.0, PI * 2.0);
            context.stroke()?;
            context.move_to(15.0, 15.0);
            context.line_to(21.0, 21.0);
            context.stroke()?;
        }
        Icon::Plus => {
            context.move_to(12.0, 4.0);
            context.line_to(12.0, 20.0);
            context.move_to(4.0, 12.0);
            context.line_to(20.0, 12.0);
            context.stroke()?;
        }
        Icon::Play => {
            context.move_to(7.0, 3.0);
            context.line_to(21.0, 12.0);
            context.line_to(7.0, 21.0);
            context.close_path();
            context.fill()?;
        }
        Icon::Pause => {
            rounded(context, ShellRect::new(5.0, 4.0, 5.0, 16.0), 0.8);
            rounded(context, ShellRect::new(14.0, 4.0, 5.0, 16.0), 0.8);
            context.fill()?;
        }
        Icon::Screenshot => {
            context.rectangle(5.0, 5.0, 14.0, 14.0);
            context.stroke()?;
            context.rectangle(8.0, 8.0, 8.0, 8.0);
            context.fill()?;
        }
    }
    context.restore()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use xbar_core::{BarModel, MinimizedWindow, MonitorId, TagState, WindowToken};

    fn snapshot() -> BarSnapshot {
        let mut snapshot = BarModel::default().snapshot();
        snapshot.wm_available = true;
        snapshot.wm_session_id = 17;
        snapshot.wm_sequence = Some(42);
        snapshot.tags = vec![TagState::default(); 9];
        snapshot.tags[1].selected = true;
        snapshot.active_tag = TagId::new(1);
        snapshot.audio.volume_percent = Percent::from_whole(51).ok();
        snapshot.brightness.percent = Percent::from_whole(65).ok();
        snapshot.minimized_windows = ["EdgeDockFirst", "EdgeDockSecond"]
            .into_iter()
            .enumerate()
            .map(|(i, title)| MinimizedWindow {
                token: WindowToken(i as u64 + 1),
                monitor: MonitorId(0),
                title: title.into(),
                app_id: "xmessage".into(),
                flags: 0,
            })
            .collect();
        snapshot
    }

    #[test]
    fn controls_stay_bounded_and_unambiguous_at_small_and_standard_sizes() {
        let snapshot = snapshot();
        for (edge, width, height) in [
            (BarEdge::Top, 1270, 42),
            (BarEdge::Top, 320, 32),
            (BarEdge::Bottom, 1024, 76),
            (BarEdge::Bottom, 180, 48),
            (BarEdge::Left, 60, 656),
            (BarEdge::Left, 32, 180),
            (BarEdge::Right, 60, 656),
            (BarEdge::Right, 32, 180),
        ] {
            let surface = ImageSurface::create(cairo::Format::ARgb32, width, height).unwrap();
            let context = Context::new(&surface).unwrap();
            let scene = render(
                &context,
                edge,
                f64::from(width),
                f64::from(height),
                &snapshot,
                None,
                None,
                None,
            )
            .unwrap();
            for (index, target) in scene.targets.iter().enumerate() {
                assert!(
                    target.rect.x >= 0.0
                        && target.rect.y >= 0.0
                        && target.rect.x + target.rect.width <= f64::from(width) + 0.001
                        && target.rect.y + target.rect.height <= f64::from(height) + 0.001,
                    "{edge:?} {target:?}"
                );
                for other in &scene.targets[index + 1..] {
                    let overlap = target.rect.intersect(other.rect);
                    assert!(
                        overlap.is_none(),
                        "overlapping {edge:?} targets {target:?} / {other:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn rounded_panels_keep_native_corner_pixels_transparent() {
        let snapshot = snapshot();
        for (edge, width, height) in [
            (BarEdge::Top, 1270, 42),
            (BarEdge::Bottom, 1024, 76),
            (BarEdge::Left, 60, 656),
            (BarEdge::Right, 60, 656),
        ] {
            let mut surface = ImageSurface::create(cairo::Format::ARgb32, width, height).unwrap();
            {
                let context = Context::new(&surface).unwrap();
                render(
                    &context,
                    edge,
                    f64::from(width),
                    f64::from(height),
                    &snapshot,
                    None,
                    None,
                    None,
                )
                .unwrap();
            }
            let stride = surface.stride() as usize;
            let data = surface.data().unwrap();
            for (x, y) in [
                (0, 0),
                (width - 1, 0),
                (0, height - 1),
                (width - 1, height - 1),
            ] {
                assert_eq!(
                    data[y as usize * stride + x as usize * 4 + 3],
                    0,
                    "{edge:?} corner {x},{y}"
                );
            }
        }
    }

    #[test]
    fn compact_workspaces_retain_the_actual_active_tag() {
        assert_eq!(visible_tags(9, 5, Some(8)), [0, 1, 2, 3, 8]);
        assert_eq!(visible_tags(9, 1, Some(8)), [8]);
        assert!(visible_tags(9, 0, Some(8)).is_empty());
    }

    #[test]
    fn restore_targets_capture_session_and_minimized_generation() {
        let snapshot = snapshot();
        let surface = ImageSurface::create(cairo::Format::ARgb32, 1024, 76).unwrap();
        let context = Context::new(&surface).unwrap();
        let scene = render(
            &context,
            BarEdge::Bottom,
            1024.0,
            76.0,
            &snapshot,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(scene.targets.iter().any(|target| matches!(
            target.action,
            ShellAction::Restore {
                window: WindowToken(1),
                wm_session_id: 17,
                minimized_generation: 42
            }
        )));
        assert!(scene.targets.iter().any(|target| matches!(
            target.action,
            ShellAction::Restore {
                window: WindowToken(2),
                wm_session_id: 17,
                minimized_generation: 42
            }
        )));
    }

    #[test]
    fn unavailable_hardware_has_no_invented_slider_value_or_hit_target() {
        let mut snapshot = snapshot();
        snapshot.brightness.percent = None;
        let surface = ImageSurface::create(cairo::Format::ARgb32, 60, 656).unwrap();
        let context = Context::new(&surface).unwrap();
        let scene = render(
            &context,
            BarEdge::Right,
            60.0,
            656.0,
            &snapshot,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(
            scene
                .targets
                .iter()
                .any(|target| target.action == ShellAction::Slider(SliderKind::Volume))
        );
        assert!(
            !scene
                .targets
                .iter()
                .any(|target| target.action == ShellAction::Slider(SliderKind::Brightness))
        );
        assert_eq!(percent(None), "--");
    }

    #[test]
    fn theme_changes_the_glass_pixels_without_changing_pointer_targets() {
        let mut snapshot = snapshot();
        let mut samples = Vec::new();
        let mut target_sets = Vec::new();
        for theme in [ThemeMode::Light, ThemeMode::Dark] {
            snapshot.theme = theme;
            let mut surface = ImageSurface::create(cairo::Format::ARgb32, 60, 656).unwrap();
            {
                let context = Context::new(&surface).unwrap();
                let scene = render(
                    &context,
                    BarEdge::Left,
                    60.0,
                    656.0,
                    &snapshot,
                    None,
                    None,
                    None,
                )
                .unwrap();
                target_sets.push(scene.targets);
            }
            let stride = surface.stride() as usize;
            let data = surface.data().unwrap();
            let pixel = 570 * stride + 30 * 4;
            samples.push(
                data[pixel..pixel + 3]
                    .iter()
                    .map(|&value| u32::from(value))
                    .sum::<u32>(),
            );
        }
        assert!(
            samples[1] * 2 < samples[0],
            "dark/light pixel sums: {samples:?}"
        );
        assert_eq!(target_sets[0], target_sets[1]);
    }
}
