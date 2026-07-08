use std::env;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::Local;
use futures::StreamExt;
use gpui::{
    App, Application, Bounds, Context, IntoElement, MouseButton, ParentElement, Pixels, Render,
    ScrollDelta, ScrollWheelEvent, SharedString, Styled, Task, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point, prelude::*,
    px, rgb, size,
};
use gpui_component::{
    Root, Selectable, Sizable, Size, black, blue_400, blue_500, cyan_500, emerald_500, emerald_600,
    gray_500, green_500, indigo_500, orange_500, red_500, rose_500, slate_100, slate_300,
    slate_700, slate_800, slate_900, tag::Tag, white,
};
use gpui_component::{
    button::{Button, ButtonCustomVariant, ButtonVariants},
    init as init_components,
};
use log::{error, info, warn};
use shared_structures::{CommandType, MonitorInfo, SharedCommand, SharedMessage, SharedRingBuffer};
use xbar_core::audio_manager::AudioManager;
use xbar_core::brightness::BrightnessManager;
use xbar_core::initialize_logging;
use xbar_core::system_monitor::SystemMonitor;

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

const TAG_ACCENTS: [u32; 9] = [
    0xEF4444, 0x14B8A6, 0x0EA5E9, 0x22C55E, 0xF59E0B, 0xEC4899, 0x3B82F6, 0x6366F1, 0x06B6D4,
];

struct GpuiComponentBar {
    active_tab: usize,
    shared_buffer: Option<Arc<SharedRingBuffer>>,
    monitor_info: Option<MonitorInfo>,
    formatted_now: String,
    show_seconds: bool,
    layout_symbol: String,
    monitor_num: i32,
    layout_selector_open: bool,
    audio_manager: AudioManager,
    system_monitor: SystemMonitor,
    brightness_manager: BrightnessManager,
    last_clock_update: Instant,
    last_monitor_update: Instant,
    stop_flag: Arc<AtomicBool>,
    _timer_task: Option<Task<()>>,
}

impl GpuiComponentBar {
    fn new(cx: &mut Context<Self>) -> Self {
        let shared_path = env::args().skip(1).last().unwrap_or_default();
        let shared_buffer =
            SharedRingBuffer::create_shared_ring_buffer_aux(&shared_path).map(Arc::new);

        let mut this = Self {
            active_tab: 0,
            shared_buffer,
            monitor_info: None,
            formatted_now: String::new(),
            show_seconds: true,
            layout_symbol: "[]=".to_string(),
            monitor_num: 0,
            layout_selector_open: false,
            audio_manager: AudioManager::new(),
            system_monitor: SystemMonitor::new(5),
            brightness_manager: BrightnessManager::new(),
            last_clock_update: Instant::now() - Duration::from_secs(2),
            last_monitor_update: Instant::now() - Duration::from_secs(3),
            stop_flag: Arc::new(AtomicBool::new(false)),
            _timer_task: None,
        };
        this.tick();
        this.spawn_clock(cx);
        this.spawn_shared_watcher(cx);
        this
    }

