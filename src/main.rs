// xilem_bar — xilem port of iced_bar.
//
// Feature parity goals with iced_bar:
//   * 9 nerd-font workspace tag buttons with selected/filled/urgent/occupied visuals
//   * Layout toggle button + 3-option selector
//   * Pills: CPU, memory, battery, brightness, volume, screenshot, time, monitor, scale
//   * Click semantics: tag → view-tag command; volume scroll/click/right-click;
//     brightness scroll/click/right-click; screenshot pill spawns `flameshot gui`
//   * Background subscription thread reading SharedRingBuffer; 1Hz clock + system
//     monitor refresh
//
// xilem is closure-based (no Message enum), so each interactive view directly
// mutates state. A background thread pushes updates onto an mpsc channel that
// xilem's `worker` view drains.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::Local;
use log::{error, info, warn};

use shared_structures::{CommandType, MonitorInfo, SharedCommand, SharedMessage, SharedRingBuffer};
use xbar_core::audio_manager::AudioManager;
use xbar_core::brightness::BrightnessManager;
use xbar_core::initialize_logging;
use xbar_core::system_monitor::SystemMonitor;

use masonry::kurbo::Axis;
use masonry::layout::{Dim, Length};
use masonry::peniko::Color;
use masonry::properties::{Dimensions, Padding};
use winit::dpi::LogicalSize;
use winit::window::WindowLevel;
use xilem::core::{MessageProxy, NoElement, View, fork};
use xilem::style::Style;
use xilem::view::{
    CrossAxisAlignment, FlexSpacer, PointerButton, button, button_any_pointer, flex, label,
    sized_box, task_raw,
};
use xilem::{EventLoop, ViewCtx, WidgetView, WindowOptions, Xilem};

// -------- Constants (mirror iced_bar) ----------------------------------------

const NERD_FONT: &str = "JetBrainsMono Nerd Font";

const TAG_ICONS: [&str; 9] = [
    "\u{F0A1E}",
    "\u{F0239}",
    "\u{F0A1B}",
    "\u{F0B79}",
    "\u{F024B}",
    "\u{F0388}",
    "\u{F0567}",
    "\u{F01F0}",
    "\u{F0297}",
];

const ICON_CPU: &str = "\u{F0FB1}";
const ICON_MEM: &str = "\u{F035B}";
const ICON_BAT_FULL: &str = "\u{F0079}";
const ICON_BAT_CHG: &str = "\u{F0084}";
const ICON_VOL_HIGH: &str = "\u{F057E}";
const ICON_VOL_MID: &str = "\u{F0580}";
const ICON_VOL_LOW: &str = "\u{F057F}";
const ICON_VOL_MUTE: &str = "\u{F075F}";
const ICON_BRIGHT: &str = "\u{F00DE}";
const ICON_SHOT: &str = "\u{F0104}";
const ICON_TIME: &str = "\u{F0954}";
const ICON_MON: &str = "\u{F0379}";
const ICON_M0: &str = "\u{F02DA}";
const ICON_M1: &str = "\u{F02DB}";
const ICON_SUN: &str = "\u{F0599}";
const ICON_MOON: &str = "\u{F0594}";

const DEFAULT_TAB_WIDTH: f64 = 42.0;
const MIN_TAB_WIDTH: f64 = 40.0;
const MAX_TAB_WIDTH: f64 = 56.0;
const TAB_SPACING: f64 = 4.0;
const TAB_FONT_SIZE: f32 = 12.0;
const PILL_FONT_SIZE: f32 = 11.0;
const LEFT_SECTION_GAP: f64 = 1.0;
const CENTER_SECTION_GAP: f64 = 2.0;
const RIGHT_SECTION_GAP: f64 = 4.0;
const RIGHTMOST_SECTION_GAP: f64 = 8.0;

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb8(r, g, b)
}
fn pill_padding() -> Padding {
    Padding::from_vh(Length::px(0.0), Length::px(2.0))
}
// -------- App state ----------------------------------------------------------

