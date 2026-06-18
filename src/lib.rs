// bar_core/src/lib.rs
// 单文件版核心库：UI 颜色/形状/Pango 文本、AppState、绘制、timerfd、eventfd、日志
// 依赖：anyhow, cairo-rs(xcb), pango, pangocairo, flexi_logger, log, libc, chrono, shared_structures

use anyhow::Result;
use cairo::{Context, LinearGradient};
use chrono::Local;
use libc;
use log::{debug, error, info, warn};
use pango::FontDescription;
use pangocairo::functions::{create_layout, show_layout};
use shared_structures::{CommandType, MonitorInfo, SharedCommand, SharedMessage, SharedRingBuffer};
use std::sync::Arc;
use std::time::Instant;

use std::f64::consts::{FRAC_PI_2, PI};

pub mod audio_manager;
pub mod battery;
pub mod brightness;
pub mod system_monitor;
pub use cairo;
pub use pango;
pub use pangocairo;
pub use audio_manager::AudioManager;
pub use battery::BatteryManager;
pub use brightness::BrightnessManager;
pub use system_monitor::SystemMonitor;

// ================= Dirty Region Tracking =================

/// Bitmask to track which UI regions have changed since last redraw
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyBits(u32);

impl DirtyBits {
    pub const NONE: u32 = 0;
    pub const TIME_CHANGED: u32 = 1 << 0;        // Clock/time display changed
    pub const HOVER_CHANGED: u32 = 1 << 1;       // Hover state changed
    pub const MONITOR_CHANGED: u32 = 1 << 2;     // Tag/window manager state changed
    pub const AUDIO_CHANGED: u32 = 1 << 3;       // Volume/audio changed
    pub const SYSTEM_CHANGED: u32 = 1 << 4;      // CPU/memory stats changed
    pub const LAYOUT_CHANGED: u32 = 1 << 5;      // Layout symbol changed
    pub const THEME_CHANGED: u32 = 1 << 6;       // Dark/Light mode changed
    pub const BRIGHTNESS_CHANGED: u32 = 1 << 7;  // Backlight brightness changed
    pub const BATTERY_CHANGED: u32 = 1 << 8;     // Battery capacity/status changed

    pub fn new(bits: u32) -> Self {
        DirtyBits(bits)
    }

    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    pub fn set(&mut self, flag: u32) {
        self.0 |= flag;
    }

    pub fn contains(&self, flag: u32) -> bool {
        (self.0 & flag) != 0
    }

    pub fn clear(&mut self) {
        self.0 = 0;
    }

    pub fn all() -> Self {
        DirtyBits(!0u32)
    }
}

// ================= 公共类型 =================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug)]
pub struct Color {
    pub r: f64,
    pub g: f64,
    pub b: f64,
}
impl Color {
    pub fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self {
            r: r as f64 / 255.0,
            g: g as f64 / 255.0,
            b: b as f64 / 255.0,
        }
    }

    // 轻量化 hover 需要的辅助方法：提亮 / 变暗
    pub fn lighten(&self, amount: f64) -> Self {
        let a = amount.clamp(0.0, 1.0);
        Self {
            r: (self.r + (1.0 - self.r) * a).clamp(0.0, 1.0),
            g: (self.g + (1.0 - self.g) * a).clamp(0.0, 1.0),
            b: (self.b + (1.0 - self.b) * a).clamp(0.0, 1.0),
        }
    }
    pub fn darken(&self, amount: f64) -> Self {
        let a = amount.clamp(0.0, 1.0);
        Self {
            r: (self.r * (1.0 - a)).clamp(0.0, 1.0),
            g: (self.g * (1.0 - a)).clamp(0.0, 1.0),
            b: (self.b * (1.0 - a)).clamp(0.0, 1.0),
        }
    }

    pub fn mix(&self, other: Color, t: f64) -> Self {
        let t = t.clamp(0.0, 1.0);
        Self {
            r: (self.r * (1.0 - t) + other.r * t).clamp(0.0, 1.0),
            g: (self.g * (1.0 - t) + other.g * t).clamp(0.0, 1.0),
            b: (self.b * (1.0 - t) + other.b * t).clamp(0.0, 1.0),
        }
    }
}

#[derive(Clone)]
pub struct Colors {
    pub bg: Color,
    pub text: Color,
    pub white: Color,
    pub black: Color,
    pub tag_colors: [Color; 9],
    pub gray: Color,
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub orange: Color,
    pub blue: Color,
    pub purple: Color,
    pub teal: Color,
    pub time: Color,
    pub accent: Color,
    pub accent_light: Color,
    pub dim: Color,
}

pub fn default_colors() -> Colors {
    Colors {
        bg: Color::rgb(17, 17, 17),
        text: Color::rgb(255, 255, 255),
        white: Color::rgb(255, 255, 255),
        black: Color::rgb(0, 0, 0),
        tag_colors: [
            Color::rgb(255, 107, 107), // red
            Color::rgb(78, 205, 196),  // cyan
            Color::rgb(69, 183, 209),  // blue
            Color::rgb(150, 206, 180), // green
            Color::rgb(254, 202, 87),  // yellow
            Color::rgb(255, 159, 243), // pink
            Color::rgb(84, 160, 255),  // light blue
            Color::rgb(95, 39, 205),   // purple
            Color::rgb(0, 210, 211),   // teal
        ],
        gray: Color::rgb(90, 90, 90),
        red: Color::rgb(230, 60, 60),
        green: Color::rgb(36, 179, 112),
        yellow: Color::rgb(240, 200, 40),
        orange: Color::rgb(255, 140, 0),
        blue: Color::rgb(50, 120, 220),
        purple: Color::rgb(150, 110, 210),
        teal: Color::rgb(0, 180, 180),
        time: Color::rgb(80, 150, 220),
        accent: Color::rgb(8, 145, 178),
        accent_light: Color::rgb(34, 211, 238),
        dim: Color::rgb(81, 90, 104),
    }
}

pub fn light_colors() -> Colors {
    Colors {
        bg: Color::rgb(245, 245, 245),
        text: Color::rgb(20, 20, 20),
        white: Color::rgb(255, 255, 255),
        black: Color::rgb(0, 0, 0),
        tag_colors: default_colors().tag_colors,
        gray: Color::rgb(130, 130, 130),
        red: Color::rgb(220, 40, 40),
        green: Color::rgb(30, 160, 100),
        yellow: Color::rgb(240, 190, 40),
        orange: Color::rgb(245, 135, 20),
        blue: Color::rgb(40, 110, 220),
        purple: Color::rgb(140, 90, 210),
        teal: Color::rgb(0, 160, 160),
        time: Color::rgb(50, 120, 210),
        accent: Color::rgb(59, 130, 246),
        accent_light: Color::rgb(96, 165, 250),
        dim: Color::rgb(100, 116, 139),
    }
}

pub fn colors_for_theme(mode: ThemeMode) -> Colors {
    match mode {
        ThemeMode::Dark => default_colors(),
        ThemeMode::Light => light_colors(),
    }
}