    fn spawn_clock(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let _ = this.update(cx, |this, cx| {
                    this.tick();
                    cx.notify();
                });
            }
        });
        self._timer_task = Some(task);
    }

    fn spawn_shared_watcher(&mut self, cx: &mut Context<Self>) {
        let Some(buf) = self.shared_buffer.clone() else {
            warn!("No shared buffer; skipping watcher thread");
            return;
        };

        let (tx, mut rx) = futures::channel::mpsc::channel::<SharedMessage>(64);
        let stop = self.stop_flag.clone();
        std::thread::spawn(move || {
            let mut previous_timestamp: u128 = 0;
            let mut tx = tx;
            while !stop.load(Ordering::Relaxed) {
                match buf.wait_for_message(Some(Duration::from_secs(2))) {
                    Ok(true) => {
                        if let Ok(Some(message)) = buf.try_read_latest_message() {
                            let ts = message.timestamp as u128;
                            if ts != previous_timestamp {
                                previous_timestamp = ts;
                                if tx.try_send(message).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Ok(false) => {}
                    Err(err) => {
                        warn!("wait_for_message failed: {err}");
                        break;
                    }
                }
            }
        });

        cx.spawn(async move |this, cx| {
            while let Some(message) = rx.next().await {
                let _ = this.update(cx, |this, cx| {
                    this.apply_shared(message);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn tick(&mut self) {
        if self.last_clock_update.elapsed() >= Duration::from_millis(900) {
            let fmt = if self.show_seconds {
                "%m-%d %H:%M:%S"
            } else {
                "%m-%d %H:%M"
            };
            self.formatted_now = Local::now().format(fmt).to_string();
            self.last_clock_update = Instant::now();
        }

        if self.last_monitor_update.elapsed() >= Duration::from_secs(2) {
            self.system_monitor.update_if_needed();
            self.audio_manager.update_if_needed();
            self.brightness_manager.update_if_needed();
            self.last_monitor_update = Instant::now();
        }
    }

    fn apply_shared(&mut self, message: SharedMessage) {
        self.monitor_info = Some(message.monitor_info);
        if let Some(monitor) = &self.monitor_info {
            self.layout_symbol = monitor.get_ltsymbol();
            self.monitor_num = monitor.monitor_num;
            for (index, status) in monitor.tag_status_vec.iter().enumerate() {
                if status.is_selected {
                    self.active_tab = index;
                }
            }
        }
    }

    fn send_tag_command(&mut self, is_view: bool) {
        let tag_bit = 1 << self.active_tab;
        let command = if is_view {
            SharedCommand::view_tag(tag_bit, self.monitor_num)
        } else {
            SharedCommand::toggle_tag(tag_bit, self.monitor_num)
        };

        if let Some(buf) = &self.shared_buffer {
            match buf.send_command(command) {
                Ok(true) => info!("Sent command: {:?}", command),
                Ok(false) => warn!("Command buffer full, command dropped"),
                Err(err) => error!("Failed to send command: {err}"),
            }
        }
    }

    fn send_layout_command(&mut self, layout_index: u32) {
        let command = SharedCommand::new(CommandType::SetLayout, layout_index, self.monitor_num);
        if let Some(buf) = &self.shared_buffer {
            let _ = buf.send_command(command);
        }
    }

    fn workspace_button(&self, index: usize, cx: &App) -> Button {
        let accent = TAG_ACCENTS[index];
        let label = TAG_ICONS[index];

        let (color, border, foreground, hover, selected) = if let Some(monitor) = &self.monitor_info
        {
            if let Some(status) = monitor.tag_status_vec.get(index) {
                if status.is_urg {
                    (red_500(), rose_500(), white(), rose_500(), true)
                } else if status.is_filled {
                    (
                        rgb(accent).into(),
                        rgb(accent).into(),
                        white(),
                        rgb(accent).into(),
                        true,
                    )
                } else if status.is_selected {
                    (
                        rgb(accent).into(),
                        rgb(accent).into(),
                        white(),
                        rgb(accent).into(),
                        true,
                    )
                } else if status.is_occ {
                    (
                        slate_800(),
                        rgb(accent).into(),
                        slate_100(),
                        slate_700(),
                        false,
                    )
                } else {
                    (slate_900(), slate_700(), slate_300(), slate_800(), false)
                }
            } else {
                (slate_900(), slate_700(), slate_300(), slate_800(), false)
            }
        } else {
            (slate_900(), slate_700(), slate_300(), slate_800(), false)
        };

        Button::new(SharedString::from(format!("tag-{index}")))
            .label(label)
            .compact()
            .rounded(px(12.))
            .selected(selected)
            .custom(
                ButtonCustomVariant::new(cx)
                    .color(color)
                    .border(border)
                    .foreground(foreground)
                    .hover(hover)
                    .active(border),
            )
    }

    fn chip_button(
        id: &'static str,
        label: impl Into<SharedString>,
        color: gpui::Hsla,
        border: gpui::Hsla,
        foreground: gpui::Hsla,
        cx: &App,
    ) -> Button {
        Button::new(id)
            .label(label)
            .compact()
            .rounded(px(999.))
            .custom(
                ButtonCustomVariant::new(cx)
                    .color(color)
                    .border(border)
                    .foreground(foreground)
                    .hover(border)
                    .active(border),
            )
    }

    fn chip_tag(
        &self,
        label: impl Into<SharedString>,
        color: gpui::Hsla,
        foreground: gpui::Hsla,
        border: gpui::Hsla,
    ) -> impl IntoElement {
        Tag::custom(color, foreground, border)
            .rounded_full()
            .with_size(Size::Small)
            .child(label.into())
    }

    fn render_workspaces(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div().flex().flex_row().items_center().gap(px(6.));
        for index in 0..TAG_ICONS.len() {
            row = row.child(self.workspace_button(index, cx).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.active_tab = index;
                    this.send_tag_command(true);
                    cx.notify();
                },
            )));
        }
        row
    }

    fn render_layouts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let toggle = Self::chip_button(
            "layout-toggle",
            self.layout_symbol.clone(),
            orange_500(),
            orange_500(),
            white(),
            cx,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.layout_selector_open = !this.layout_selector_open;
            cx.notify();
        }));

        let mut root = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(toggle);
        if self.layout_selector_open {
            let options = [("[]=", 0_u32), ("><>", 1_u32), ("[M]", 2_u32)];
            let mut row = div().flex().flex_row().items_center().gap(px(4.));
            for (symbol, layout_index) in options {
                let selected = symbol == self.layout_symbol;
                let button = Self::chip_button(
                    "layout-option",
                    symbol,
                    if selected {
                        emerald_500()
                    } else {
                        indigo_500()
                    },
                    if selected {
                        emerald_600()
                    } else {
                        indigo_500()
                    },
                    white(),
                    cx,
                )
                .selected(selected)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.send_layout_command(layout_index);
                    this.layout_selector_open = false;
                    cx.notify();
                }));
                row = row.child(button);
            }
            root = root.child(row);
        }
        root
    }

    fn render_usage_pills(&self) -> impl IntoElement {
        let snapshot = self.system_monitor.get_snapshot();
        let cpu = snapshot.map(|s| s.cpu_average).unwrap_or(0.0);
        let mem = snapshot.map(|s| s.memory_usage_percent).unwrap_or(0.0);

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(self.usage_chip(ICON_CPU, cpu))
            .child(self.usage_chip(ICON_MEM, mem))
            .child(self.battery_chip())
    }

    fn usage_chip(&self, icon: &str, value: f32) -> impl IntoElement {
        let (bg, fg) = if value <= 30.0 {
            (green_500(), white())
        } else if value <= 60.0 {
            (rgb(0xFACC15).into(), black())
        } else if value <= 80.0 {
            (orange_500(), white())
        } else {
            (red_500(), white())
        };
        self.chip_tag(format!("{icon}  {value:.0}%"), bg, fg, bg)
    }

    fn battery_chip(&self) -> impl IntoElement {
        let (percent, charging) = self
            .system_monitor
            .get_snapshot()
            .map(|snapshot| (snapshot.battery_percent, snapshot.is_charging))
            .unwrap_or((0.0, false));
        let icon = if charging {
            ICON_BAT_CHG
        } else {
            ICON_BAT_FULL
        };
        let (bg, fg) = if percent > 50.0 {
            (green_500(), white())
        } else if percent > 20.0 {
            (rgb(0xFACC15).into(), black())
        } else {
            (red_500(), white())
        };
        self.chip_tag(format!("{icon}  {percent:.0}%"), bg, fg, bg)
    }

    fn render_interactive_pills(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let brightness = match self.brightness_manager.percent() {
            Some(value) => format!("{ICON_BRIGHT}  {value}%"),
            None => format!("{ICON_BRIGHT}  --"),
        };

        let master = self.audio_manager.get_master_device();
        let (volume, muted, has_device) = if let Some(device) = master {
            (device.volume.clamp(0, 100), device.is_muted, true)
        } else {
            (0, true, false)
        };
        let volume_icon = volume_icon(volume, muted, has_device);
        let volume_label = if has_device {
            format!("{volume_icon}  {volume}%")
        } else {
            format!("{volume_icon}  --")
        };

        let time = format!("{ICON_TIME}  {}", self.formatted_now);
        let monitor = format!("{ICON_MON}  {}", monitor_num_to_icon(self.monitor_num));

        let brightness_chip = div()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.brightness_manager.adjust(5);
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    this.brightness_manager.adjust(-5);
                    cx.notify();
                }),
            )
            .child(Self::chip_button(
                "brightness",
                brightness,
                rgb(0xEAB308).into(),
                rgb(0xCA8A04).into(),
                slate_900(),
                cx,
            ));

        let volume_chip = div()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if let Some(device) = this.audio_manager.get_master_device().cloned() {
                        let _ = this.audio_manager.toggle_mute(&device.name);
                    }
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let delta_y = match event.delta {
                    ScrollDelta::Pixels(delta) => f32::from(delta.y),
                    ScrollDelta::Lines(delta) => delta.y,
                };

                if delta_y == 0.0 {
                    return;
                }

                if let Some(device) = this.audio_manager.get_master_device().cloned() {
                    let step = if delta_y > 0.0 { 5 } else { -5 };
                    let new_volume = (device.volume + step).clamp(0, 100);
                    let _ =
                        this.audio_manager
                            .set_volume(&device.name, new_volume, device.is_muted);
                }

                cx.notify();
            }))
            .child(Self::chip_button(
                "volume",
                volume_label,
                if muted || !has_device {
                    gray_500()
                } else {
                    cyan_500()
                },
                if muted || !has_device {
                    slate_700()
                } else {
                    cyan_500()
                },
                white(),
                cx,
            ));

        let screenshot_chip =
            Self::chip_button("screenshot", ICON_SHOT, cyan_500(), blue_500(), white(), cx)
                .on_click(cx.listener(|_, _, _, _| {
                    if let Err(err) = Command::new("flameshot").arg("gui").spawn() {
                        warn!("Failed to spawn flameshot: {err}");
                    }
                }));

        let time_chip = Self::chip_button("time", time, blue_400(), blue_500(), white(), cx)
            .on_click(cx.listener(|this, _, _, cx| {
                this.show_seconds = !this.show_seconds;
                this.tick();
                cx.notify();
            }));

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(brightness_chip)
            .child(volume_chip)
            .child(screenshot_chip)
            .child(time_chip)
            .child(self.chip_tag(monitor, indigo_500(), white(), indigo_500()))
            .child(self.chip_tag("s: 1.00", slate_700(), slate_100(), slate_700()))
    }
}

