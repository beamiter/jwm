use chrono::Local;
use iced::futures::channel::mpsc;
use iced::futures::{SinkExt, Stream, StreamExt};
use iced::mouse;
use iced::time::{self, milliseconds};
use iced::widget::container;
use iced::widget::{Space, button, rich_text};
use iced::widget::{mouse_area, span};
use iced::{Font, stream, theme};

use iced::window::Id;
use iced::{
    Background, Border, Color, Element, Length, Size, Subscription, Task, Theme, border, color,
    widget::{Column, Row, text},
    window,
};

use log::{debug, error, info, warn};
use std::env;
use std::process::Command;
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use shared_structures::{CommandType, MonitorInfo, SharedCommand, SharedMessage, SharedRingBuffer};
use xbar_core::audio_manager::AudioManager;
use xbar_core::brightness::BrightnessManager;
use xbar_core::initialize_logging;
use xbar_core::system_monitor::SystemMonitor;

static _START: Once = Once::new();

const NERD_FONT: Font = Font::new("JetBrainsMono Nerd Font");

// Nerd-font icons used across the bar (aligned with tauri_react_bar)
const TAG_ICONS: [&str; 9] = [
    "\u{F0A1E}", // terminal
    "\u{F0239}", // browser
    "\u{F0A1B}", // code
    "\u{F0B79}", // chat
    "\u{F024B}", // folder
    "\u{F0388}", // music
    "\u{F0567}", // video
    "\u{F01F0}", // mail
    "\u{F0297}", // gamepad
];

const ICON_CPU: &str = "\u{F4BC}";
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

fn main() -> iced::Result {
    let args: Vec<String> = env::args().collect();
    let application_id = "dev.iced.bar".to_string();
    let shared_path = args.iter().skip(1).last().cloned().unwrap_or_default();

    if let Err(e) = initialize_logging("iced_bar", &shared_path) {
        eprintln!("Failed to initialize logging: {}", e);
        std::process::exit(1);
    }

    iced::application(IcedBar::new, IcedBar::update, IcedBar::view)
        .window(window::Settings {
            platform_specific: window::settings::PlatformSpecific {
                application_id,
                ..Default::default()
            },
            size: Size::from([800., 40.]),
            decorations: false,
            transparent: true,
            level: window::Level::AlwaysOnTop,
            ..Default::default()
        })
        .default_font(NERD_FONT)
        .subscription(IcedBar::subscription)
        .title("iced_bar")
        .scale_factor(IcedBar::scale_factor)
        .run()
}

#[allow(dead_code)]
enum Input {
    DoSomeWork,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
enum Message {
    TabSelected(usize),
    LayoutClicked(u32),
    ToggleLayoutSelector,
    ShowSecondsToggle,

    GetWindowId,
    WindowIdReceived(Option<Id>),

    GetScaleFactor(f32),

    MouseEnterScreenShot,
    MouseExitScreenShot,
    LeftClick,
    RightClick,

    UpdateTime,
    SharedMemoryUpdated(SharedMessage),
    SharedMemoryError(String),

    // Audio
    AudioToggleMute,
    AudioAdjust(i32),

    // Brightness
    BrightnessAdjust(i32),
}

struct IcedBar {
    active_tab: usize,
    tabs: [&'static str; 9],
    tab_colors: [Color; 9],
    shared_buffer_rc: Option<Arc<SharedRingBuffer>>,
    shared_path: String,
    monitor_info_opt: Option<MonitorInfo>,
    formated_now: String,
    current_window_id: Option<Id>,
    scale_factor: f32,
    is_hovered: bool,
    mouse_position: Option<iced::Point>,
    show_seconds: bool,
    layout_symbol: String,
    monitor_num: i32,

    // Audio + System + Brightness
    audio_manager: AudioManager,
    system_monitor: SystemMonitor,
    brightness_manager: BrightnessManager,

    transparent: bool,

    // throttle
    last_clock_update: Instant,
    last_monitor_update: Instant,

    // layout selector
    layout_selector_open: bool,
}

impl Default for IcedBar {
    fn default() -> Self {
        IcedBar::new()
    }
}

impl IcedBar {
    const DEFAULT_COLOR: Color = color!(0x666666);
    const TAB_WIDTH: f32 = 38.0;
    const TAB_HEIGHT: f32 = 32.0;
    const TAB_SPACING: f32 = 8.0;
    const PILL_HEIGHT: f32 = 26.0;