#[derive(Debug, Clone)]
enum WorkerEvent {
    Tick,                       // 1Hz clock tick
    Shared(Box<SharedMessage>), // shared-memory update
    SharedError(String),
}

struct XilemBar {
    active_tab: usize,
    tab_colors: [Color; 9],
    shared_buffer_rc: Option<Arc<SharedRingBuffer>>,

    monitor_info_opt: Option<MonitorInfo>,
    formated_now: String,
    show_seconds: bool,
    layout_symbol: String,
    monitor_num: i32,
    layout_selector_open: bool,

    audio_manager: AudioManager,
    system_monitor: SystemMonitor,
    brightness_manager: BrightnessManager,

    theme_mode: ThemeMode,
    theme: Theme,

    last_clock_update: Instant,
    last_monitor_update: Instant,
}

impl Default for XilemBar {
    fn default() -> Self {
        Self::new()
    }
}

impl XilemBar {
    fn new() -> Self {
        let args: Vec<String> = env::args().collect();
        let shared_path = args.iter().skip(1).last().cloned().unwrap_or_default();

        let shared_buffer_rc =
            SharedRingBuffer::create_shared_ring_buffer_aux(&shared_path).map(Arc::new);

        let theme_mode = load_theme_mode();
        Self {
            active_tab: 0,
            tab_colors: [
                rgb(0xFF, 0x6B, 0x6B),
                rgb(0x4E, 0xCD, 0xC4),
                rgb(0x45, 0xB7, 0xD1),
                rgb(0x96, 0xCE, 0xB4),
                rgb(0xFE, 0xCA, 0x57),
                rgb(0xFF, 0x9F, 0xF3),
                rgb(0x54, 0xA0, 0xFF),
                rgb(0x5F, 0x27, 0xCD),
                rgb(0x00, 0xD2, 0xD3),
            ],
            shared_buffer_rc,
            monitor_info_opt: None,
            formated_now: String::new(),
            show_seconds: true,
            layout_symbol: "[]=".to_string(),
            monitor_num: 0,
            layout_selector_open: false,
            audio_manager: AudioManager::new(),
            system_monitor: SystemMonitor::new(5),
            brightness_manager: BrightnessManager::new(),
            theme_mode,
            theme: Theme::from_mode(theme_mode),
            last_clock_update: Instant::now(),
            last_monitor_update: Instant::now(),
        }
    }

    fn toggle_theme(&mut self) {
        self.theme_mode = match self.theme_mode {
            ThemeMode::Dark => ThemeMode::Light,
            ThemeMode::Light => ThemeMode::Dark,
        };
        self.theme = Theme::from_mode(self.theme_mode);
        save_theme_mode(self.theme_mode);
    }

    fn send_tag_command(&mut self, is_view: bool) {
        let tag_bit = 1 << self.active_tab;
        let command = if is_view {
            SharedCommand::view_tag(tag_bit, self.monitor_num)
        } else {
            SharedCommand::toggle_tag(tag_bit, self.monitor_num)
        };
        if let Some(buf) = &self.shared_buffer_rc {
            match buf.send_command(command) {
                Ok(true) => info!("Sent command: {:?}", command),
                Ok(false) => warn!("Command buffer full, command dropped"),
                Err(e) => error!("Failed to send command: {}", e),
            }
        }
    }

    fn send_layout_command(&mut self, layout_index: u32) {
        let cmd = SharedCommand::new(CommandType::SetLayout, layout_index, self.monitor_num);
        if let Some(buf) = &self.shared_buffer_rc {
            let _ = buf.send_command(cmd);
        }
    }