impl Drop for GpuiComponentBar {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}

impl Render for GpuiComponentBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let left = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .child(self.render_workspaces(cx))
            .child(div().w(px(1.)).h(px(24.)).bg(slate_700()))
            .child(self.render_layouts(cx));

        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .child(self.render_usage_pills())
            .child(div().w(px(1.)).h(px(24.)).bg(slate_700()))
            .child(self.render_interactive_pills(cx));

        div()
            .size_full()
            .px(px(10.))
            .py(px(6.))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .bg(slate_900())
            .border_b_1()
            .border_color(slate_800())
            .font_family(NERD_FONT)
            .text_color(white())
            .child(left)
            .child(right)
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

fn monitor_num_to_icon(monitor_num: i32) -> String {
    match monitor_num {
        0 => ICON_M0.to_string(),
        1 => ICON_M1.to_string(),
        other => format!("M{other}"),
    }
}

fn main() {
    let shared_path = env::args().skip(1).last().unwrap_or_default();
    let _ = initialize_logging("gpui_component_bar", &shared_path);

    Application::new().run(|cx: &mut App| {
        init_components(cx);
        let height: Pixels = px(42.);
        let width: Pixels = cx
            .primary_display()
            .map(|display| display.bounds().size.width)
            .unwrap_or(px(1920.));

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(0.), px(0.)),
                size: size(width, height),
            })),
            titlebar: None,
            window_background: WindowBackgroundAppearance::Transparent,
            kind: WindowKind::Normal,
            is_resizable: false,
            is_minimizable: false,
            window_min_size: Some(size(width, height)),
            app_id: Some("gpui_component_bar".into()),
            ..Default::default()
        };

        cx.open_window(options, |window, cx| {
            let view = cx.new(GpuiComponentBar::new);
            cx.new(|cx| Root::new(view, window, cx))
        })
        .expect("failed to open gpui_component_bar window");
        cx.activate(true);
    });
}