    fn new() -> Self {
        let args: Vec<String> = env::args().collect();
        let shared_path = args.iter().skip(1).last().cloned().unwrap_or_default();

        let shared_buffer_rc =
            SharedRingBuffer::create_shared_ring_buffer_aux(&shared_path).map(Arc::new);

        Self {
            active_tab: 0,
            tabs: TAG_ICONS,
            tab_colors: [
                color!(0xFF6B6B), // red
                color!(0x4ECDC4), // cyan
                color!(0x45B7D1), // blue
                color!(0x96CEB4), // green
                color!(0xFECA57), // yellow
                color!(0xFF9FF3), // pink
                color!(0x54A0FF), // light blue
                color!(0x5F27CD), // purple
                color!(0x00D2D3), // teal
            ],
            shared_buffer_rc,
            shared_path,
            monitor_info_opt: None,
            formated_now: String::new(),
            current_window_id: None,
            scale_factor: 1.0,
            is_hovered: false,
            mouse_position: None,
            show_seconds: true,
            layout_symbol: "[]=".to_string(),
            monitor_num: 0,
            audio_manager: AudioManager::new(),
            system_monitor: SystemMonitor::new(5),
            brightness_manager: BrightnessManager::new(),
            transparent: true,
            last_clock_update: Instant::now(),
            last_monitor_update: Instant::now(),
            layout_selector_open: false,
        }
    }

    fn prepare_worker() -> impl Stream<Item = Message> {
        stream::channel(10, async |mut output| {
            let _ = output.send(Message::GetWindowId).await;
        })
    }