    fn on_worker(&mut self, ev: WorkerEvent) {
        match ev {
            WorkerEvent::Tick => {
                if self.last_clock_update.elapsed() >= Duration::from_millis(900) {
                    let fmt = if self.show_seconds {
                        "%Y-%m-%d %H:%M:%S"
                    } else {
                        "%Y-%m-%d %H:%M"
                    };
                    self.formated_now = Local::now().format(fmt).to_string();
                    self.last_clock_update = Instant::now();
                }
                if self.last_monitor_update.elapsed() >= Duration::from_secs(2) {
                    self.system_monitor.update_if_needed();
                    self.audio_manager.update_if_needed();
                    self.brightness_manager.update_if_needed();
                    self.last_monitor_update = Instant::now();
                }
            }
            WorkerEvent::Shared(msg) => {
                self.monitor_info_opt = Some(msg.monitor_info);
                if let Some(mi) = &self.monitor_info_opt {
                    self.layout_symbol = mi.get_ltsymbol();
                    self.monitor_num = mi.monitor_num;
                    for (idx, ts) in mi.tag_status_vec.iter().enumerate() {
                        if ts.is_selected {
                            self.active_tab = idx;
                        }
                    }
                }
            }
            WorkerEvent::SharedError(e) => warn!("SharedMemoryError: {e}"),
        }
    }

    // -------- View helpers -----------------------------------------------------

    fn tag_visuals(&self, index: usize) -> (Color, f64, Color) {
        let tag_color = *self.tab_colors.get(index).unwrap_or(&rgb(0x66, 0x66, 0x66));
        if let Some(monitor) = &self.monitor_info_opt {
            if let Some(s) = monitor.tag_status_vec.get(index) {
                if s.is_urg {
                    return (self.theme.urgent_bg, 2.0, self.theme.urgent_border);
                }
                if s.is_filled {
                    return (tag_color, 2.0, tag_color);
                }
                if s.is_selected {
                    return (with_alpha(tag_color, 0.7), 1.5, tag_color);
                }
                if s.is_occ {
                    return (
                        with_alpha(tag_color, 0.22),
                        1.5,
                        with_alpha(tag_color, 0.95),
                    );
                }
            }
        }
        (self.theme.tag_inactive_bg, 1.0, with_alpha(tag_color, 0.9))
    }

    fn is_tag_active(&self, index: usize) -> bool {
        self.monitor_info_opt
            .as_ref()
            .and_then(|m| m.tag_status_vec.get(index))
            .map(|s| s.is_filled || s.is_selected || s.is_urg || s.is_occ)
            .unwrap_or(false)
    }

    fn workspace_tab_width(&self) -> f64 {
        let Some(monitor_width) = self
            .monitor_info_opt
            .as_ref()
            .map(|m| m.monitor_width)
            .filter(|w| *w > 0)
        else {
            return DEFAULT_TAB_WIDTH;
        };

        // Keep the left workspace group proportional to the monitor width,
        // while clamping individual tab width to avoid extreme sizes.
        let group_width = (monitor_width as f64 * 0.20).clamp(360.0, 500.0);
        let total_spacing = TAB_SPACING * (TAG_ICONS.len().saturating_sub(1) as f64);
        ((group_width - total_spacing) / TAG_ICONS.len() as f64).clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH)
    }
}

fn with_alpha(mut c: Color, a: f32) -> Color {
    c.components[3] = a;
    c
}

// -------- Reusable view builders --------------------------------------------

fn flat<V>(inner: V) -> impl WidgetView<XilemBar>
where
    V: WidgetView<XilemBar> + 'static,
{
    sized_box(inner)
        .padding(pill_padding())
        .height(Dim::Stretch)
}

// Catppuccin Mocha (dark) / Latte (light) palettes, swapped at runtime.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum ThemeMode {
    Dark,
    Light,
}

#[derive(Copy, Clone)]
struct Theme {
    bar_bg: Color,
    fg: Color,
    subtle: Color,
    tag_inactive_bg: Color,
    tag_inactive_text: Color,
    tag_active_text: Color,
    urgent_bg: Color,
    urgent_border: Color,
    green: Color,
    yellow: Color,
    orange: Color,
    red: Color,
    teal: Color,
    blue: Color,
    mauve: Color,
    pink: Color,
}