/// A slightly refined palette used by the bar frontends for a more
/// "desktop-polished" look (softer bg, higher-contrast text).
pub fn tuned_colors_for_theme(mode: ThemeMode) -> Colors {
    let mut c = colors_for_theme(mode);
    match mode {
        ThemeMode::Dark => {
            c.bg = Color::rgb(13, 16, 23);
            c.text = Color::rgb(235, 238, 245);
            c.gray = Color::rgb(90, 96, 110);
            c.time = Color::rgb(86, 156, 214);
        }
        ThemeMode::Light => {
            c.bg = Color::rgb(246, 247, 250);
            c.text = Color::rgb(22, 24, 28);
            c.gray = Color::rgb(120, 128, 145);
            c.time = Color::rgb(60, 120, 210);
        }
    }
    c
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub x: i16,
    pub y: i16,
    pub w: u16,
    pub h: u16,
}
impl Rect {
    pub fn contains(&self, px: i16, py: i16) -> bool {
        px >= self.x
            && py >= self.y
            && (px as i32) < (self.x as i32 + self.w as i32)
            && (py as i32) < (self.y as i32 + self.h as i32)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ShapeStyle {
    Chamfer,
    Pill,
}

#[derive(Clone, Debug)]
pub struct BarConfig {
    pub bar_height: u16,
    pub padding_x: f64,
    pub padding_y: f64,
    pub tag_spacing: f64,
    pub pill_hpadding: f64,
    pub pill_radius: f64,
    pub shape_style: ShapeStyle,
    pub time_icon: &'static str,
    pub screenshot_label: &'static str,

    // Emoji labels
    pub tag_labels: [&'static str; 9],
    pub theme_dark_label: &'static str,
    pub theme_light_label: &'static str,
    pub monitor_labels: [&'static str; 4], // M0, M1, M2, fallback
    pub volume_label: &'static str,
    pub mute_label: &'static str,
    pub brightness_label: &'static str,
    pub battery_label: &'static str,
    pub battery_charging_label: &'static str,
    pub cpu_label: &'static str,
    pub mem_label: &'static str,

    // 可选组件
    pub show_audio: bool,
    pub show_theme_toggle: bool,
    pub show_brightness: bool,
    pub show_battery: bool,
    pub volume_step: i32,
    pub brightness_step: i32,
}
impl Default for BarConfig {
    fn default() -> Self {
        Self {
            bar_height: 40,
            padding_x: 8.0,
            padding_y: 4.0,
            tag_spacing: 6.0,
            pill_hpadding: 10.0,
            pill_radius: 6.0,
            shape_style: ShapeStyle::Pill,
            time_icon: "TIME",
            screenshot_label: "SHOT",

            tag_labels: ["1", "2", "3", "4", "5", "6", "7", "8", "9"],
            theme_dark_label: "DARK",
            theme_light_label: "LIGHT",
            monitor_labels: ["M0", "M1", "M2", "M?"],
            volume_label: "VOL",
            mute_label: "MUTE",
            brightness_label: "BRT",
            battery_label: "BAT",
            battery_charging_label: "CHG",
            cpu_label: "CPU",
            mem_label: "MEM",

            show_audio: false,
            show_theme_toggle: false,
            show_brightness: false,
            show_battery: false,
            volume_step: 5,
            brightness_step: 5,
        }
    }
}

// ================= AppState 与业务逻辑 =================

pub struct AppState {
    pub shared_buffer: Option<Arc<SharedRingBuffer>>,
    pub monitor_info: Option<MonitorInfo>,
    pub monitor_num: i32,
    pub layout_symbol: String,

    pub tag_rects: [Rect; 9],
    pub active_tab: usize,

    pub layout_button_rect: Rect,
    pub layout_selector_open: bool,
    pub layout_option_rects: [Rect; 3],

    pub ss_rect: Rect,
    pub time_rect: Rect,
    pub is_ss_hover: bool,
    pub show_seconds: bool,

    pub audio_rect: Rect,
    pub theme_rect: Rect,
    pub brightness_rect: Rect,
    pub battery_rect: Rect,
    pub theme_mode: ThemeMode,

    // Hover 状态
    pub hover_target: HoverTarget,

    // hover 判定区域
    pub mem_rect: Rect,
    pub cpu_rect: Rect,
    pub mon_rect: Rect,

    pub audio_manager: AudioManager,
    pub system_monitor: SystemMonitor,
    pub brightness_manager: BrightnessManager,
    pub battery_manager: BatteryManager,

    /// Relative step (percentage) applied on a brightness click/scroll.
    pub brightness_step: i32,

    pub last_time_string: String,
    pub last_monitor_update: Instant,

    pub shape_style: ShapeStyle,

    // Dirty region tracking for selective redraw
    pub dirty_fields: DirtyBits,
}

// 排他式 hover 的命中目标
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoverTarget {
    None,
    Tag(usize),
    LayoutOption(usize),
    LayoutButton,
    Screenshot,
    Time,
    Audio,
    Theme,
    Brightness,
    Battery,
    Mem,
    Cpu,
    Monitor,
}

impl AppState {
    pub fn new(shared_buffer: Option<Arc<SharedRingBuffer>>) -> Self {
        Self {
            shared_buffer,
            monitor_info: None,
            monitor_num: 0,
            layout_symbol: "[]=".to_string(),
            tag_rects: [Rect::default(); 9],
            active_tab: 0,

            layout_button_rect: Rect::default(),
            layout_selector_open: false,
            layout_option_rects: [Rect::default(), Rect::default(), Rect::default()],

            ss_rect: Rect::default(),
            time_rect: Rect::default(),
            is_ss_hover: false,
            show_seconds: false,

            audio_rect: Rect::default(),
            theme_rect: Rect::default(),
            brightness_rect: Rect::default(),
            battery_rect: Rect::default(),
            theme_mode: ThemeMode::Dark,

            hover_target: HoverTarget::None,

            mem_rect: Rect::default(),
            cpu_rect: Rect::default(),
            mon_rect: Rect::default(),

            audio_manager: AudioManager::new(),
            system_monitor: SystemMonitor::new(5),
            brightness_manager: BrightnessManager::new(),
            battery_manager: BatteryManager::new(),

            brightness_step: 5,

            last_time_string: String::new(),
            last_monitor_update: Instant::now(),

            shape_style: ShapeStyle::Pill,

            // Initialize with all dirty to force full redraw on first draw
            dirty_fields: DirtyBits::all(),
        }
    }
    pub fn monitor_num_to_label(num: i32) -> String {
        format!("M{}", num)
    }
    pub fn monitor_label<'a>(&self, cfg: &'a BarConfig) -> &'a str {
        let idx = self.monitor_num as usize;
        if idx < cfg.monitor_labels.len() - 1 {
            cfg.monitor_labels[idx]
        } else {
            cfg.monitor_labels[cfg.monitor_labels.len() - 1] // fallback
        }
    }
    pub fn update_from_shared(&mut self, msg: SharedMessage) {
        debug!("SharedMemoryUpdated: {:?}", msg.timestamp);
        let old_info = self.monitor_info.clone();
        self.monitor_info = Some(msg.monitor_info);
        if let Some(mi) = self.monitor_info.as_ref() {
            // Mark monitor state changed if info differs
            if old_info.as_ref() != Some(mi) {
                self.dirty_fields.set(DirtyBits::MONITOR_CHANGED);
            }
            let new_symbol = mi.get_ltsymbol();
            if new_symbol != self.layout_symbol {
                self.dirty_fields.set(DirtyBits::LAYOUT_CHANGED);
                self.layout_symbol = new_symbol;
            } else {
                self.layout_symbol = new_symbol;
            }
            self.monitor_num = mi.monitor_num;
            for (i, tag) in mi.tag_status_vec.iter().enumerate() {
                if tag.is_selected {
                    self.active_tab = i;
                }
            }
        }
    }
    pub fn send_tag_command_index(&mut self, idx: usize, is_view: bool) {
        let tag_bit = 1 << idx;
        let cmd = if is_view {
            SharedCommand::view_tag(tag_bit, self.monitor_num)
        } else {
            SharedCommand::toggle_tag(tag_bit, self.monitor_num)
        };
        if let Some(buf) = &self.shared_buffer {
            match buf.send_command(cmd) {
                Ok(true) => info!("Sent command: {:?} by shared_buffer", cmd),
                Ok(false) => warn!("Command buffer full, command dropped"),
                Err(e) => error!("Failed to send command: {}", e),
            }
        }
    }
    pub fn send_layout_command(&mut self, layout_index: u32) {
        let cmd = SharedCommand::new(CommandType::SetLayout, layout_index, self.monitor_num);
        if let Some(buf) = &self.shared_buffer {
            match buf.send_command(cmd) {
                Ok(true) => info!("Sent command: {:?} by shared_buffer", cmd),
                Ok(false) => warn!("Command buffer full, command dropped"),
                Err(e) => error!("Failed to send command: {}", e),
            }
        }
    }
    pub fn format_time(&self) -> String {
        let now = Local::now();
        if self.show_seconds {
            now.format("%Y-%m-%d %H:%M:%S").to_string()
        } else {
            now.format("%Y-%m-%d %H:%M").to_string()
        }
    }
    pub fn handle_buttons(&mut self, px: i16, py: i16, button: u8) -> bool {
        let mut need_redraw = false;
        // 左侧 tag：左键 view，右键 toggle
        for (i, rect) in self.tag_rects.iter().enumerate() {
            if rect.contains(px, py) {
                if button == 1 {
                    self.active_tab = i;
                    self.send_tag_command_index(i, true);
                } else if button == 3 {
                    self.send_tag_command_index(i, false);
                }
                self.dirty_fields.set(DirtyBits::MONITOR_CHANGED);
                need_redraw = true;
                break;
            }
        }
        // 布局按钮
        if self.layout_button_rect.contains(px, py) && button == 1 {
            self.layout_selector_open = !self.layout_selector_open;
            self.dirty_fields.set(DirtyBits::LAYOUT_CHANGED);
            need_redraw = true;
        }
        // 布局选项
        for (idx, r) in self.layout_option_rects.iter().enumerate() {
            if r.w > 0 && r.contains(px, py) && button == 1 {
                self.send_layout_command(idx as u32);
                self.layout_selector_open = false;
                self.dirty_fields.set(DirtyBits::LAYOUT_CHANGED);
                need_redraw = true;
                break;
            }
        }
        // 截图
        if self.ss_rect.contains(px, py) && button == 1 {
            if let Err(e) = std::process::Command::new("flameshot").arg("gui").spawn() {
                warn!("Failed to spawn flameshot: {e}");
            }
        }

        // 主题切换
        if self.theme_rect.w > 0 && self.theme_rect.contains(px, py) && button == 1 {
            let old_theme = self.theme_mode;
            self.theme_mode = match self.theme_mode {
                ThemeMode::Dark => ThemeMode::Light,
                ThemeMode::Light => ThemeMode::Dark,
            };
            if old_theme != self.theme_mode {
                self.dirty_fields.set(DirtyBits::THEME_CHANGED);
            }
            need_redraw = true;
        }

        // 音量：左键静音/取消静音；滚轮调节
        if self.audio_rect.w > 0 && self.audio_rect.contains(px, py) {
            if let Some(dev) = self.audio_manager.get_master_device().cloned() {
                match button {
                    1 => {
                        let _ = self.audio_manager.toggle_mute(&dev.name);
                        self.dirty_fields.set(DirtyBits::AUDIO_CHANGED);
                        need_redraw = true;
                    }
                    4 => {
                        let _ = self.audio_manager.adjust_volume(&dev.name, 5);
                        self.dirty_fields.set(DirtyBits::AUDIO_CHANGED);
                        need_redraw = true;
                    }
                    5 => {
                        let _ = self.audio_manager.adjust_volume(&dev.name, -5);
                        self.dirty_fields.set(DirtyBits::AUDIO_CHANGED);
                        need_redraw = true;
                    }
                    3 => {
                        // 右键尝试打开音量控制面板（可选）
                        let _ = std::process::Command::new("pavucontrol").spawn();
                    }
                    _ => {}
                }
            }
        }
        // 亮度：左键提高一档，右键降低一档，滚轮上/下调节
        if self.brightness_rect.w > 0 && self.brightness_rect.contains(px, py) {
            let step = self.brightness_step;
            let delta = match button {
                1 | 4 => step,
                3 | 5 => -step,
                _ => 0,
            };
            if delta != 0 {
                self.brightness_manager.adjust(delta);
                self.dirty_fields.set(DirtyBits::BRIGHTNESS_CHANGED);
                need_redraw = true;
            }
        }
        // 电量：左键强制刷新读数
        if self.battery_rect.w > 0 && self.battery_rect.contains(px, py) && button == 1 {
            if self.battery_manager.refresh() {
                self.dirty_fields.set(DirtyBits::BATTERY_CHANGED);
                need_redraw = true;
            }
        }
        // 时间 pill 切换秒显示
        if self.time_rect.contains(px, py) && button == 1 {
            self.show_seconds = !self.show_seconds;
            self.dirty_fields.set(DirtyBits::TIME_CHANGED);
            need_redraw = true;
        }
        need_redraw
    }

    // 命中测试：排他式，按优先级选1个
    fn hit_test(&self, px: i16, py: i16) -> HoverTarget {
        // 1) 布局选项（仅在打开时）
        if self.layout_selector_open {
            for (i, r) in self.layout_option_rects.iter().enumerate() {
                if r.w > 0 && r.contains(px, py) {
                    return HoverTarget::LayoutOption(i);
                }
            }
        }
        // 2) 布局按钮
        if self.layout_button_rect.contains(px, py) {
            return HoverTarget::LayoutButton;
        }
        // 3) 右侧 pills（按你喜欢的优先级；这里时间优先）
        if self.time_rect.contains(px, py) {
            return HoverTarget::Time;
        }
        if self.ss_rect.contains(px, py) {
            return HoverTarget::Screenshot;
        }
        if self.audio_rect.w > 0 && self.audio_rect.contains(px, py) {
            return HoverTarget::Audio;
        }
        if self.theme_rect.w > 0 && self.theme_rect.contains(px, py) {
            return HoverTarget::Theme;
        }
        if self.brightness_rect.w > 0 && self.brightness_rect.contains(px, py) {
            return HoverTarget::Brightness;
        }
        if self.battery_rect.w > 0 && self.battery_rect.contains(px, py) {
            return HoverTarget::Battery;
        }
        if self.mem_rect.contains(px, py) {
            return HoverTarget::Mem;
        }
        if self.cpu_rect.contains(px, py) {
            return HoverTarget::Cpu;
        }
        if self.mon_rect.contains(px, py) {
            return HoverTarget::Monitor;
        }
        // 4) 左侧 tags（只取第一个命中的）
        for (i, rect) in self.tag_rects.iter().enumerate() {
            if rect.contains(px, py) {
                return HoverTarget::Tag(i);
            }
        }
        HoverTarget::None
    }

    // 鼠标移动：更新 hover 状态。返回是否需要重绘（排他式）
    pub fn update_hover(&mut self, px: i16, py: i16) -> bool {
        let prev = self.hover_target;
        self.hover_target = self.hit_test(px, py);
        if prev != self.hover_target {
            self.dirty_fields.set(DirtyBits::HOVER_CHANGED);
            true
        } else {
            false
        }
    }

    // 鼠标离开：清空 hover 状态。返回是否需要重绘
    pub fn clear_hover(&mut self) -> bool {
        let changed = self.hover_target != HoverTarget::None;
        if changed {
            self.dirty_fields.set(DirtyBits::HOVER_CHANGED);
        }
        self.hover_target = HoverTarget::None;
        changed
    }
}

// ================= 绘制相关：Pango 文字与形状 =================

fn pango_text_size(layout: &pango::Layout, text: &str) -> (i32, i32) {
    layout.set_text(text);
    layout.pixel_size()
}
fn pango_draw_text_centered(
    cr: &Context,
    layout: &pango::Layout,
    color: Color,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    text: &str,
) {
    layout.set_text(text);
    let (tw, th) = layout.pixel_size();
    let tx = x + (w - tw as f64) / 2.0;
    let ty = y + (h - th as f64) / 2.0 - 1.0;
    cr.set_source_rgb(color.r, color.g, color.b);
    cr.move_to(tx, ty);
    show_layout(cr, layout);
}
fn pango_draw_text_left(
    cr: &Context,
    layout: &pango::Layout,
    color: Color,
    x: f64,
    y: f64,
    text: &str,
) {
    layout.set_text(text);
    cr.set_source_rgb(color.r, color.g, color.b);
    cr.move_to(x, y);
    show_layout(cr, layout);
}

fn cairo_path_round_rect(cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    cr.new_path();
    cr.move_to(x + r, y);
    cr.line_to(x + w - r, y);
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.line_to(x + w, y + h - r);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.line_to(x + r, y + h);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.line_to(x, y + r);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}
fn fill_round(cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64, color: Color) -> Result<()> {
    cairo_path_round_rect(cr, x, y, w, h, r);
    cr.set_source_rgb(color.r, color.g, color.b);
    cr.fill()
        .map_err(|e| anyhow::anyhow!("cairo fill failed: {:?}", e))
}

fn clip_shape(cr: &Context, style: ShapeStyle, x: f64, y: f64, w: f64, h: f64, k: f64) {
    match style {
        ShapeStyle::Chamfer => cairo_path_chamfer(cr, x, y, w, h, k),
        ShapeStyle::Pill => {
            let r = k.min(h / 2.0).floor();
            cairo_path_round_rect(cr, x, y, w, h, r);
        }
    }
    cr.clip();
}

fn overlay_top_highlight(
    cr: &Context,
    style: ShapeStyle,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    k: f64,
    base: Color,
) -> Result<()> {
    // 顶部高光：更柔和的拟物质感 (relm_bar inspired)
    let top = base.lighten(0.12);
    let mid = base.lighten(0.04);
    let bottom = base.darken(0.02);

    cr.save()?;
    clip_shape(cr, style, x, y, w, h, k);

    let grad = LinearGradient::new(0.0, y, 0.0, y + h);
    grad.add_color_stop_rgb(0.0, top.r, top.g, top.b);
    grad.add_color_stop_rgb(0.45, mid.r, mid.g, mid.b);
    grad.add_color_stop_rgb(1.0, bottom.r, bottom.g, bottom.b);

    cr.set_source(&grad)?;
    cr.rectangle(x, y, w, h);
    cr.fill()?;

    // 额外的高光带（更柔和）
    let band_h = (h * 0.30).max(5.0);
    let b_top = base.lighten(0.15);
    let b_mid = base.lighten(0.06);
    let grad2 = LinearGradient::new(0.0, y, 0.0, y + band_h);
    grad2.add_color_stop_rgb(0.0, b_top.r, b_top.g, b_top.b);
    grad2.add_color_stop_rgb(1.0, b_mid.r, b_mid.g, b_mid.b);
    cr.set_source(&grad2)?;
    cr.rectangle(x, y, w, band_h);
    cr.fill()?;

    cr.restore()?;
    Ok(())
}

fn pill_border_color(fill: Color, is_light_theme: bool) -> Color {
    if is_light_theme {
        fill.darken(0.14)
    } else {
        fill.darken(0.20)
    }
}

fn draw_soft_shadow(
    cr: &Context,
    style: ShapeStyle,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    k: f64,
    bg: Color,
    is_light_theme: bool,
) -> Result<()> {
    // 软阴影：more subtle, relm_bar inspired
    let shadow = if is_light_theme {
        bg.darken(0.06)
    } else {
        bg.darken(0.30)
    };
    let dx = 0.5;
    let dy = 0.8;

    match style {
        ShapeStyle::Chamfer => fill_chamfer(cr, x + dx, y + dy, w, h, k, shadow)?,
        ShapeStyle::Pill => {
            let r = k.min(h / 2.0).floor();
            fill_round(cr, x + dx, y + dy, w, h, r, shadow)?
        }
    }
    Ok(())
}

fn stroke_round_with_fill(
    cr: &Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    r: f64,
    border_w: f64,
    border_color: Color,
    fill_color: Option<Color>,
) -> Result<()> {
    if border_w <= 0.0 {
        if let Some(fill) = fill_color {
            fill_round(cr, x, y, w, h, r, fill)?;
        }
        return Ok(());
    }
    // 外边框
    fill_round(cr, x, y, w, h, r, border_color)?;
    // 内填充
    if let Some(fill) = fill_color {
        let x2 = x + border_w;
        let y2 = y + border_w;
        let w2 = (w - 2.0 * border_w).max(0.0);
        let h2 = (h - 2.0 * border_w).max(0.0);
        if w2 > 0.0 && h2 > 0.0 {
            let r2 = (r - border_w).max(0.0);
            fill_round(cr, x2, y2, w2, h2, r2, fill)?;
            let _ = overlay_top_highlight(cr, ShapeStyle::Pill, x2, y2, w2, h2, r2, fill);
        }
    }
    Ok(())
}

fn cairo_path_chamfer(cr: &Context, x: f64, y: f64, w: f64, h: f64, k: f64) {
    let k = k.min(w / 2.0).min(h / 2.0).max(0.0);
    cr.new_path();
    cr.move_to(x + k, y);
    cr.line_to(x + w - k, y);
    cr.line_to(x + w, y + k);
    cr.line_to(x + w, y + h - k);
    cr.line_to(x + w - k, y + h);
    cr.line_to(x + k, y + h);
    cr.line_to(x, y + h - k);
    cr.line_to(x, y + k);
    cr.close_path();
}
fn fill_chamfer(cr: &Context, x: f64, y: f64, w: f64, h: f64, k: f64, color: Color) -> Result<()> {
    cairo_path_chamfer(cr, x, y, w, h, k);
    cr.set_source_rgb(color.r, color.g, color.b);
    cr.fill()
        .map_err(|e| anyhow::anyhow!("cairo fill failed: {:?}", e))
}
fn stroke_chamfer_with_fill(
    cr: &Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    k: f64,
    border_w: f64,
    border_color: Color,
    fill_color: Option<Color>,
) -> Result<()> {
    if border_w <= 0.0 {
        if let Some(fill) = fill_color {
            fill_chamfer(cr, x, y, w, h, k, fill)?;
        }
        return Ok(());
    }
    // 外边框
    fill_chamfer(cr, x, y, w, h, k, border_color)?;
    // 内部填充
    if let Some(fill) = fill_color {
        let x2 = x + border_w;
        let y2 = y + border_w;
        let w2 = (w - 2.0 * border_w).max(0.0);
        let h2 = (h - 2.0 * border_w).max(0.0);
        if w2 > 0.0 && h2 > 0.0 {
            let k2 = (k - border_w).max(0.0);
            fill_chamfer(cr, x2, y2, w2, h2, k2, fill)?;
            let _ = overlay_top_highlight(cr, ShapeStyle::Chamfer, x2, y2, w2, h2, k2, fill);
        }
    }
    Ok(())
}

fn set_source_color_with_alpha(cr: &Context, color: Color, alpha: f64) {
    cr.set_source_rgba(color.r, color.g, color.b, alpha.clamp(0.0, 1.0));
}

fn paint_bar_background(
    cr: &Context,
    width: u16,
    height: u16,
    bg: Color,
    is_light: bool,
    background_opacity: f64,
) -> Result<()> {
    let background_opacity = background_opacity.clamp(0.0, 1.0);
    let w = width as f64;
    let h = height as f64;
    // relm_bar inspired: subtler gradient
    let top = if is_light { bg.darken(0.02) } else { bg.lighten(0.03) };
    let bottom = if is_light { bg.lighten(0.01) } else { bg.darken(0.03) };

    let grad = LinearGradient::new(0.0, 0.0, 0.0, h);
    grad.add_color_stop_rgba(0.0, top.r, top.g, top.b, background_opacity);
    grad.add_color_stop_rgba(1.0, bottom.r, bottom.g, bottom.b, background_opacity);
    cr.set_source(&grad)?;
    cr.rectangle(0.0, 0.0, w, h);
    cr.fill()?;

    // 顶部高光线 + 底部阴影线（更柔和）
    let top_line = if is_light { bg.lighten(0.12) } else { bg.lighten(0.06) };
    let bottom_line = if is_light { bg.darken(0.06) } else { bg.darken(0.15) };
    set_source_color_with_alpha(cr, top_line, background_opacity);
    cr.rectangle(0.0, 0.0, w, 1.0);
    cr.fill()?;
    set_source_color_with_alpha(cr, bottom_line, background_opacity);
    cr.rectangle(0.0, h - 1.0, w, 1.0);
    cr.fill()?;

    // 外框（极轻微）
    let frame = if is_light { bg.darken(0.05) } else { bg.darken(0.10) };
    set_source_color_with_alpha(cr, frame, background_opacity);
    cr.set_line_width(1.0);
    cr.rectangle(0.5, 0.5, (w - 1.0).max(0.0), (h - 1.0).max(0.0));
    let _ = cr.stroke();

    Ok(())
}

fn stroke_shape_with_fill(
    cr: &Context,
    style: ShapeStyle,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    k: f64,
    border_w: f64,
    border_color: Color,
    fill_color: Option<Color>,
) -> Result<()> {
    match style {
        ShapeStyle::Chamfer => {
            stroke_chamfer_with_fill(cr, x, y, w, h, k, border_w, border_color, fill_color)
        }
        ShapeStyle::Pill => {
            let r = k.min(h / 2.0).floor();
            stroke_round_with_fill(cr, x, y, w, h, r, border_w, border_color, fill_color)
        }
    }
}

// 使用率配色 — relm_bar inspired muted metric colors
fn usage_bg_color(_colors: &Colors, usage: f32, is_light_theme: bool) -> Color {
    let u = usage.clamp(0.0, 100.0);
    if is_light_theme {
        if u <= 30.0 {
            Color::rgb(230, 247, 234)
        } else if u <= 60.0 {
            Color::rgb(255, 248, 230)
        } else if u <= 80.0 {
            Color::rgb(255, 242, 232)
        } else {
            Color::rgb(255, 234, 234)
        }
    } else {
        // dark theme: pre-composited on (13,16,23)
        if u <= 30.0 {
            Color::rgb(17, 49, 36)
        } else if u <= 60.0 {
            Color::rgb(55, 42, 21)
        } else if u <= 80.0 {
            Color::rgb(55, 34, 23)
        } else {
            Color::rgb(58, 26, 32)
        }
    }
}
fn usage_text_color(_colors: &Colors, usage: f32, is_light_theme: bool) -> Color {
    let u = usage.clamp(0.0, 100.0);
    if is_light_theme {
        if u <= 30.0 {
            Color::rgb(24, 121, 78)
        } else if u <= 60.0 {
            Color::rgb(138, 93, 0)
        } else if u <= 80.0 {
            Color::rgb(159, 58, 13)
        } else {
            Color::rgb(168, 7, 26)
        }
    } else {
        if u <= 30.0 {
            Color::rgb(209, 243, 218)
        } else if u <= 60.0 {
            Color::rgb(242, 235, 226)
        } else if u <= 80.0 {
            Color::rgb(242, 226, 204)
        } else {
            Color::rgb(241, 216, 216)
        }
    }
}

fn tag_visuals(
    colors: &Colors,
    mi: Option<&MonitorInfo>,
    idx: usize,
    is_light_theme: bool,
) -> (Color, f64, Color, Color, bool) {
    if let Some(monitor) = mi {
        if let Some(status) = monitor.tag_status_vec.get(idx) {
            if status.is_urg {
                return (colors.red, 2.0, colors.red, colors.white, true);
            } else if status.is_selected {
                if is_light_theme {
                    // relm_bar light: blue accent selected
                    return (
                        Color::rgb(59, 130, 246),
                        2.0,
                        Color::rgb(59, 130, 246),
                        colors.white,
                        true,
                    );
                } else {
                    // relm_bar dark: teal selected (pre-composited)
                    return (
                        Color::rgb(10, 106, 132),
                        2.0,
                        Color::rgb(25, 123, 141),
                        colors.white,
                        true,
                    );
                }
            } else if status.is_filled {
                if is_light_theme {
                    return (
                        Color::rgb(37, 99, 235),
                        1.0,
                        Color::rgb(37, 99, 235),
                        colors.white,
                        true,
                    );
                } else {
                    return (
                        Color::rgb(9, 126, 155),
                        1.0,
                        Color::rgb(20, 140, 160),
                        colors.white,
                        true,
                    );
                }
            } else if status.is_occ {
                if is_light_theme {
                    return (
                        Color::rgb(224, 242, 254),
                        1.0,
                        Color::rgb(147, 197, 253),
                        Color::rgb(30, 64, 175),
                        true,
                    );
                } else {
                    return (
                        Color::rgb(17, 44, 57),
                        1.0,
                        Color::rgb(18, 59, 70),
                        Color::rgb(207, 224, 236),
                        true,
                    );
                }
            }
        }
    }
    // Empty state
    if is_light_theme {
        (
            Color::rgb(241, 245, 249),
            1.0,
            Color::rgb(203, 213, 225),
            colors.dim,
            true,
        )
    } else {
        (
            Color::rgb(14, 19, 31),
            1.0,
            Color::rgb(27, 31, 39),
            colors.dim,
            true,
        )
    }
}

// ================= Region draw functions =================

/// Draw left-side tag pills. Returns the x position after the last tag.
pub fn draw_left_tags(
    cr: &Context,
    cfg: &BarConfig,
    state: &mut AppState,
    layout: &pango::Layout,
    pill_h: f64,
    is_light_theme: bool,
    colors: &Colors,
) -> Result<f64> {
    let tags = &cfg.tag_labels;
    let mut x = cfg.padding_x;
    for (i, label) in tags.iter().enumerate() {
        let (tw, _th) = pango_text_size(&layout, label);
        let w = ((tw as f64) + 2.0 * cfg.pill_hpadding).max(40.0);

        let (mut bg, mut bw, mut bc, txt_color, draw_bg) =
            tag_visuals(colors, state.monitor_info.as_ref(), i, is_light_theme);

        bc = pill_border_color(bc, is_light_theme);

        if HoverTarget::Tag(i) == state.hover_target {
            bg = bg.lighten(0.10);
            bc = bc.lighten(0.10);
            bw = (bw + 1.0).min(3.0);
        }

        if draw_bg {
            let _ = draw_soft_shadow(
                cr,
                state.shape_style,
                x,
                cfg.padding_y,
                w,
                pill_h,
                cfg.pill_radius,
                colors.bg,
                is_light_theme,
            );
            stroke_shape_with_fill(
                cr,
                state.shape_style,
                x,
                cfg.padding_y,
                w,
                pill_h,
                cfg.pill_radius,
                bw,
                bc,
                Some(bg),
            )?;
            pango_draw_text_centered(cr, &layout, txt_color, x, cfg.padding_y, w, pill_h, label);
        }
        state.tag_rects[i] = Rect {
            x: x as i16,
            y: cfg.padding_y as i16,
            w: w as u16,
            h: pill_h as u16,
        };
        x += w + cfg.tag_spacing;
    }
    Ok(x)
}

/// Draw the layout button pill (left-side). Returns the x position after the button.
pub fn draw_layout_button(
    cr: &Context,
    cfg: &BarConfig,
    state: &mut AppState,
    layout: &pango::Layout,
    pill_h: f64,
    is_light_theme: bool,
    colors: &Colors,
    x: f64,
) -> Result<f64> {
    let layout_label = state.layout_symbol.as_str();
    let (lw, lh) = pango_text_size(&layout, layout_label);
    let lw_total = lw as f64 + 2.0 * cfg.pill_hpadding;

    let mut layout_fill = if state.layout_selector_open {
        if is_light_theme { Color::rgb(20, 184, 166) } else { Color::rgb(13, 120, 110) }
    } else {
        if is_light_theme { Color::rgb(249, 115, 22) } else { Color::rgb(160, 75, 15) }
    };
    let mut layout_border = pill_border_color(layout_fill, is_light_theme);
    let mut layout_bw = 1.0;
    if state.hover_target == HoverTarget::LayoutButton {
        layout_fill = layout_fill.lighten(0.08);
        layout_border = pill_border_color(layout_fill, is_light_theme).lighten(0.06);
        layout_bw = 2.0;
    }
    let _ = draw_soft_shadow(
        cr,
        state.shape_style,
        x,
        cfg.padding_y,
        lw_total,
        pill_h,
        cfg.pill_radius,
        colors.bg,
        is_light_theme,
    );
    stroke_shape_with_fill(
        cr,
        state.shape_style,
        x,
        cfg.padding_y,
        lw_total,
        pill_h,
        cfg.pill_radius,
        layout_bw,
        layout_border,
        Some(layout_fill),
    )?;
    let ty = cfg.padding_y + (pill_h - lh as f64) / 2.0 - 1.0;
    pango_draw_text_left(
        cr,
        &layout,
        colors.white,
        x + cfg.pill_hpadding,
        ty,
        layout_label,
    );
    state.layout_button_rect = Rect {
        x: x as i16,
        y: cfg.padding_y as i16,
        w: lw_total as u16,
        h: pill_h as u16,
    };
    Ok(x + lw_total + cfg.tag_spacing)
}

/// Draw layout option pills (expanded selector), left-side.
/// `x` is the starting X position (after the layout button).
pub fn draw_layout_options(
    cr: &Context,
    cfg: &BarConfig,
    state: &mut AppState,
    layout: &pango::Layout,
    pill_h: f64,
    is_light_theme: bool,
    colors: &Colors,
    x: f64,
) -> Result<()> {
    if state.layout_selector_open {
        // relm_bar style: current layout gets teal-green, others are subtle dark
        let current_sym = state.layout_symbol.as_str();
        let layout_defs: [(&str, u32); 3] = [("[]=", 0), ("><>", 1), ("[M]", 2)];
        let layouts: [(&str, u32, Color); 3] = [
            (layout_defs[0].0, layout_defs[0].1, if layout_defs[0].0 == current_sym {
                if is_light_theme { Color::rgb(34, 197, 94) } else { Color::rgb(16, 120, 80) }
            } else {
                if is_light_theme { Color::rgb(220, 225, 230) } else { Color::rgb(30, 41, 59) }
            }),
            (layout_defs[1].0, layout_defs[1].1, if layout_defs[1].0 == current_sym {
                if is_light_theme { Color::rgb(34, 197, 94) } else { Color::rgb(16, 120, 80) }
            } else {
                if is_light_theme { Color::rgb(220, 225, 230) } else { Color::rgb(30, 41, 59) }
            }),
            (layout_defs[2].0, layout_defs[2].1, if layout_defs[2].0 == current_sym {
                if is_light_theme { Color::rgb(34, 197, 94) } else { Color::rgb(16, 120, 80) }
            } else {
                if is_light_theme { Color::rgb(220, 225, 230) } else { Color::rgb(30, 41, 59) }
            }),
        ];
        let mut opt_x = x;
        for (i, (sym, _idx, base_color)) in layouts.iter().enumerate() {
            let (tw, _th) = pango_text_size(&layout, sym);
            let w = ((tw as f64) + 2.0 * (cfg.pill_hpadding - 2.0)).max(32.0);

            let mut fill = *base_color;
            let mut border = pill_border_color(fill, is_light_theme);
            let mut bw = 1.0;
            if HoverTarget::LayoutOption(i) == state.hover_target {
                fill = fill.lighten(0.08);
                border = pill_border_color(fill, is_light_theme).lighten(0.06);
                bw = 2.0;
            }
            let _ = draw_soft_shadow(
                cr,
                state.shape_style,
                opt_x,
                cfg.padding_y,
                w,
                pill_h,
                cfg.pill_radius,
                colors.bg,
                is_light_theme,
            );
            stroke_shape_with_fill(
                cr,
                state.shape_style,
                opt_x,
                cfg.padding_y,
                w,
                pill_h,
                cfg.pill_radius,
                bw,
                border,
                Some(fill),
            )?;
            let opt_text = if *sym == current_sym {
                colors.white
            } else if is_light_theme {
                colors.text
            } else {
                Color::rgb(226, 232, 240)
            };
            pango_draw_text_centered(cr, &layout, opt_text, opt_x, cfg.padding_y, w, pill_h, sym);
            state.layout_option_rects[i] = Rect {
                x: opt_x as i16,
                y: cfg.padding_y as i16,
                w: w as u16,
                h: pill_h as u16,
            };
            opt_x += w + cfg.tag_spacing;
        }
    } else {
        state.layout_option_rects = [Rect::default(), Rect::default(), Rect::default()];
    }
    Ok(())
}

/// Draw theme toggle pill (right-side, optional).
pub fn draw_theme_toggle(
    cr: &Context,
    cfg: &BarConfig,
    state: &mut AppState,
    layout: &pango::Layout,
    pill_h: f64,
    is_light_theme: bool,
    colors: &Colors,
    right_x: &mut f64,
) -> Result<()> {
    if cfg.show_theme_toggle {
        let label = match state.theme_mode {
            ThemeMode::Dark => cfg.theme_dark_label,
            ThemeMode::Light => cfg.theme_light_label,
        };
        let (tw, th) = pango_text_size(&layout, label);
        let w = (tw as f64 + 2.0 * (cfg.pill_hpadding - 2.0)).max(54.0);
        *right_x -= w + cfg.tag_spacing;
        // relm_bar style: subtle neutral pill
        let mut fill = if is_light_theme {
            Color::rgb(255, 255, 255)
        } else {
            Color::rgb(30, 41, 59)
        };
        let mut border = if is_light_theme {
            Color::rgb(220, 225, 230)
        } else {
            Color::rgb(45, 55, 72)
        };
        let mut bw = 1.0;
        if HoverTarget::Theme == state.hover_target {
            fill = fill.lighten(0.08);
            border = border.lighten(0.10);
            bw = 2.0;
        }
        let _ = draw_soft_shadow(
            cr,
            state.shape_style,
            *right_x,
            cfg.padding_y,
            w,
            pill_h,
            cfg.pill_radius,
            colors.bg,
            is_light_theme,
        );
        stroke_shape_with_fill(
            cr,
            state.shape_style,
            *right_x,
            cfg.padding_y,
            w,
            pill_h,
            cfg.pill_radius,
            bw,
            border,
            Some(fill),
        )?;
        let theme_text = if is_light_theme { colors.text } else { Color::rgb(229, 231, 235) };
        let ty = cfg.padding_y + (pill_h - th as f64) / 2.0 - 1.0;
        pango_draw_text_left(
            cr,
            &layout,
            theme_text,
            *right_x + (w - tw as f64) / 2.0,
            ty,
            label,
        );
        state.theme_rect = Rect {
            x: *right_x as i16,
            y: cfg.padding_y as i16,
            w: w as u16,
            h: pill_h as u16,
        };
    } else {
        state.theme_rect = Rect::default();
    }
    Ok(())
}

/// Draw monitor badge pill (right-side).
pub fn draw_monitor_badge(
    cr: &Context,
    cfg: &BarConfig,
    state: &mut AppState,
    layout: &pango::Layout,
    pill_h: f64,
    is_light_theme: bool,
    colors: &Colors,
    right_x: &mut f64,
) -> Result<()> {
    let mon_label = state.monitor_label(cfg).to_string();
    let (mon_w, mon_h) = pango_text_size(&layout, &mon_label);
    let mon_total = mon_w as f64 + 2.0 * cfg.pill_hpadding;
    *right_x -= mon_total + cfg.tag_spacing;
    // relm_bar style: subtle dark monitor badge
    let mut mon_fill = if is_light_theme {
        Color::rgb(255, 255, 255)
    } else {
        Color::rgb(15, 23, 42)
    };
    let mut mon_border = if is_light_theme {
        Color::rgb(220, 225, 230)
    } else {
        Color::rgb(30, 38, 52)
    };
    let mut mon_bw = 1.0;
    if HoverTarget::Monitor == state.hover_target {
        mon_fill = mon_fill.lighten(0.08);
        mon_border = mon_border.lighten(0.10);
        mon_bw = 2.0;
    }
    let _ = draw_soft_shadow(
        cr,
        state.shape_style,
        *right_x,
        cfg.padding_y,
        mon_total,
        pill_h,
        cfg.pill_radius,
        colors.bg,
        is_light_theme,
    );
    stroke_shape_with_fill(
        cr,
        state.shape_style,
        *right_x,
        cfg.padding_y,
        mon_total,
        pill_h,
        cfg.pill_radius,
        mon_bw,
        mon_border,
        Some(mon_fill),
    )?;
    let mon_text = if is_light_theme { colors.text } else { Color::rgb(226, 232, 240) };
    let ty_mon = cfg.padding_y + (pill_h - mon_h as f64) / 2.0 - 1.0;
    pango_draw_text_left(
        cr,
        &layout,
        mon_text,
        *right_x + cfg.pill_hpadding,
        ty_mon,
        &mon_label,
    );
    state.mon_rect = Rect {
        x: *right_x as i16,
        y: cfg.padding_y as i16,
        w: mon_total as u16,
        h: pill_h as u16,
    };
    Ok(())
}

/// Draw time display pill (right-side).
pub fn draw_time_display(
    cr: &Context,
    cfg: &BarConfig,
    state: &mut AppState,
    layout: &pango::Layout,
    pill_h: f64,
    is_light_theme: bool,
    colors: &Colors,
    right_x: &mut f64,
) -> Result<()> {
    let time_str = state.format_time();
    let time_label = format!("{} {}", cfg.time_icon, time_str);
    let (time_w, time_h) = pango_text_size(&layout, &time_label);
    let time_total = time_w as f64 + 2.0 * cfg.pill_hpadding;
    *right_x -= time_total + cfg.tag_spacing;
    // relm_bar style: dark teal time pill
    let mut time_fill = if is_light_theme {
        Color::rgb(224, 242, 254)
    } else {
        Color::rgb(9, 41, 64)
    };
    let mut time_border = if is_light_theme {
        Color::rgb(147, 197, 253)
    } else {
        Color::rgb(35, 68, 77)
    };
    let mut time_bw = 1.0;
    if HoverTarget::Time == state.hover_target {
        time_fill = time_fill.lighten(0.08);
        time_border = time_border.lighten(0.10);
        time_bw = 2.0;
    }
    let _ = draw_soft_shadow(
        cr,
        state.shape_style,
        *right_x,
        cfg.padding_y,
        time_total,
        pill_h,
        cfg.pill_radius,
        colors.bg,
        is_light_theme,
    );
    stroke_shape_with_fill(
        cr,
        state.shape_style,
        *right_x,
        cfg.padding_y,
        time_total,
        pill_h,
        cfg.pill_radius,
        time_bw,
        time_border,
        Some(time_fill),
    )?;
    let time_text = if is_light_theme { Color::rgb(15, 23, 42) } else { Color::rgb(236, 254, 255) };
    let ty_time = cfg.padding_y + (pill_h - time_h as f64) / 2.0 - 1.0;
    pango_draw_text_left(
        cr,
        &layout,
        time_text,
        *right_x + cfg.pill_hpadding,
        ty_time,
        &time_label,
    );
    state.time_rect = Rect {
        x: *right_x as i16,
        y: cfg.padding_y as i16,
        w: time_total as u16,
        h: pill_h as u16,
    };
    Ok(())
}

/// Draw screenshot button pill (right-side).
pub fn draw_screenshot_button(
    cr: &Context,
    cfg: &BarConfig,
    state: &mut AppState,
    layout: &pango::Layout,
    pill_h: f64,
    is_light_theme: bool,
    colors: &Colors,
    right_x: &mut f64,
) -> Result<()> {
    let ss_label = cfg.screenshot_label;
    let (ss_w, ss_h) = pango_text_size(&layout, ss_label);
    let ss_total = ss_w as f64 + 2.0 * cfg.pill_hpadding;
    *right_x -= ss_total + cfg.tag_spacing;
    // relm_bar style: subtle screenshot pill
    let mut ss_fill = if is_light_theme {
        Color::rgb(238, 247, 251)
    } else {
        Color::rgb(30, 41, 59)
    };
    let mut ss_border = if is_light_theme {
        Color::rgb(204, 230, 242)
    } else {
        Color::rgb(45, 55, 72)
    };
    let mut ss_bw = 1.0;
    if HoverTarget::Screenshot == state.hover_target {
        ss_fill = ss_fill.lighten(0.08);
        ss_border = ss_border.lighten(0.10);
        ss_bw = 2.0;
    }
    let _ = draw_soft_shadow(
        cr,
        state.shape_style,
        *right_x,
        cfg.padding_y,
        ss_total,
        pill_h,
        cfg.pill_radius,
        colors.bg,
        is_light_theme,
    );
    stroke_shape_with_fill(
        cr,
        state.shape_style,
        *right_x,
        cfg.padding_y,
        ss_total,
        pill_h,
        cfg.pill_radius,
        ss_bw,
        ss_border,
        Some(ss_fill),
    )?;
    let ss_text = if is_light_theme { Color::rgb(21, 94, 117) } else { Color::rgb(226, 232, 240) };
    let ty_ss = cfg.padding_y + (pill_h - ss_h as f64) / 2.0 - 1.0;
    pango_draw_text_left(
        cr,
        &layout,
        ss_text,
        *right_x + cfg.pill_hpadding,
        ty_ss,
        ss_label,
    );
    state.ss_rect = Rect {
        x: *right_x as i16,
        y: cfg.padding_y as i16,
        w: ss_total as u16,
        h: pill_h as u16,
    };
    Ok(())
}

/// Draw audio volume pill (right-side, optional).
pub fn draw_audio_volume(
    cr: &Context,
    cfg: &BarConfig,
    state: &mut AppState,
    layout: &pango::Layout,
    pill_h: f64,
    is_light_theme: bool,
    colors: &Colors,
    right_x: &mut f64,
) -> Result<()> {
    if cfg.show_audio {
        let (label, muted) = if let Some(dev) = state.audio_manager.get_master_device() {
            let tag = if dev.is_muted { cfg.mute_label } else { cfg.volume_label };
            (
                format!("{} {}%", tag, dev.volume.clamp(0, 100)),
                dev.is_muted,
            )
        } else {
            (format!("{} --", cfg.volume_label), true)
        };

        let (aw, ah) = pango_text_size(&layout, &label);
        let a_total = aw as f64 + 2.0 * cfg.pill_hpadding;
        *right_x -= a_total + cfg.tag_spacing;

        // relm_bar style: teal accent for volume, subtle for muted
        let mut fill = if muted {
            if is_light_theme { Color::rgb(243, 244, 246) } else { Color::rgb(30, 41, 59) }
        } else {
            if is_light_theme { Color::rgb(255, 255, 255) } else { Color::rgb(12, 50, 70) }
        };
        let mut border = if muted {
            if is_light_theme { Color::rgb(209, 213, 219) } else { Color::rgb(45, 55, 72) }
        } else {
            if is_light_theme { Color::rgb(147, 197, 253) } else { Color::rgb(20, 70, 90) }
        };
        let mut bw = 1.0;
        if HoverTarget::Audio == state.hover_target {
            fill = fill.lighten(0.08);
            border = border.lighten(0.10);
            bw = 2.0;
        }
        let _ = draw_soft_shadow(
            cr,
            state.shape_style,
            *right_x,
            cfg.padding_y,
            a_total,
            pill_h,
            cfg.pill_radius,
            colors.bg,
            is_light_theme,
        );
        stroke_shape_with_fill(
            cr,
            state.shape_style,
            *right_x,
            cfg.padding_y,
            a_total,
            pill_h,
            cfg.pill_radius,
            bw,
            border,
            Some(fill),
        )?;
        let ty = cfg.padding_y + (pill_h - ah as f64) / 2.0 - 1.0;
        let audio_text = if is_light_theme {
            if muted { colors.text } else { Color::rgb(15, 23, 42) }
        } else {
            Color::rgb(236, 254, 255)
        };
        pango_draw_text_left(cr, &layout, audio_text, *right_x + cfg.pill_hpadding, ty, &label);
        state.audio_rect = Rect {
            x: *right_x as i16,
            y: cfg.padding_y as i16,
            w: a_total as u16,
            h: pill_h as u16,
        };
    } else {
        state.audio_rect = Rect::default();
    }
    Ok(())
}

/// Draw memory stats pill (right-side).
/// Returns (mem_total_gb, mem_used_gb, cpu_avg) for use by draw_cpu_stats.
pub fn draw_memory_stats(
    cr: &Context,
    cfg: &BarConfig,
    state: &mut AppState,
    layout: &pango::Layout,
    pill_h: f64,
    is_light_theme: bool,
    colors: &Colors,
    right_x: &mut f64,
) -> Result<f32> {
    let (mem_total_gb, mem_used_gb, cpu_avg) =
        if let Some(snap) = state.system_monitor.get_snapshot() {
            (
                (snap.memory_total as f32) / 1e9,
                (snap.memory_used as f32) / 1e9,
                snap.cpu_average,
            )
        } else {
            (0.0, 0.0, 0.0)
        };
    let mem_usage = if mem_total_gb > 0.0 {
        (mem_used_gb / mem_total_gb) * 100.0
    } else {
        0.0
    };
    let mem_label = format!("{} {:.0}%", cfg.mem_label, mem_usage.clamp(0.0, 100.0));
    let (mem_w, mem_h) = pango_text_size(&layout, &mem_label);
    let mem_total = mem_w as f64 + 2.0 * cfg.pill_hpadding;
    *right_x -= mem_total + cfg.tag_spacing;
    let base_mem_bg = usage_bg_color(colors, mem_usage, is_light_theme);
    let base_mem_fg = usage_text_color(colors, mem_usage, is_light_theme);
    let mut mem_bg = base_mem_bg;
    let mut mem_border = pill_border_color(base_mem_bg, is_light_theme);
    let mut mem_bw = 1.0;
    if HoverTarget::Mem == state.hover_target {
        mem_bg = mem_bg.lighten(0.08);
        mem_border = pill_border_color(mem_bg, is_light_theme).lighten(0.06);
        mem_bw = 2.0;
    }
    let _ = draw_soft_shadow(
        cr,
        state.shape_style,
        *right_x,
        cfg.padding_y,
        mem_total,
        pill_h,
        cfg.pill_radius,
        colors.bg,
        is_light_theme,
    );
    stroke_shape_with_fill(
        cr,
        state.shape_style,
        *right_x,
        cfg.padding_y,
        mem_total,
        pill_h,
        cfg.pill_radius,
        mem_bw,
        mem_border,
        Some(mem_bg),
    )?;
    let ty_mem = cfg.padding_y + (pill_h - mem_h as f64) / 2.0 - 1.0;
    pango_draw_text_left(
        cr,
        &layout,
        base_mem_fg,
        *right_x + cfg.pill_hpadding,
        ty_mem,
        &mem_label,
    );
    state.mem_rect = Rect {
        x: *right_x as i16,
        y: cfg.padding_y as i16,
        w: mem_total as u16,
        h: pill_h as u16,
    };
    Ok(cpu_avg)
}

/// Draw CPU stats pill (right-side).
pub fn draw_cpu_stats(
    cr: &Context,
    cfg: &BarConfig,
    state: &mut AppState,
    layout: &pango::Layout,
    pill_h: f64,
    is_light_theme: bool,
    colors: &Colors,
    right_x: &mut f64,
    cpu_avg: f32,
) -> Result<()> {
    let cpu_label = format!("{} {:.0}%", cfg.cpu_label, cpu_avg.clamp(0.0, 100.0));
    let (cpu_w, cpu_h) = pango_text_size(&layout, &cpu_label);
    let cpu_total = cpu_w as f64 + 2.0 * cfg.pill_hpadding;
    *right_x -= cpu_total + cfg.tag_spacing;
    let base_cpu_bg = usage_bg_color(colors, cpu_avg, is_light_theme);
    let base_cpu_fg = usage_text_color(colors, cpu_avg, is_light_theme);
    let mut cpu_bg = base_cpu_bg;
    let mut cpu_border = pill_border_color(base_cpu_bg, is_light_theme);
    let mut cpu_bw = 1.0;
    if HoverTarget::Cpu == state.hover_target {
        cpu_bg = cpu_bg.lighten(0.08);
        cpu_border = pill_border_color(cpu_bg, is_light_theme).lighten(0.06);
        cpu_bw = 2.0;
    }
    let _ = draw_soft_shadow(
        cr,
        state.shape_style,
        *right_x,
        cfg.padding_y,
        cpu_total,
        pill_h,
        cfg.pill_radius,
        colors.bg,
        is_light_theme,
    );
    stroke_shape_with_fill(
        cr,
        state.shape_style,
        *right_x,
        cfg.padding_y,
        cpu_total,
        pill_h,
        cfg.pill_radius,
        cpu_bw,
        cpu_border,
        Some(cpu_bg),
    )?;
    let ty_cpu = cfg.padding_y + (pill_h - cpu_h as f64) / 2.0 - 1.0;
    pango_draw_text_left(
        cr,
        &layout,
        base_cpu_fg,
        *right_x + cfg.pill_hpadding,
        ty_cpu,
        &cpu_label,
    );
    state.cpu_rect = Rect {
        x: *right_x as i16,
        y: cfg.padding_y as i16,
        w: cpu_total as u16,
        h: pill_h as u16,
    };
    Ok(())
}

/// Draw backlight brightness pill (right-side, optional).
pub fn draw_brightness(
    cr: &Context,
    cfg: &BarConfig,
    state: &mut AppState,
    layout: &pango::Layout,
    pill_h: f64,
    is_light_theme: bool,
    colors: &Colors,
    right_x: &mut f64,
) -> Result<()> {
    if !cfg.show_brightness {
        state.brightness_rect = Rect::default();
        return Ok(());
    }
    let label = match state.brightness_manager.percent() {
        Some(p) => format!("{} {}%", cfg.brightness_label, p),
        None => format!("{} --", cfg.brightness_label),
    };
    let (bw_t, bh_t) = pango_text_size(&layout, &label);
    let b_total = bw_t as f64 + 2.0 * cfg.pill_hpadding;
    *right_x -= b_total + cfg.tag_spacing;

    // relm_bar style: warm amber accent for brightness
    let mut fill = if is_light_theme {
        Color::rgb(255, 251, 235)
    } else {
        Color::rgb(58, 46, 16)
    };
    let mut border = if is_light_theme {
        Color::rgb(253, 230, 138)
    } else {
        Color::rgb(90, 72, 24)
    };
    let mut bw = 1.0;
    if HoverTarget::Brightness == state.hover_target {
        fill = fill.lighten(0.08);
        border = border.lighten(0.10);
        bw = 2.0;
    }
    let _ = draw_soft_shadow(
        cr,
        state.shape_style,
        *right_x,
        cfg.padding_y,
        b_total,
        pill_h,
        cfg.pill_radius,
        colors.bg,
        is_light_theme,
    );
    stroke_shape_with_fill(
        cr,
        state.shape_style,
        *right_x,
        cfg.padding_y,
        b_total,
        pill_h,
        cfg.pill_radius,
        bw,
        border,
        Some(fill),
    )?;
    let text_color = if is_light_theme {
        Color::rgb(146, 104, 16)
    } else {
        Color::rgb(254, 243, 199)
    };
    let ty = cfg.padding_y + (pill_h - bh_t as f64) / 2.0 - 1.0;
    pango_draw_text_left(cr, &layout, text_color, *right_x + cfg.pill_hpadding, ty, &label);
    state.brightness_rect = Rect {
        x: *right_x as i16,
        y: cfg.padding_y as i16,
        w: b_total as u16,
        h: pill_h as u16,
    };
    Ok(())
}

/// Draw battery pill (right-side, optional).
pub fn draw_battery(
    cr: &Context,
    cfg: &BarConfig,
    state: &mut AppState,
    layout: &pango::Layout,
    pill_h: f64,
    is_light_theme: bool,
    colors: &Colors,
    right_x: &mut f64,
) -> Result<()> {
    if !cfg.show_battery {
        state.battery_rect = Rect::default();
        return Ok(());
    }
    let charging = state.battery_manager.is_charging();
    let (label, low) = match state.battery_manager.capacity() {
        Some(c) => {
            let icon = if charging {
                cfg.battery_charging_label
            } else {
                cfg.battery_label
            };
            (format!("{} {}%", icon, c), c <= 20)
        }
        None => (format!("{} --", cfg.battery_label), false),
    };
    let (bw_t, bh_t) = pango_text_size(&layout, &label);
    let b_total = bw_t as f64 + 2.0 * cfg.pill_hpadding;
    *right_x -= b_total + cfg.tag_spacing;

    // Color: charging -> teal/green, low -> red, otherwise neutral green
    let (mut fill, mut border, text_color) = if charging {
        if is_light_theme {
            (
                Color::rgb(220, 252, 231),
                Color::rgb(134, 239, 172),
                Color::rgb(22, 101, 52),
            )
        } else {
            (
                Color::rgb(16, 52, 32),
                Color::rgb(28, 78, 50),
                Color::rgb(187, 247, 208),
            )
        }
    } else if low {
        if is_light_theme {
            (
                Color::rgb(255, 234, 234),
                Color::rgb(252, 165, 165),
                Color::rgb(168, 7, 26),
            )
        } else {
            (
                Color::rgb(58, 26, 32),
                Color::rgb(90, 40, 48),
                Color::rgb(241, 216, 216),
            )
        }
    } else if is_light_theme {
        (
            Color::rgb(240, 253, 244),
            Color::rgb(187, 247, 208),
            Color::rgb(22, 101, 52),
        )
    } else {
        (
            Color::rgb(17, 49, 36),
            Color::rgb(28, 70, 52),
            Color::rgb(209, 243, 218),
        )
    };
    let mut bw = 1.0;
    if HoverTarget::Battery == state.hover_target {
        fill = fill.lighten(0.08);
        border = border.lighten(0.10);
        bw = 2.0;
    }
    let _ = draw_soft_shadow(
        cr,
        state.shape_style,
        *right_x,
        cfg.padding_y,
        b_total,
        pill_h,
        cfg.pill_radius,
        colors.bg,
        is_light_theme,
    );
    stroke_shape_with_fill(
        cr,
        state.shape_style,
        *right_x,
        cfg.padding_y,
        b_total,
        pill_h,
        cfg.pill_radius,
        bw,
        border,
        Some(fill),
    )?;
    let ty = cfg.padding_y + (pill_h - bh_t as f64) / 2.0 - 1.0;
    pango_draw_text_left(cr, &layout, text_color, *right_x + cfg.pill_hpadding, ty, &label);
    state.battery_rect = Rect {
        x: *right_x as i16,
        y: cfg.padding_y as i16,
        w: b_total as u16,
        h: pill_h as u16,
    };
    Ok(())
}

// ================= 对外：绘制入口 =================

pub fn draw_bar(
    cr: &Context,
    width: u16,
    height: u16,
    colors: &Colors,
    state: &mut AppState,
    font: &FontDescription,
    cfg: &BarConfig,
) -> Result<()> {
    draw_bar_with_background_opacity(cr, width, height, colors, state, font, cfg, 1.0)
}

pub fn draw_bar_with_background_opacity(
    cr: &Context,
    width: u16,
    height: u16,
    colors: &Colors,
    state: &mut AppState,
    font: &FontDescription,
    cfg: &BarConfig,
    background_opacity: f64,
) -> Result<()> {
    let is_light_theme = colors.bg.r > 0.7 && colors.bg.g > 0.7 && colors.bg.b > 0.7;
    paint_bar_background(cr, width, height, colors.bg, is_light_theme, background_opacity)?;

    let layout = create_layout(cr);
    layout.set_font_description(Some(font));

    let pill_h = (height as f64) - 2.0 * cfg.padding_y;

    // Left side: tags -> layout button -> layout options
    let x = draw_left_tags(cr, cfg, state, &layout, pill_h, is_light_theme, colors)?;
    let x = draw_layout_button(cr, cfg, state, &layout, pill_h, is_light_theme, colors, x)?;
    draw_layout_options(cr, cfg, state, &layout, pill_h, is_light_theme, colors, x)?;

    // Right side: right-to-left
    let mut right_x = width as f64 - cfg.padding_x;
    draw_theme_toggle(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;
    draw_monitor_badge(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;
    draw_time_display(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;
    draw_screenshot_button(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;
    draw_audio_volume(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;
    let cpu_avg = draw_memory_stats(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;
    draw_cpu_stats(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x, cpu_avg)?;
    draw_brightness(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;
    draw_battery(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;

    Ok(())
}

/// Draw bar with selective region redrawing based on dirty bits.
///
/// - `dirty_bits = None`       -> Full redraw (all regions)
/// - `dirty_bits = Some(0)`    -> Skip redraw entirely
/// - `dirty_bits = Some(bits)` -> Redraw only the affected regions
///
/// HOVER_CHANGED and THEME_CHANGED trigger a full redraw because they
/// affect the visual appearance of every region.
pub fn draw_bar_with_dirty(
    cr: &Context,
    width: u16,
    height: u16,
    colors: &Colors,
    state: &mut AppState,
    font: &FontDescription,
    cfg: &BarConfig,
    dirty_bits: Option<DirtyBits>,
) -> Result<()> {
    draw_bar_with_dirty_background_opacity(
        cr,
        width,
        height,
        colors,
        state,
        font,
        cfg,
        dirty_bits,
        1.0,
    )
}

pub fn draw_bar_with_dirty_background_opacity(
    cr: &Context,
    width: u16,
    height: u16,
    colors: &Colors,
    state: &mut AppState,
    font: &FontDescription,
    cfg: &BarConfig,
    dirty_bits: Option<DirtyBits>,
    background_opacity: f64,
) -> Result<()> {
    // If dirty_bits is Some(0), nothing changed -- skip entirely.
    if let Some(ref dirty) = dirty_bits {
        if dirty.is_empty() {
            return Ok(());
        }
    }

    // Determine whether we need a full redraw.
    // None means "no tracking, redraw everything".
    // HOVER_CHANGED / THEME_CHANGED affect all regions, so treat as full.
    let full_redraw = match dirty_bits {
        None => true,
        Some(ref d) => {
            d.contains(DirtyBits::HOVER_CHANGED) || d.contains(DirtyBits::THEME_CHANGED)
        }
    };

    // ── Always: background, layout object, theme detection, pill_h ──
    let is_light_theme = colors.bg.r > 0.7 && colors.bg.g > 0.7 && colors.bg.b > 0.7;
    paint_bar_background(cr, width, height, colors.bg, is_light_theme, background_opacity)?;

    let layout = create_layout(cr);
    layout.set_font_description(Some(font));

    let pill_h = (height as f64) - 2.0 * cfg.padding_y;

    // Helper: should we draw a given region?
    // Under selective mode, only the regions whose dirty flag is set are drawn.
    let dirty_ref = &dirty_bits;
    let should_draw = |flag: u32| -> bool {
        if full_redraw {
            return true;
        }
        match dirty_ref {
            None => true,
            Some(d) => d.contains(flag),
        }
    };

    // ── Left side (left-to-right, position-chained) ──
    //
    // Tags and layout button positions depend on each other, so we always
    // compute positions for all left-side items.  We only *draw* the ones
    // that are dirty, but we still need the returned x for the next item.
    //
    // MONITOR_CHANGED affects tags (tag visuals depend on monitor_info).
    // LAYOUT_CHANGED affects layout button + layout options.

    let draw_tags = should_draw(DirtyBits::MONITOR_CHANGED);
    let draw_layout = should_draw(DirtyBits::LAYOUT_CHANGED);

    // Tags -- always compute positions for chaining; conditionally render.
    let x = if draw_tags {
        draw_left_tags(cr, cfg, state, &layout, pill_h, is_light_theme, colors)?
    } else {
        // Recompute x position without drawing (measure-only pass).
        let tags = &cfg.tag_labels;
        let mut x = cfg.padding_x;
        for (i, label) in tags.iter().enumerate() {
            let (tw, _th) = pango_text_size(&layout, label);
            let w = ((tw as f64) + 2.0 * cfg.pill_hpadding).max(40.0);
            state.tag_rects[i] = Rect {
                x: x as i16,
                y: cfg.padding_y as i16,
                w: w as u16,
                h: pill_h as u16,
            };
            x += w + cfg.tag_spacing;
        }
        x
    };

    // Layout button
    let x = if draw_layout {
        draw_layout_button(cr, cfg, state, &layout, pill_h, is_light_theme, colors, x)?
    } else {
        // Measure-only: advance x past the layout button.
        let layout_label = state.layout_symbol.as_str();
        let (lw, _lh) = pango_text_size(&layout, layout_label);
        let lw_total = lw as f64 + 2.0 * cfg.pill_hpadding;
        state.layout_button_rect = Rect {
            x: x as i16,
            y: cfg.padding_y as i16,
            w: lw_total as u16,
            h: pill_h as u16,
        };
        x + lw_total + cfg.tag_spacing
    };

    // Layout options
    if draw_layout {
        draw_layout_options(cr, cfg, state, &layout, pill_h, is_light_theme, colors, x)?;
    }

    // ── Right side (right-to-left, position-chained) ──
    //
    // Each right-side region decrements `right_x`.  Like the left side, we
    // always advance the position but only draw when the region is dirty.
    //
    // The call order matches draw_bar():
    //   theme_toggle -> monitor_badge -> time_display -> screenshot_button
    //   -> audio_volume -> memory_stats -> cpu_stats

    let mut right_x = width as f64 - cfg.padding_x;

    // Theme toggle (always if show_theme_toggle; dirty on THEME_CHANGED -- but
    // THEME_CHANGED already triggers full_redraw, so under selective mode this
    // is always skipped unless full_redraw. We draw it under full_redraw.)
    if full_redraw {
        draw_theme_toggle(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;
    } else if cfg.show_theme_toggle {
        // Advance right_x without drawing so subsequent positions are correct.
        let label = match state.theme_mode {
            ThemeMode::Dark => cfg.theme_dark_label,
            ThemeMode::Light => cfg.theme_light_label,
        };
        let (tw, _) = pango_text_size(&layout, label);
        let w = (tw as f64 + 2.0 * (cfg.pill_hpadding - 2.0)).max(54.0);
        right_x -= w + cfg.tag_spacing;
    }

    // Monitor badge
    if should_draw(DirtyBits::MONITOR_CHANGED) {
        draw_monitor_badge(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;
    } else {
        let mon_label = state.monitor_label(cfg).to_string();
        let (mon_w, _) = pango_text_size(&layout, &mon_label);
        let mon_total = mon_w as f64 + 2.0 * cfg.pill_hpadding;
        right_x -= mon_total + cfg.tag_spacing;
    }

    // Time display
    if should_draw(DirtyBits::TIME_CHANGED) {
        draw_time_display(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;
    } else {
        // Advance past time pill. Use the cached rect width if available,
        // otherwise measure the current time string.
        if state.time_rect.w > 0 {
            right_x -= state.time_rect.w as f64 + cfg.tag_spacing;
        } else {
            let time_str = state.format_time();
            let time_label = format!("{} {}", cfg.time_icon, time_str);
            let (tw, _) = pango_text_size(&layout, &time_label);
            let w = tw as f64 + 2.0 * cfg.pill_hpadding;
            right_x -= w + cfg.tag_spacing;
        }
    }

    // Screenshot button (static, always drawn on full redraw; skip on selective)
    if full_redraw {
        draw_screenshot_button(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;
    } else {
        let ss_label = cfg.screenshot_label;
        let (ss_w, _) = pango_text_size(&layout, ss_label);
        let ss_total = ss_w as f64 + 2.0 * cfg.pill_hpadding;
        right_x -= ss_total + cfg.tag_spacing;
    }

    // Audio volume
    if should_draw(DirtyBits::AUDIO_CHANGED) {
        draw_audio_volume(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;
    } else if cfg.show_audio {
        // Advance past audio pill without drawing.
        let (label, _muted) = if let Some(dev) = state.audio_manager.get_master_device() {
            let tag = if dev.is_muted { cfg.mute_label } else { cfg.volume_label };
            (format!("{} {}%", tag, dev.volume.clamp(0, 100)), dev.is_muted)
        } else {
            (format!("{} --", cfg.volume_label), true)
        };
        let (aw, _) = pango_text_size(&layout, &label);
        let a_total = aw as f64 + 2.0 * cfg.pill_hpadding;
        right_x -= a_total + cfg.tag_spacing;
    }

    // Memory stats + CPU stats (coupled: memory returns cpu_avg for cpu pill)
    let draw_system = should_draw(DirtyBits::SYSTEM_CHANGED);
    if draw_system {
        let cpu_avg = draw_memory_stats(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;
        draw_cpu_stats(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x, cpu_avg)?;
    } else {
        // Advance past memory pill.
        let (mem_total_gb, mem_used_gb, cpu_avg) =
            if let Some(snap) = state.system_monitor.get_snapshot() {
                ((snap.memory_total as f32) / 1e9, (snap.memory_used as f32) / 1e9, snap.cpu_average)
            } else {
                (0.0, 0.0, 0.0)
            };
        let mem_usage = if mem_total_gb > 0.0 { (mem_used_gb / mem_total_gb) * 100.0 } else { 0.0 };
        let mem_label = format!("{} {:.0}%", cfg.mem_label, mem_usage.clamp(0.0, 100.0));
        let (mem_w, _) = pango_text_size(&layout, &mem_label);
        let mem_total = mem_w as f64 + 2.0 * cfg.pill_hpadding;
        right_x -= mem_total + cfg.tag_spacing;

        // Advance past CPU pill.
        let cpu_label = format!("{} {:.0}%", cfg.cpu_label, cpu_avg.clamp(0.0, 100.0));
        let (cpu_w, _) = pango_text_size(&layout, &cpu_label);
        let cpu_total = cpu_w as f64 + 2.0 * cfg.pill_hpadding;
        right_x -= cpu_total + cfg.tag_spacing;
    }

    // Brightness
    if should_draw(DirtyBits::BRIGHTNESS_CHANGED) {
        draw_brightness(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;
    } else if cfg.show_brightness {
        let label = match state.brightness_manager.percent() {
            Some(p) => format!("{} {}%", cfg.brightness_label, p),
            None => format!("{} --", cfg.brightness_label),
        };
        let (bw_t, _) = pango_text_size(&layout, &label);
        let b_total = bw_t as f64 + 2.0 * cfg.pill_hpadding;
        right_x -= b_total + cfg.tag_spacing;
    }

    // Battery
    if should_draw(DirtyBits::BATTERY_CHANGED) {
        draw_battery(cr, cfg, state, &layout, pill_h, is_light_theme, colors, &mut right_x)?;
    } else if cfg.show_battery {
        let charging = state.battery_manager.is_charging();
        let label = match state.battery_manager.capacity() {
            Some(c) => {
                let icon = if charging {
                    cfg.battery_charging_label
                } else {
                    cfg.battery_label
                };
                format!("{} {}%", icon, c)
            }
            None => format!("{} --", cfg.battery_label),
        };
        let (bw_t, _) = pango_text_size(&layout, &label);
        let b_total = bw_t as f64 + 2.0 * cfg.pill_hpadding;
        right_x -= b_total + cfg.tag_spacing;
    }
    let _ = right_x; // suppress unused warning

    // ── Clear dirty bits ──
    state.dirty_fields = DirtyBits::new(0);

    Ok(())
}

// ================= timerfd 对齐到秒 =================

pub fn arm_second_timer(tfd: libc::c_int) -> std::io::Result<()> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let now_ns = (ts.tv_sec as i128) * 1_000_000_000i128 + (ts.tv_nsec as i128);
    let next_sec_ns = ((ts.tv_sec as i128) + 1) * 1_000_000_000i128;
    let diff_ns = (next_sec_ns - now_ns) as i64;

    let its = libc::itimerspec {
        it_value: libc::timespec {
            tv_sec: (diff_ns / 1_000_000_000) as libc::time_t,
            tv_nsec: (diff_ns % 1_000_000_000) as libc::c_long,
        },
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 500_000_000,
        },
    };
    let rc = unsafe { libc::timerfd_settime(tfd, 0, &its as *const _, std::ptr::null_mut()) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// ================= eventfd 集成 shared_ring_buffer 通知 =================

pub const SHARED_TOKEN: u64 = 3;

pub fn spawn_shared_eventfd_notifier(
    shared_buffer: Option<Arc<SharedRingBuffer>>,
    non_block: bool,
) -> Option<libc::c_int> {
    let Some(buf) = shared_buffer.clone() else {
        return None;
    };
    // 创建 eventfd：非阻塞 + CLOEXEC
    let efd = unsafe {
        if non_block {
            libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC)
        } else {
            libc::eventfd(0, libc::EFD_CLOEXEC)
        }
    };
    if efd < 0 {
        error!("eventfd create failed: {}", std::io::Error::last_os_error());
        return None;
    }
    std::thread::spawn(move || {
        loop {
            match buf.wait_for_message(None) {
                Ok(true) => {
                    // 有新消息到达，通知主线程
                    let one: u64 = 1;
                    let ptr = &one as *const u64 as *const libc::c_void;
                    let r = unsafe { libc::write(efd, ptr, std::mem::size_of::<u64>()) };
                    if r < 0 {
                        let err = std::io::Error::last_os_error();
                        if let Some(code) = err.raw_os_error() {
                            // EBADF: 主线程可能已关闭 efd，退出线程
                            if code == libc::EBADF {
                                break;
                            }
                            // EAGAIN: 计数器已满（极少见），忽略
                            if code != libc::EAGAIN {
                                warn!("eventfd write error: {}", err);
                            }
                        }
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    warn!("Shared wait failed: {}", e);
                    break;
                }
            }
        }
    });
    Some(efd)
}

// ================= 日志初始化 =================

pub fn initialize_logging(program_name: &str, shared_path: &str) -> Result<()> {
    use chrono::Local as ChronoLocal;
    use flexi_logger::{Cleanup, Criterion, Duplicate, FileSpec, Logger, Naming};

    let tmp_now = ChronoLocal::now();
    let timestamp = tmp_now.format("%Y-%m-%d_%H_%M_%S").to_string();

    let log_dir_candidates = [Some("/var/tmp/jwm".to_string())];

    let log_dir = log_dir_candidates
        .into_iter()
        .flatten()
        .find(|p| {
            std::fs::create_dir_all(p).ok();
            std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
        })
        .unwrap_or_else(|| ".".to_string());

    let file_name = if shared_path.is_empty() {
        program_name.to_string()
    } else {
        std::path::Path::new(shared_path)
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| format!("{}_{}", program_name, name))
            .unwrap_or_else(|| program_name.to_string())
    };

    let log_filename = format!("{}_{}", file_name, timestamp);
    let log_spec = std::env::var("RUST_LOG").unwrap_or_else(|_| "debug".to_string());

    Logger::try_with_str(log_spec)?
        .format_for_files(flexi_logger::detailed_format)
        .format_for_stderr(flexi_logger::colored_opt_format)
        .log_to_file(
            FileSpec::default()
                .directory(&log_dir)
                .basename(log_filename)
                .suffix("log"),
        )
        .duplicate_to_stdout(Duplicate::Info)
        .rotate(
            Criterion::Size(10_000_000), // 10MB
            Naming::Numbers,
            Cleanup::KeepLogFiles(5),
        )
        .start()?;

    info!("Log directory: {}", log_dir);
    Ok(())
}