    fn message_notify_subscription(
        shared_buffer_rc: Option<Arc<SharedRingBuffer>>,
    ) -> Subscription<Message> {
        Subscription::run_with(shared_buffer_rc, |shared_buffer_rc_ref| {
            let owned_shared_buffer_rc = shared_buffer_rc_ref.clone();
            stream::channel(100, move |mut output: mpsc::Sender<Message>| async move {
                let shared_buffer = if let Some(shared_buffer) = owned_shared_buffer_rc {
                    shared_buffer
                } else {
                    let _ = output
                        .send(Message::SharedMemoryError(
                            "Empty shared buffer".to_string(),
                        ))
                        .await;
                    return;
                };

                let (mut tx, mut rx) = mpsc::channel::<Message>(100);
                let buffer_clone = shared_buffer.clone();
                let stop = Arc::new(AtomicBool::new(false));
                let stop_c = stop.clone();

                std::thread::spawn(move || {
                    let mut prev_timestamp: u128 = 0;
                    while !stop_c.load(Ordering::Relaxed) {
                        match buffer_clone.wait_for_message(Some(Duration::from_secs(2))) {
                            Ok(true) => {
                                if let Ok(Some(message)) = buffer_clone.try_read_latest_message() {
                                    let ts: u128 = message.timestamp as u128;
                                    if prev_timestamp != ts {
                                        prev_timestamp = ts;
                                        if tx
                                            .try_send(Message::SharedMemoryUpdated(message))
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                            }
                            Ok(false) => { /* timeout */ }
                            Err(e) => {
                                let _ = tx.try_send(Message::SharedMemoryError(format!(
                                    "Wait for message failed: {}",
                                    e
                                )));
                                break;
                            }
                        }
                    }
                });

                while let Some(msg) = rx.next().await {
                    if output.send(msg).await.is_err() {
                        break;
                    }
                }

                stop.store(true, Ordering::Relaxed);
            })
        })
    }

    fn send_tag_command(&mut self, is_view: bool) {
        let tag_bit = 1 << self.active_tab;
        let command = if is_view {
            SharedCommand::view_tag(tag_bit, self.monitor_num)
        } else {
            SharedCommand::toggle_tag(tag_bit, self.monitor_num)
        };

        if let Some(shared_buffer) = &self.shared_buffer_rc {
            match shared_buffer.send_command(command) {
                Ok(true) => info!("Sent command: {:?} by shared_buffer", command),
                Ok(false) => warn!("Command buffer full, command dropped"),
                Err(e) => error!("Failed to send command: {}", e),
            }
        }
    }

    fn send_layout_command(&mut self, layout_index: u32) {
        let command = SharedCommand::new(CommandType::SetLayout, layout_index, self.monitor_num);
        if let Some(shared_buffer) = &self.shared_buffer_rc {
            match shared_buffer.send_command(command) {
                Ok(true) => info!("Sent command: {:?} by shared_buffer", command),
                Ok(false) => warn!("Command buffer full, command dropped"),
                Err(e) => error!("Failed to send command: {}", e),
            }
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::TabSelected(tab_index) => {
                info!("Tab selected: {}", tab_index);
                self.active_tab = tab_index;
                self.send_tag_command(true);
                Task::none()
            }

            Message::LayoutClicked(layout_index) => {
                self.send_layout_command(layout_index);
                info!("Layout selected: {}", layout_index);
                self.layout_selector_open = false;
                Task::none()
            }

            Message::ToggleLayoutSelector => {
                self.layout_selector_open = !self.layout_selector_open;
                Task::none()
            }

            Message::GetWindowId => {
                info!("GetWindowId");
                window::latest().map(Message::WindowIdReceived)
            }

            Message::WindowIdReceived(window_id) => {
                if let Some(wid) = window_id {
                    info!("WindowIdReceived: {:?}", wid);
                    self.current_window_id = Some(wid);
                    Task::batch([window::scale_factor(wid).map(Message::GetScaleFactor)])
                } else {
                    warn!("WindowId not available yet");
                    Task::none()
                }
            }

            Message::MouseEnterScreenShot => {
                self.is_hovered = true;
                Task::none()
            }

            Message::MouseExitScreenShot => {
                self.is_hovered = false;
                self.mouse_position = None;
                Task::none()
            }

            Message::ShowSecondsToggle => {
                self.show_seconds = !self.show_seconds;
                return Task::perform(async {}, |_| Message::UpdateTime);
            }

            Message::LeftClick => {
                if let Err(e) = Command::new("flameshot").arg("gui").spawn() {
                    warn!("Failed to spawn flameshot: {e}");
                }
                Task::none()
            }

            Message::RightClick => Task::none(),

            Message::AudioToggleMute => {
                if let Some(dev) = self.audio_manager.get_master_device().cloned() {
                    let _ = self.audio_manager.toggle_mute(&dev.name);
                }
                Task::none()
            }

            Message::AudioAdjust(delta) => {
                if let Some(dev) = self.audio_manager.get_master_device().cloned() {
                    let new_v = (dev.volume + delta).clamp(0, 100);
                    let _ = self.audio_manager.set_volume(&dev.name, new_v, dev.is_muted);
                }
                Task::none()
            }

            Message::BrightnessAdjust(delta) => {
                self.brightness_manager.adjust(delta);
                Task::none()
            }

            Message::GetScaleFactor(scale_factor) => {
                info!("scale_factor: {}", scale_factor);
                self.scale_factor = scale_factor;
                Task::none()
            }

            Message::UpdateTime => {
                if self.last_clock_update.elapsed() >= Duration::from_millis(900) {
                    let tmp_now = Local::now();
                    let format_str = if self.show_seconds {
                        "%Y-%m-%d %H:%M:%S"
                    } else {
                        "%Y-%m-%d %H:%M"
                    };
                    self.formated_now = tmp_now.format(format_str).to_string();
                    self.last_clock_update = Instant::now();
                }

                if self.last_monitor_update.elapsed() >= Duration::from_secs(2) {
                    self.system_monitor.update_if_needed();
                    self.audio_manager.update_if_needed();
                    self.brightness_manager.update_if_needed();
                    self.last_monitor_update = Instant::now();
                }

                Task::none()
            }

            Message::SharedMemoryUpdated(message) => {
                debug!("SharedMemoryUpdated: {:?}", message.timestamp);
                self.monitor_info_opt = Some(message.monitor_info);
                if let Some(monitor_info) = self.monitor_info_opt.as_ref() {
                    self.layout_symbol = monitor_info.get_ltsymbol();
                    self.monitor_num = monitor_info.monitor_num;
                    for (index, tag_status) in monitor_info.tag_status_vec.iter().enumerate() {
                        if tag_status.is_selected {
                            self.active_tab = index;
                        }
                    }
                }
                Task::none()
            }

            Message::SharedMemoryError(err) => {
                warn!("SharedMemoryError: {err}");
                Task::none()
            }
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        if self.current_window_id.is_none() {
            Subscription::run(Self::prepare_worker)
        } else {
            let clock = time::every(milliseconds(1000)).map(|_| Message::UpdateTime);
            let shared = if self.shared_path.is_empty() {
                Subscription::none()
            } else {
                Self::message_notify_subscription(self.shared_buffer_rc.clone())
            };
            Subscription::batch(vec![clock, shared])
        }
    }

    #[allow(dead_code)]
    fn style(&self, theme: &Theme) -> theme::Style {
        if self.transparent {
            theme::Style {
                background_color: Color::TRANSPARENT,
                text_color: theme.palette().background.base.text,
            }
        } else {
            theme::default(theme)
        }
    }

    fn scale_factor(&self) -> f32 {
        1.0 / self.scale_factor
    }

    fn monitor_num_to_icon(monitor_num: i32) -> String {
        match monitor_num {
            0 => ICON_M0.to_string(),
            1 => ICON_M1.to_string(),
            n => format!("M{}", n),
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

    // -------- Workspace pills --------
    fn tag_visuals(&self, index: usize) -> (Color, f32, Color) {
        let tag_color = self
            .tab_colors
            .get(index)
            .copied()
            .unwrap_or(Self::DEFAULT_COLOR);

        if let Some(monitor) = self.monitor_info_opt.as_ref() {
            if let Some(status) = monitor.tag_status_vec.get(index) {
                if status.is_urg {
                    return (
                        Color::from_rgba(0.86, 0.21, 0.27, 1.0),
                        2.0,
                        Color::from_rgba(0.74, 0.13, 0.19, 1.0),
                    );
                } else if status.is_filled {
                    return (tag_color.scale_alpha(1.0), 2.0, tag_color);
                } else if status.is_selected {
                    return (tag_color.scale_alpha(0.7), 1.5, tag_color);
                } else if status.is_occ {
                    return (tag_color.scale_alpha(0.3), 1.0, tag_color.scale_alpha(0.6));
                }
            }
        }

        // default
        (Color::WHITE.scale_alpha(0.9), 1.0, color!(0xDEE2E6))
    }

    fn workspace_button<'a>(
        &self,
        index: usize,
        label: &'a str,
    ) -> iced::widget::Button<'a, Message> {
        let (bg, border_w, border_c) = self.tag_visuals(index);
        let is_selected = self
            .monitor_info_opt
            .as_ref()
            .and_then(|m| m.tag_status_vec.get(index))
            .map(|s| s.is_filled || s.is_selected || s.is_urg)
            .unwrap_or(false);

        let text_color = if is_selected {
            // Yellow tag uses dark text for legibility
            if index == 4 {
                color!(0x333333)
            } else {
                Color::WHITE
            }
        } else {
            color!(0x333333)
        };

        let radius = 6.0;
        button(
            rich_text![span(label.to_string()).color(text_color)]
                .size(18)
                .on_link_click(std::convert::identity),
        )
        .padding([4, 6])
        .width(Self::TAB_WIDTH)
        .height(Self::TAB_HEIGHT)
        .style(move |_theme: &Theme, status: button::Status| {
            let mut background = bg;
            let mut border_width = border_w;

            match status {
                button::Status::Hovered => {
                    border_width = (border_w + 1.0).min(3.0);
                    if background.a > 0.0 {
                        background.a = (background.a + 0.08).min(1.0);
                    } else {
                        background = Color::from_rgba(1.0, 1.0, 1.0, 0.10);
                    }
                }
                button::Status::Pressed => {
                    if background.a > 0.0 {
                        background.a = (background.a + 0.12).min(1.0);
                    } else {
                        background = Color::from_rgba(0.9, 0.9, 0.9, 0.10);
                    }
                }
                _ => {}
            }

            button::Style {
                background: Some(Background::Color(background)),
                text_color,
                border: Border {
                    color: border_c,
                    width: border_width,
                    radius: border::Radius::from(radius),
                },
                ..Default::default()
            }
        })
        .on_press(Message::TabSelected(index))
    }

    // -------- Pills --------

    fn pill_style(bg: Color, border_c: Color, text_color: Color) -> container::Style {
        container::Style {
            background: Some(Background::Color(bg)),
            text_color: Some(text_color),
            border: Border {
                radius: border::radius(12.0),
                width: 1.0,
                color: border_c,
            },
            ..Default::default()
        }
    }

    fn usage_colors(usage_percent: f32) -> (Color, Color) {
        match usage_percent {
            u if u <= 30.0 => (color!(0x1FBF51).scale_alpha(0.9), Color::WHITE),
            u if u <= 60.0 => (color!(0xF4C20D).scale_alpha(0.9), color!(0x000000)),
            u if u <= 80.0 => (color!(0xFF8C1A).scale_alpha(0.9), Color::WHITE),
            _ => (color!(0xE53935).scale_alpha(0.9), Color::WHITE),
        }
    }

    fn battery_colors(percent: f32) -> (Color, Color) {
        if percent > 50.0 {
            (color!(0x1FBF51).scale_alpha(0.9), Color::WHITE)
        } else if percent > 20.0 {
            (color!(0xF4C20D).scale_alpha(0.9), color!(0x000000))
        } else {
            (color!(0xE53935).scale_alpha(0.9), Color::WHITE)
        }
    }

    fn usage_pill<'a>(&self, icon: &'a str, value: f32) -> Element<'a, Message> {
        let (bg, fg) = Self::usage_colors(value);
        container(
            text(format!("{}  {:.0}%", icon, value))
                .size(14)
                .color(fg),
        )
        .padding([3, 10])
        .height(Self::PILL_HEIGHT)
        .style(move |_theme: &Theme| Self::pill_style(bg, bg, fg))
        .into()
    }

    fn battery_pill<'a>(&self) -> Element<'a, Message> {
        let (pct, charging) = self
            .system_monitor
            .get_snapshot()
            .map(|s| (s.battery_percent, s.is_charging))
            .unwrap_or((0.0, false));
        let icon = if charging { ICON_BAT_CHG } else { ICON_BAT_FULL };
        let (bg, fg) = Self::battery_colors(pct);
        container(
            text(format!("{}  {:.0}%", icon, pct))
                .size(14)
                .color(fg),
        )
        .padding([3, 10])
        .height(Self::PILL_HEIGHT)
        .style(move |_theme: &Theme| Self::pill_style(bg, bg, fg))
        .into()
    }

    fn brightness_pill<'a>(&self) -> Element<'a, Message> {
        let label = match self.brightness_manager.percent() {
            Some(p) => format!("{}  {}%", ICON_BRIGHT, p),
            None => format!("{}  --", ICON_BRIGHT),
        };
        let bg = color!(0xFDE047).scale_alpha(0.92);
        let fg = color!(0x1F2937);
        let pill = container(text(label).size(14).color(fg))
            .padding([3, 10])
            .height(Self::PILL_HEIGHT)
            .style(move |_theme: &Theme| Self::pill_style(bg, color!(0xFACC15), fg));

        mouse_area(pill)
            .on_press(Message::BrightnessAdjust(5))
            .on_right_press(Message::BrightnessAdjust(-5))
            .on_scroll(|delta| {
                let d = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y,
                };
                if d > 0.0 {
                    Message::BrightnessAdjust(5)
                } else {
                    Message::BrightnessAdjust(-5)
                }
            })
            .into()
    }