impl Theme {
    const fn mocha() -> Self {
        Self {
            bar_bg: Color::from_rgba8(0x1E, 0x20, 0x32, 128),
            fg: Color::from_rgba8(0xCD, 0xD6, 0xF4, 255),
            subtle: Color::from_rgba8(0x9C, 0xA0, 0xB0, 255),
            tag_inactive_bg: Color::from_rgba8(0x45, 0x47, 0x5A, 217),
            tag_inactive_text: Color::from_rgba8(0xCD, 0xD6, 0xF4, 255),
            tag_active_text: Color::WHITE,
            urgent_bg: Color::from_rgba8(0xDB, 0x36, 0x45, 255),
            urgent_border: Color::from_rgba8(0xBC, 0x21, 0x30, 255),
            green: Color::from_rgba8(0xA6, 0xE3, 0xA1, 255),
            yellow: Color::from_rgba8(0xF9, 0xE2, 0xAF, 255),
            orange: Color::from_rgba8(0xFA, 0xB3, 0x87, 255),
            red: Color::from_rgba8(0xF3, 0x8B, 0xA8, 255),
            teal: Color::from_rgba8(0x94, 0xE2, 0xD5, 255),
            blue: Color::from_rgba8(0x89, 0xB4, 0xFA, 255),
            mauve: Color::from_rgba8(0xCB, 0xA6, 0xF7, 255),
            pink: Color::from_rgba8(0xF5, 0xC2, 0xE7, 255),
        }
    }
    const fn latte() -> Self {
        Self {
            bar_bg: Color::from_rgba8(0xEF, 0xF1, 0xF5, 160),
            fg: Color::from_rgba8(0x4C, 0x4F, 0x69, 255),
            subtle: Color::from_rgba8(0x6C, 0x6F, 0x85, 255),
            tag_inactive_bg: Color::from_rgba8(0xCC, 0xD0, 0xDA, 217),
            tag_inactive_text: Color::from_rgba8(0x4C, 0x4F, 0x69, 255),
            tag_active_text: Color::WHITE,
            urgent_bg: Color::from_rgba8(0xD2, 0x0F, 0x39, 255),
            urgent_border: Color::from_rgba8(0xA8, 0x0B, 0x2E, 255),
            green: Color::from_rgba8(0x40, 0xA0, 0x2B, 255),
            yellow: Color::from_rgba8(0xDF, 0x8E, 0x1D, 255),
            orange: Color::from_rgba8(0xFE, 0x64, 0x0B, 255),
            red: Color::from_rgba8(0xD2, 0x0F, 0x39, 255),
            teal: Color::from_rgba8(0x17, 0x92, 0x99, 255),
            blue: Color::from_rgba8(0x1E, 0x66, 0xF5, 255),
            mauve: Color::from_rgba8(0x88, 0x39, 0xEF, 255),
            pink: Color::from_rgba8(0xEA, 0x76, 0xCB, 255),
        }
    }
    const fn from_mode(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self::mocha(),
            ThemeMode::Light => Self::latte(),
        }
    }
}

fn theme_config_path() -> Option<PathBuf> {
    let home = env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push(".config/xilem_bar/theme");
    Some(p)
}

fn load_theme_mode() -> ThemeMode {
    let Some(p) = theme_config_path() else {
        return ThemeMode::Dark;
    };
    match fs::read_to_string(&p).ok().as_deref().map(str::trim) {
        Some("light") => ThemeMode::Light,
        _ => ThemeMode::Dark,
    }
}

fn save_theme_mode(mode: ThemeMode) {
    let Some(p) = theme_config_path() else {
        return;
    };
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let s = match mode {
        ThemeMode::Dark => "dark",
        ThemeMode::Light => "light",
    };
    if let Err(e) = fs::write(&p, s) {
        warn!("failed to persist theme mode to {}: {e}", p.display());
    }
}

fn usage_color(theme: &Theme, usage: f32) -> Color {
    if usage <= 30.0 {
        theme.green
    } else if usage <= 60.0 {
        theme.yellow
    } else if usage <= 80.0 {
        theme.orange
    } else {
        theme.red
    }
}

fn battery_color(theme: &Theme, pct: f32) -> Color {
    if pct > 50.0 {
        theme.green
    } else if pct > 20.0 {
        theme.yellow
    } else {
        theme.red
    }
}

fn volume_icon(volume: i32, muted: bool, has_device: bool) -> &'static str {
    if !has_device || muted || volume <= 0 {
        ICON_VOL_MUTE
    } else if volume < 34 {
        ICON_VOL_LOW
    } else if volume < 67 {
        ICON_VOL_MID
    } else {
        ICON_VOL_HIGH
    }
}

fn monitor_num_to_icon(n: i32) -> String {
    match n {
        0 => ICON_M0.to_string(),
        1 => ICON_M1.to_string(),
        n => format!("M{}", n),
    }
}

// -------- Sub-views ----------------------------------------------------------

fn workspace_tag(state: &mut XilemBar, index: usize) -> impl WidgetView<XilemBar> + use<> {
    let label_str = TAG_ICONS[index].to_string();
    let (bg, border_w, border_c) = state.tag_visuals(index);
    let is_active = state.is_tag_active(index);
    let tab_width = state.workspace_tab_width();
    let text_color = if is_active {
        state.theme.tag_active_text
    } else {
        state.theme.tag_inactive_text
    };

    let inner = label(label_str).text_size(TAB_FONT_SIZE).color(text_color);
    sized_box(button(inner, move |s: &mut XilemBar| {
        s.active_tab = index;
        s.send_tag_command(true);
    }))
    .dims(Dimensions::new(
        Dim::Fixed(Length::px(tab_width)),
        Dim::Stretch,
    ))
    .background(bg)
    .border(border_c, Length::px(border_w))
    .corner_radius(Length::px(3.0))
}

fn workspace_row(state: &mut XilemBar) -> impl WidgetView<XilemBar> + use<> {
    flex(
        Axis::Horizontal,
        (
            workspace_tag(state, 0),
            workspace_tag(state, 1),
            workspace_tag(state, 2),
            workspace_tag(state, 3),
            workspace_tag(state, 4),
            workspace_tag(state, 5),
            workspace_tag(state, 6),
            workspace_tag(state, 7),
            workspace_tag(state, 8),
        ),
    )
    .cross_axis_alignment(CrossAxisAlignment::Stretch)
    .gap(Length::px(TAB_SPACING))
}

fn layout_toggle(state: &mut XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let open = state.layout_selector_open;
    let fg = if open {
        state.theme.green
    } else {
        state.theme.orange
    };
    let label_str = state.layout_symbol.clone();

    flat(button(
        label(label_str).text_size(PILL_FONT_SIZE).color(fg),
        |s: &mut XilemBar| {
            s.layout_selector_open = !s.layout_selector_open;
        },
    ))
}

fn layout_options(state: &mut XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let current = state.layout_symbol.clone();
    let theme = state.theme;
    let mk = move |sym: &'static str, idx: u32, current: String| {
        let is_current = sym == current;
        let fg = if is_current { theme.green } else { theme.blue };
        flat(button(
            label(sym).text_size(PILL_FONT_SIZE).color(fg),
            move |s: &mut XilemBar| {
                s.send_layout_command(idx);
                s.layout_selector_open = false;
            },
        ))
    };

    flex(
        Axis::Horizontal,
        (
            mk("[]=", 0, current.clone()),
            mk("><>", 1, current.clone()),
            mk("[M]", 2, current),
        ),
    )
    .cross_axis_alignment(CrossAxisAlignment::Stretch)
    .gap(Length::px(4.0))
}