    fn volume_pill<'a>(&self) -> Element<'a, Message> {
        let master = self.audio_manager.get_master_device();
        let (volume, muted, has_device) = if let Some(dev) = master {
            (dev.volume.clamp(0, 100), dev.is_muted, true)
        } else {
            (0, true, false)
        };

        let icon = Self::volume_icon(volume, muted, has_device);
        let label = if has_device {
            format!("{}  {}%", icon, volume)
        } else {
            format!("{}  --", icon)
        };

        let (bg, border_c, fg) = if muted || !has_device {
            (
                color!(0x787878).scale_alpha(0.85),
                color!(0x888888),
                color!(0xEEEEEE),
            )
        } else {
            (
                color!(0x14B8A6).scale_alpha(0.9),
                color!(0x14B8A6),
                Color::WHITE,
            )
        };

        let pill = container(text(label).size(14).color(fg))
            .padding([3, 10])
            .height(Self::PILL_HEIGHT)
            .style(move |_theme: &Theme| Self::pill_style(bg, border_c, fg));

        mouse_area(pill)
            .on_press(Message::AudioToggleMute)
            .on_scroll(|delta| {
                let d = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y,
                    mouse::ScrollDelta::Pixels { y, .. } => y,
                };
                if d > 0.0 {
                    Message::AudioAdjust(5)
                } else {
                    Message::AudioAdjust(-5)
                }
            })
            .into()
    }