fn usage_pill_view(
    theme: &Theme,
    icon: &'static str,
    value: f32,
) -> impl WidgetView<XilemBar> + use<> {
    let accent = usage_color(theme, value);
    let fg = theme.fg;
    flat(
        flex(
            Axis::Horizontal,
            (
                label(icon.to_string())
                    .text_size(PILL_FONT_SIZE)
                    .color(accent),
                label(format!("{:.0}%", value))
                    .text_size(PILL_FONT_SIZE)
                    .color(fg),
            ),
        )
        .gap(Length::px(3.0)),
    )
}

fn battery_pill_view(state: &XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let (pct, charging) = state
        .system_monitor
        .get_snapshot()
        .map(|s| (s.battery_percent, s.is_charging))
        .unwrap_or((0.0, false));
    let icon = if charging {
        ICON_BAT_CHG
    } else {
        ICON_BAT_FULL
    };
    let accent = battery_color(&state.theme, pct);
    let fg = state.theme.fg;
    flat(
        flex(
            Axis::Horizontal,
            (
                label(icon.to_string())
                    .text_size(PILL_FONT_SIZE)
                    .color(accent),
                label(format!("{:.0}%", pct))
                    .text_size(PILL_FONT_SIZE)
                    .color(fg),
            ),
        )
        .gap(Length::px(3.0)),
    )
}

fn brightness_pill_view(state: &XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let pct = state.brightness_manager.percent();
    let pct_str = match pct {
        Some(p) => format!("{}%", p),
        None => "--".to_string(),
    };
    let accent = state.theme.yellow;
    let fg = state.theme.fg;
    // Left-click brightens (+10), right-click dims (-10).
    let inner = flex(
        Axis::Horizontal,
        (
            label(ICON_BRIGHT.to_string())
                .text_size(PILL_FONT_SIZE)
                .color(accent),
            label(pct_str).text_size(PILL_FONT_SIZE).color(fg),
        ),
    )
    .gap(Length::px(3.0));

    flat(button_any_pointer(
        inner,
        |s: &mut XilemBar, btn: Option<PointerButton>| {
            let delta = if matches!(btn, Some(PointerButton::Secondary)) {
                -10
            } else {
                10
            };
            let _ = s.brightness_manager.adjust(delta);
        },
    ))
}

fn volume_pill_view(state: &XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let master = state.audio_manager.get_master_device();
    let (vol, muted, has_dev) = if let Some(d) = master {
        (d.volume.clamp(0, 100), d.is_muted, true)
    } else {
        (0, true, false)
    };
    let icon = volume_icon(vol, muted, has_dev);
    let pct_str = if has_dev {
        format!("{}%", vol)
    } else {
        "--".to_string()
    };
    let accent = if muted || !has_dev {
        state.theme.subtle
    } else {
        state.theme.teal
    };
    let fg = state.theme.fg;
    let inner = flex(
        Axis::Horizontal,
        (
            label(icon.to_string())
                .text_size(PILL_FONT_SIZE)
                .color(accent),
            label(pct_str).text_size(PILL_FONT_SIZE).color(fg),
        ),
    )
    .gap(Length::px(3.0));
    // Left-click +5, right-click −5, middle-click toggles mute.
    flat(button_any_pointer(
        inner,
        |s: &mut XilemBar, btn: Option<PointerButton>| {
            let Some(d) = s.audio_manager.get_master_device().cloned() else {
                return;
            };
            match btn {
                Some(PointerButton::Secondary) => {
                    let _ = s.audio_manager.adjust_volume(&d.name, -5);
                }
                Some(PointerButton::Auxiliary) => {
                    let _ = s.audio_manager.toggle_mute(&d.name);
                }
                _ => {
                    let _ = s.audio_manager.adjust_volume(&d.name, 5);
                }
            }
        },
    ))
}

fn screenshot_pill_view(state: &XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let inner = label(ICON_SHOT.to_string())
        .text_size(PILL_FONT_SIZE)
        .color(state.theme.pink);
    flat(button(inner, |_s: &mut XilemBar| {
        if let Err(e) = Command::new("flameshot").arg("gui").spawn() {
            warn!("Failed to spawn flameshot: {e}");
        }
    }))
}

fn theme_pill_view(state: &XilemBar) -> impl WidgetView<XilemBar> + use<> {
    // Show the icon for the mode you'll switch TO: sun if currently dark, moon if light.
    let (icon, accent) = match state.theme_mode {
        ThemeMode::Dark => (ICON_SUN, state.theme.yellow),
        ThemeMode::Light => (ICON_MOON, state.theme.mauve),
    };
    flat(button(
        label(icon.to_string())
            .text_size(PILL_FONT_SIZE)
            .color(accent),
        |s: &mut XilemBar| s.toggle_theme(),
    ))
}

fn time_pill_view(state: &XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let accent = state.theme.blue;
    let fg = state.theme.fg;
    let inner = flex(
        Axis::Horizontal,
        (
            label(ICON_TIME.to_string())
                .text_size(PILL_FONT_SIZE)
                .color(accent),
            label(state.formated_now.clone())
                .text_size(PILL_FONT_SIZE)
                .color(fg),
        ),
    )
    .gap(Length::px(3.0));
    flat(button(inner, |s: &mut XilemBar| {
        s.show_seconds = !s.show_seconds;
    }))
}

fn monitor_pill_view(theme: &Theme, monitor_num: i32) -> impl WidgetView<XilemBar> + use<> {
    let accent = theme.mauve;
    let fg = theme.fg;
    flat(
        flex(
            Axis::Horizontal,
            (
                label(ICON_MON.to_string())
                    .text_size(PILL_FONT_SIZE)
                    .color(accent),
                label(monitor_num_to_icon(monitor_num))
                    .text_size(PILL_FONT_SIZE)
                    .color(fg),
            ),
        )
        .gap(Length::px(3.0)),
    )
}

fn scale_pill_view(theme: &Theme, scale: f32) -> impl WidgetView<XilemBar> + use<> {
    flat(flex(
        Axis::Horizontal,
        label(format!("s: {:.2}", scale))
            .text_size(PILL_FONT_SIZE)
            .color(theme.subtle),
    ))
}

// -------- Top-level view -----------------------------------------------------

fn app_logic(state: &mut XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let snapshot = state.system_monitor.get_snapshot();
    let cpu = snapshot.map(|s| s.cpu_average).unwrap_or(0.0);
    let mem = snapshot.map(|s| s.memory_usage_percent).unwrap_or(0.0);

    let monitor_num = state
        .monitor_info_opt
        .as_ref()
        .map(|m| m.monitor_num)
        .unwrap_or(0);

    let theme = state.theme;
    let tags = workspace_row(state);
    let lt_btn = layout_toggle(state);
    let lt_options: Option<_> = if state.layout_selector_open {
        Some(layout_options(state))
    } else {
        None
    };

    flex(
        Axis::Horizontal,
        (
            (
                tags,
                FlexSpacer::Fixed(Length::px(LEFT_SECTION_GAP)),
                lt_btn,
                FlexSpacer::Fixed(Length::px(LEFT_SECTION_GAP)),
                lt_options,
                FlexSpacer::Flex(1.0),
            ),
            (
                usage_pill_view(&theme, ICON_CPU, cpu),
                FlexSpacer::Fixed(Length::px(CENTER_SECTION_GAP)),
                usage_pill_view(&theme, ICON_MEM, mem),
                FlexSpacer::Fixed(Length::px(CENTER_SECTION_GAP)),
                battery_pill_view(state),
                FlexSpacer::Fixed(Length::px(CENTER_SECTION_GAP)),
                brightness_pill_view(state),
                FlexSpacer::Fixed(Length::px(CENTER_SECTION_GAP)),
                volume_pill_view(state),
            ),
            (
                FlexSpacer::Fixed(Length::px(RIGHT_SECTION_GAP)),
                screenshot_pill_view(state),
                FlexSpacer::Fixed(Length::px(RIGHT_SECTION_GAP)),
                theme_pill_view(state),
                FlexSpacer::Fixed(Length::px(RIGHTMOST_SECTION_GAP)),
                time_pill_view(state),
                FlexSpacer::Fixed(Length::px(RIGHTMOST_SECTION_GAP)),
                monitor_pill_view(&theme, monitor_num),
                FlexSpacer::Fixed(Length::px(RIGHTMOST_SECTION_GAP)),
                scale_pill_view(&theme, 1.0),
            ),
        ),
    )
    .cross_axis_alignment(CrossAxisAlignment::Stretch)
    .gap(Length::ZERO)
}