    fn screenshot_pill<'a>(&self) -> Element<'a, Message> {
        let is_hovered = self.is_hovered;
        let pill = container(text(ICON_SHOT.to_string()).size(15).color(Color::WHITE))
            .padding([3, 10])
            .height(Self::PILL_HEIGHT)
            .style(move |_theme: &Theme| {
                let bg = if is_hovered {
                    color!(0xFF8800).scale_alpha(0.95)
                } else {
                    color!(0x00CCCC).scale_alpha(0.90)
                };
                Self::pill_style(bg, bg, Color::WHITE)
            });

        mouse_area(pill)
            .on_enter(Message::MouseEnterScreenShot)
            .on_exit(Message::MouseExitScreenShot)
            .on_press(Message::LeftClick)
            .into()
    }

    fn time_pill<'a>(&self) -> Element<'a, Message> {
        let bg = color!(0x4DA3FF).scale_alpha(0.9);
        let pill = container(
            text(format!("{}  {}", ICON_TIME, self.formated_now))
                .size(14)
                .color(Color::WHITE),
        )
        .padding([3, 10])
        .height(Self::PILL_HEIGHT)
        .style(move |_theme: &Theme| Self::pill_style(bg, bg, Color::WHITE));

        mouse_area(pill).on_press(Message::ShowSecondsToggle).into()
    }