// -------- Background workers -------------------------------------------------

// 1Hz clock + system monitor tick.
fn clock_task() -> impl View<XilemBar, (), ViewCtx, Element = NoElement> + use<> {
    task_raw(
        |proxy: MessageProxy<WorkerEvent>, _state: &mut XilemBar| async move {
            let mut iv = tokio::time::interval(Duration::from_secs(1));
            loop {
                iv.tick().await;
                if proxy.message(WorkerEvent::Tick).is_err() {
                    break;
                }
            }
        },
        |state: &mut XilemBar, ev: WorkerEvent| state.on_worker(ev),
    )
}

// Shared-memory watcher: spawns an OS thread that blocks on the futex, posts
// SharedMessage updates via the message proxy.
fn shared_mem_worker(
    state: &mut XilemBar,
) -> impl View<XilemBar, (), ViewCtx, Element = NoElement> + use<> {
    let buf = state.shared_buffer_rc.clone();
    task_raw(
        move |proxy: MessageProxy<WorkerEvent>, _state: &mut XilemBar| {
            let buf = buf.clone();
            async move {
                std::thread::spawn(move || {
                    let Some(buf) = buf else {
                        let _ =
                            proxy.message(WorkerEvent::SharedError("Empty shared buffer".into()));
                        return;
                    };
                    let stop = Arc::new(AtomicBool::new(false));
                    let mut prev_ts: u128 = 0;
                    while !stop.load(Ordering::Relaxed) {
                        match buf.wait_for_message(Some(Duration::from_secs(2))) {
                            Ok(true) => {
                                if let Ok(Some(msg)) = buf.try_read_latest_message() {
                                    let ts = msg.timestamp as u128;
                                    if prev_ts != ts {
                                        prev_ts = ts;
                                        if proxy
                                            .message(WorkerEvent::Shared(Box::new(msg)))
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                            }
                            Ok(false) => {}
                            Err(e) => {
                                let _ = proxy.message(WorkerEvent::SharedError(format!("{e}")));
                                break;
                            }
                        }
                    }
                });
            }
        },
        |state: &mut XilemBar, ev: WorkerEvent| state.on_worker(ev),
    )
}

// Top-level view: fork attaches the background tasks to the visible tree.
fn root(state: &mut XilemBar) -> impl WidgetView<XilemBar> + use<> {
    let bar_bg = state.theme.bar_bg;
    fork(
        sized_box(app_logic(state))
            .padding(Padding::from_vh(Length::px(0.0), Length::px(6.0)))
            .background(bar_bg),
        (clock_task(), shared_mem_worker(state)),
    )
}

// -------- main ---------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let shared_path = args.iter().skip(1).last().cloned().unwrap_or_default();
    let _ = initialize_logging("xilem_bar", &shared_path);

    let opts = WindowOptions::new("xilem_bar")
        .with_initial_inner_size(LogicalSize::new(800.0, 26.0))
        .with_decorations(false)
        .with_transparent(true)
        .with_window_level(WindowLevel::AlwaysOnTop)
        .with_resizable(false);

    let _ = NERD_FONT;
    Xilem::new_simple(XilemBar::new(), root, opts)
        .with_default_base_color(Color::TRANSPARENT)
        .run_in(EventLoop::with_user_event())?;
    Ok(())
}