    fn monitor_pill<'a>(&self, monitor_num: i32) -> Element<'a, Message> {
        let bg = color!(0x9B59B6).scale_alpha(0.9);
        container(
            text(format!(
                "{}  {}",
                ICON_MON,
                Self::monitor_num_to_icon(monitor_num)
            ))
            .size(14)
            .color(Color::WHITE),
        )
        .padding([3, 10])
        .height(Self::PILL_HEIGHT)
        .style(move |_theme: &Theme| Self::pill_style(bg, bg, Color::WHITE))
        .into()
    }

    fn scale_pill<'a>(&self, scale: Option<f32>) -> Element<'a, Message> {
        let bg = color!(0x787878).scale_alpha(0.88);
        let label = match scale {
            Some(s) => format!("s: {:.2}", s),
            None => "s: --".to_string(),
        };
        container(text(label).size(14).color(Color::WHITE))
            .padding([3, 10])
            .height(Self::PILL_HEIGHT)
            .style(move |_theme: &Theme| Self::pill_style(bg, bg, Color::WHITE))
            .into()
    }

    fn layout_toggle_button<'a>(&self) -> iced::widget::Button<'a, Message> {
        let is_open = self.layout_selector_open;
        let color_open = color!(0x3CB371);
        let color_close = color!(0xD35400);

        let pill_color = if is_open { color_open } else { color_close };
        let label = self.layout_symbol.clone();

        button(rich_text![span(label).color(Color::WHITE)].on_link_click(std::convert::identity))
            .padding([3, 10])
            .height(Self::PILL_HEIGHT)
            .style(move |_theme: &Theme, status: button::Status| {
                let mut bg = pill_color.scale_alpha(0.85);
                let mut border_w = 1.0;

                if matches!(status, button::Status::Hovered) {
                    bg.a = 1.0;
                    border_w = 2.0;
                }

                button::Style {
                    background: Some(Background::Color(bg)),
                    text_color: Color::WHITE,
                    border: Border {
                        color: pill_color,
                        width: border_w,
                        radius: border::Radius::from(12.0),
                    },
                    ..Default::default()
                }
            })
            .on_press(Message::ToggleLayoutSelector)
    }

    fn layout_options_row(&self) -> Element<'_, Message> {
        let layouts: [(&str, u32); 3] = [("[]=", 0), ("><>", 1), ("[M]", 2)];
        let current = self.layout_symbol.as_str();

        let mut row = Row::new().spacing(6);
        for (sym, idx) in layouts {
            let is_current = sym == current;

            let btn = button(text(sym).color(Color::WHITE))
                .padding([3, 10])
                .height(Self::PILL_HEIGHT)
                .style(move |_theme: &Theme, status: button::Status| {
                    let base = if is_current {
                        color!(0x3CB371)
                    } else {
                        color!(0x4169E1)
                    };

                    let mut bg = base.scale_alpha(0.85);
                    let mut border_w = if is_current { 2.0 } else { 1.0 };

                    if matches!(status, button::Status::Hovered) {
                        bg.a = 1.0;
                        border_w += 1.0;
                    }

                    button::Style {
                        background: Some(Background::Color(bg)),
                        text_color: Color::WHITE,
                        border: Border {
                            color: base,
                            width: border_w,
                            radius: border::Radius::from(12.0),
                        },
                        ..Default::default()
                    }
                })
                .on_press(Message::LayoutClicked(idx));

            row = row.push(btn);
        }

        row.into()
    }

    fn view_work_space(&self) -> Element<'_, Message> {
        // Workspace tag buttons
        let mut tags_row = Row::new().spacing(Self::TAB_SPACING * 0.5);
        for (index, label) in self.tabs.iter().enumerate() {
            tags_row = tags_row
                .push(self.workspace_button(index, label))
                .align_y(iced::Alignment::Center);
        }

        let layout_button = self.layout_toggle_button();
        let layout_selector = if self.layout_selector_open {
            self.layout_options_row()
        } else {
            Row::new().into()
        };

        // System info pills
        let snapshot = self.system_monitor.get_snapshot();
        let cpu_usage = snapshot.map(|s| s.cpu_average).unwrap_or(0.0);
        let memory_usage = snapshot.map(|s| s.memory_usage_percent).unwrap_or(0.0);

        let cpu_pill = self.usage_pill(ICON_CPU, cpu_usage);
        let memory_pill = self.usage_pill(ICON_MEM, memory_usage);
        let battery_pill = self.battery_pill();

        let brightness_pill = self.brightness_pill();
        let volume_pill = self.volume_pill();
        let screenshot_pill = self.screenshot_pill();
        let time_pill = self.time_pill();

        let monitor_num = self
            .monitor_info_opt
            .as_ref()
            .map(|m| m.monitor_num)
            .unwrap_or(0);
        let monitor_pill = self.monitor_pill(monitor_num);

        let scale_pill = self.scale_pill(Some(self.scale_factor));

        Row::new()
            .push(tags_row)
            .push(Space::new().width(6).height(Length::Fill))
            .push(layout_button)
            .push(Space::new().width(6).height(Length::Fill))
            .push(layout_selector)
            .push(Space::new().width(Length::Fill).height(Length::Fill))
            .push(cpu_pill)
            .push(Space::new().width(6).height(Length::Fill))
            .push(memory_pill)
            .push(Space::new().width(6).height(Length::Fill))
            .push(battery_pill)
            .push(Space::new().width(8).height(Length::Fill))
            .push(brightness_pill)
            .push(Space::new().width(6).height(Length::Fill))
            .push(volume_pill)
            .push(Space::new().width(6).height(Length::Fill))
            .push(screenshot_pill)
            .push(Space::new().width(6).height(Length::Fill))
            .push(time_pill)
            .push(Space::new().width(6).height(Length::Fill))
            .push(monitor_pill)
            .push(Space::new().width(6).height(Length::Fill))
            .push(scale_pill)
            .align_y(iced::Alignment::Center)
            .into()
    }

    fn view(&self) -> Element<'_, Message> {
        let work_space_row = self.view_work_space();

        Column::new()
            .padding(4)
            .spacing(Self::TAB_SPACING)
            .push(work_space_row)
            .into()
    }
}
