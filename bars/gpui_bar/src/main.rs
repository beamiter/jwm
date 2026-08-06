// gpui_bar — gpui port of iced_bar.
//
// Feature parity with iced_bar:
//   * 9 nerd-font workspace tag buttons with selected/filled/urgent/occupied visuals
//   * Layout toggle + 3-option selector
//   * Pills: CPU, memory, battery, brightness, volume, screenshot, time, monitor, scale
//   * Click semantics: tag → view-tag command; volume left-click mute + wheel adjust; brightness left/right click;
//     screenshot pill spawns `flameshot gui`; clock toggles seconds
//   * A nonblocking transport poller reconnects after WM restarts; a 1Hz timer
//     drives the core runtime
//
// gpui's render model: a `Render` impl returns an `Element` tree built from
// `div()` with tailwind-like styling. State mutation happens via
// `cx.listener(|this, ev, window, cx| { ... cx.notify(); })`.

use std::env;
use std::time::Duration;

use log::{debug, warn};

use xbar_core::logging::init as initialize_logging;
use xbar_core::{
    BarEffect, BarRuntime, LayoutId, ModelConfig, MonitorGeometry, PlatformEffectHandler,
    RuntimeUpdate, ShellRoute, TagId, TransportRecoveryConfig, UserAction,
};
use xbar_linux_actions::ProcessActionHandler;

use gpui::{
    App, Bounds, Context, IntoElement, MouseButton, ParentElement, Pixels, Render, Rgba,
    ScrollDelta, ScrollWheelEvent, SharedString, Styled, Task, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point, prelude::*,
    px, size,
};
use gpui_platform::application;
use std::sync::Arc;
use x11rb::connection::Connection as _;
use x11rb::xcb_ffi::XCBConnection;
use xbar_core::glass::{DEFAULT_BACKGROUND_OPACITY, fallback_rgb};

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

const ICON_CPU: &str = "\u{F4BC}";
const ICON_MEM: &str = "\u{F035B}";
const ICON_BAT_FULL: &str = "\u{F0079}";
const ICON_BAT_CHG: &str = "\u{F0084}";
const ICON_VOL_HIGH: &str = "\u{F057E}";
const ICON_VOL_MID: &str = "\u{F0580}";
const ICON_VOL_LOW: &str = "\u{F057F}";
const ICON_VOL_MUTE: &str = "\u{F075F}";
const ICON_BRIGHT: &str = "\u{F00DE}";
const ICON_SHELL: &str = "\u{F0F2A}";
const ICON_SHOT: &str = "\u{F0104}";
const ICON_TIME: &str = "\u{F0954}";
const ICON_MON: &str = "\u{F0379}";
const ICON_M0: &str = "\u{F02DA}";
const ICON_M1: &str = "\u{F02DB}";

const TAG_COLORS: [u32; 9] = [
    0xFF6B6B, 0x4ECDC4, 0x45B7D1, 0x96CEB4, 0xFECA57, 0xFF9FF3, 0x54A0FF, 0x5F27CD, 0x00D2D3,
];
const TRANSPORT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

// -------- App state ----------------------------------------------------------

struct GpuiBar {
    runtime: BarRuntime,
    process_actions: ProcessActionHandler,
    active_geometry: Option<MonitorGeometry>,
    default_size: Option<gpui::Size<Pixels>>,
    last_scale_factor: Option<f32>,
    geometry_dirty: bool,
    _timer_task: Option<Task<()>>,
    _transport_task: Option<Task<()>>,

    // --- Compositor coupling ---
    /// The one background wash this bar paints: the shared fallback color at
    /// the configured opacity when a compositor blends the bar into the
    /// desktop, fully opaque when nothing would.
    background: Rgba,
}

impl GpuiBar {
    fn new(background: Rgba, cx: &mut Context<Self>) -> Self {
        let args: Vec<String> = env::args().collect();
        let shared_path = args.iter().skip(1).last().cloned().unwrap_or_default();
        let config = ModelConfig {
            show_seconds: true,
            clock_minute_format: "%m-%d %H:%M".into(),
            clock_second_format: "%m-%d %H:%M:%S".into(),
            ..ModelConfig::default()
        };
        let runtime = if shared_path.is_empty() {
            BarRuntime::new(config)
        } else {
            let recovery = TransportRecoveryConfig::new(shared_path, TRANSPORT_RETRY_INTERVAL)
                .expect("static transport recovery config is valid");
            BarRuntime::with_managed_transport(config, recovery)
        }
        .expect("gpui bar model configuration is valid");

        let mut this = Self {
            runtime,
            process_actions: ProcessActionHandler::default(),
            active_geometry: None,
            background,
            default_size: None,
            last_scale_factor: None,
            geometry_dirty: false,
            _timer_task: None,
            _transport_task: None,
        };
        this.spawn_clock(cx);
        this.spawn_transport_poller(cx);
        this
    }

    fn spawn_clock(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let _ = this.update(cx, |this, cx| {
                    let update = this.runtime.tick();
                    this.handle_runtime_update(update);
                    cx.notify();
                });
            }
        });
        self._timer_task = Some(task);
    }

    fn spawn_transport_poller(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(TRANSPORT_POLL_INTERVAL)
                    .await;
                let _ = this.update(cx, |this, cx| {
                    let update = this.runtime.poll_transport();
                    this.handle_runtime_update(update);
                    cx.notify();
                });
            }
        });
        self._transport_task = Some(task);
    }

    fn dispatch(&mut self, action: UserAction) {
        let update = self.runtime.dispatch(action);
        self.handle_runtime_update(update);
    }

    fn dispatch_wm(&mut self, action: UserAction) {
        if !self.runtime.view().wm_available {
            debug!("ignoring WM action while the WM projection is unavailable");
            return;
        }
        self.dispatch(action);
    }

    fn handle_runtime_update(&mut self, update: RuntimeUpdate) {
        for issue in update.issues {
            warn!("xbar runtime issue: {issue:?}");
        }
        for effect in update.platform_effects {
            match effect {
                BarEffect::ApplyMonitorGeometry(geometry) => {
                    self.active_geometry = Some(geometry);
                    self.geometry_dirty = true;
                }
                BarEffect::ClearMonitorGeometry => {
                    self.active_geometry = None;
                    self.geometry_dirty = true;
                }
                effect @ (BarEffect::Screenshot | BarEffect::OpenAudioControl) => {
                    if let Err(error) = self.process_actions.handle(effect) {
                        warn!("failed to handle platform effect: {error}");
                    }
                }
                unhandled => warn!("unhandled xbar platform effect: {unhandled:?}"),
            }
        }
    }
}

// -------- Color helpers ------------------------------------------------------

fn rgba_alpha(hex: u32, alpha: f32) -> Rgba {
    let r = ((hex >> 16) & 0xFF) as f32 / 255.0;
    let g = ((hex >> 8) & 0xFF) as f32 / 255.0;
    let b = (hex & 0xFF) as f32 / 255.0;
    Rgba { r, g, b, a: alpha }
}

fn usage_colors(u: f32) -> (Rgba, Rgba) {
    if u <= 30.0 {
        (rgba_alpha(0x1FBF51, 0.9), rgba_alpha(0xFFFFFF, 1.0))
    } else if u <= 60.0 {
        (rgba_alpha(0xF4C20D, 0.9), rgba_alpha(0x000000, 1.0))
    } else if u <= 80.0 {
        (rgba_alpha(0xFF8C1A, 0.9), rgba_alpha(0xFFFFFF, 1.0))
    } else {
        (rgba_alpha(0xE53935, 0.9), rgba_alpha(0xFFFFFF, 1.0))
    }
}

fn battery_colors(pct: f32) -> (Rgba, Rgba) {
    if pct > 50.0 {
        (rgba_alpha(0x1FBF51, 0.9), rgba_alpha(0xFFFFFF, 1.0))
    } else if pct > 20.0 {
        (rgba_alpha(0xF4C20D, 0.9), rgba_alpha(0x000000, 1.0))
    } else {
        (rgba_alpha(0xE53935, 0.9), rgba_alpha(0xFFFFFF, 1.0))
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

// -------- Render -------------------------------------------------------------

impl GpuiBar {
    fn render_tag(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let icon = TAG_ICONS[index];
        let tag_color = TAG_COLORS[index];

        let view = self.runtime.view();
        let (bg, border_c, is_active) = if view.wm_available
            && let Some(s) = view.tags.get(index)
        {
            if s.urgent {
                (rgba_alpha(0xDB3645, 1.0), rgba_alpha(0xBC2130, 1.0), true)
            } else if s.filled {
                (rgba_alpha(tag_color, 1.0), rgba_alpha(tag_color, 1.0), true)
            } else if s.selected {
                (rgba_alpha(tag_color, 0.7), rgba_alpha(tag_color, 1.0), true)
            } else if s.occupied {
                (
                    rgba_alpha(tag_color, 0.3),
                    rgba_alpha(tag_color, 0.6),
                    false,
                )
            } else {
                (rgba_alpha(0xFFFFFF, 0.9), rgba_alpha(0xDEE2E6, 1.0), false)
            }
        } else {
            (rgba_alpha(0xFFFFFF, 0.9), rgba_alpha(0xDEE2E6, 1.0), false)
        };

        let text_color = if is_active {
            if index == 4 {
                rgba_alpha(0x333333, 1.0)
            } else {
                rgba_alpha(0xFFFFFF, 1.0)
            }
        } else {
            rgba_alpha(0x333333, 1.0)
        };

        div()
            .id(SharedString::from(format!("tag-{}", index)))
            .w(px(30.))
            .h(px(26.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .bg(bg)
            .border_color(border_c)
            .border_2()
            .text_color(text_color)
            .text_size(px(11.))
            .child(icon)
            .hover(|s| s.opacity(0.85))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev, _window, cx| {
                    if let Some(tag) = TagId::new(index) {
                        this.dispatch_wm(UserAction::ViewTag(tag));
                    }
                    cx.notify();
                }),
            )
    }

    fn render_pill(
        &self,
        id: &'static str,
        bg: Rgba,
        border_c: Rgba,
        fg: Rgba,
        content: impl Into<SharedString>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .h(px(22.))
            .px(px(7.))
            .py(px(2.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(10.))
            .bg(bg)
            .border_1()
            .border_color(border_c)
            .text_color(fg)
            .text_size(px(11.))
            .child(content.into())
    }

    fn render_usage_pill(&self, id: &'static str, icon: &str, value: f32) -> impl IntoElement {
        let (bg, fg) = usage_colors(value);
        self.render_pill(id, bg, bg, fg, format!("{}  {:.0}%", icon, value))
    }

    fn render_battery_pill(&self) -> impl IntoElement {
        let battery = self.runtime.view().battery;
        let pct = battery.percent.map_or(100.0, |value| value.as_f32());
        let charging = battery.charging;
        let icon = if charging {
            ICON_BAT_CHG
        } else {
            ICON_BAT_FULL
        };
        let (bg, fg) = battery_colors(pct);
        self.render_pill("battery", bg, bg, fg, format!("{}  {:.0}%", icon, pct))
    }

    fn render_brightness_pill(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let label = match self.runtime.view().brightness.percent {
            Some(percent) => format!("{}  {}%", ICON_BRIGHT, percent.rounded()),
            None => format!("{}  --", ICON_BRIGHT),
        };
        let bg = rgba_alpha(0xFDE047, 0.92);
        let border = rgba_alpha(0xFACC15, 1.0);
        let fg = rgba_alpha(0x1F2937, 1.0);

        div()
            .id("brightness")
            .h(px(22.))
            .px(px(7.))
            .py(px(2.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(10.))
            .bg(bg)
            .border_1()
            .border_color(border)
            .text_color(fg)
            .text_size(px(11.))
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.dispatch(UserAction::BrightnessUp);
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _ev, _w, cx| {
                    this.dispatch(UserAction::BrightnessDown);
                    cx.notify();
                }),
            )
    }

    fn render_volume_pill(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let audio = self.runtime.view().audio;
        let (vol, has_dev) = audio
            .volume_percent
            .map_or((0, false), |percent| (i32::from(percent.rounded()), true));
        let muted = audio.muted;
        let icon = volume_icon(vol, muted, has_dev);
        let label = if has_dev {
            format!("{}  {}%", icon, vol)
        } else {
            format!("{}  --", icon)
        };
        let (bg, border, fg) = if muted || !has_dev {
            (
                rgba_alpha(0x787878, 0.85),
                rgba_alpha(0x888888, 1.0),
                rgba_alpha(0xEEEEEE, 1.0),
            )
        } else {
            (
                rgba_alpha(0x14B8A6, 0.9),
                rgba_alpha(0x14B8A6, 1.0),
                rgba_alpha(0xFFFFFF, 1.0),
            )
        };

        div()
            .id("volume")
            .h(px(22.))
            .px(px(7.))
            .py(px(2.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(10.))
            .bg(bg)
            .border_1()
            .border_color(border)
            .text_color(fg)
            .text_size(px(11.))
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.dispatch(UserAction::ToggleMute);
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _w, cx| {
                let delta_y = match event.delta {
                    ScrollDelta::Pixels(delta) => f32::from(delta.y),
                    ScrollDelta::Lines(delta) => delta.y,
                };

                if delta_y == 0.0 {
                    return;
                }

                let action = if delta_y > 0.0 {
                    UserAction::VolumeUp
                } else {
                    UserAction::VolumeDown
                };
                this.dispatch(action);

                cx.notify();
            }))
    }

    /// Entry point into JWM's own shell surface.
    ///
    /// One pill: it opens the hub, and the hub is itself the page that routes
    /// to applications, notifications, clipboard, calendar and wallpaper. The
    /// bar renders none of that content and keeps no shell state.
    fn render_shell_pill(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        // Grayed rather than hidden: the shell lives in the window manager, so
        // an unreachable one has to look unreachable.
        let available = self.runtime.view().wm_available;
        let bg = if available {
            rgba_alpha(0x7C6CFF, 0.90)
        } else {
            rgba_alpha(0x555B66, 0.70)
        };
        let hover_bg = if available {
            rgba_alpha(0x9688FF, 0.95)
        } else {
            bg
        };
        div()
            .id("shell")
            .h(px(22.))
            .px(px(7.))
            .py(px(2.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(10.))
            .bg(bg)
            .border_1()
            .border_color(bg)
            .text_color(rgba_alpha(0xFFFFFF, if available { 1.0 } else { 0.55 }))
            .text_size(px(12.))
            .child(ICON_SHELL)
            .hover(move |s| s.bg(hover_bg).border_color(hover_bg))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    if this.runtime.view().wm_available {
                        this.dispatch(UserAction::OpenShellHub(ShellRoute::Hub));
                        cx.notify();
                    }
                }),
            )
    }

    fn render_screenshot_pill(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let bg = rgba_alpha(0x00CCCC, 0.90);
        let hover_bg = rgba_alpha(0xFF8800, 0.95);
        div()
            .id("screenshot")
            .h(px(22.))
            .px(px(7.))
            .py(px(2.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(10.))
            .bg(bg)
            .border_1()
            .border_color(bg)
            .text_color(rgba_alpha(0xFFFFFF, 1.0))
            .text_size(px(12.))
            .child(ICON_SHOT)
            .hover(move |s| s.bg(hover_bg).border_color(hover_bg))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.dispatch(UserAction::Screenshot);
                    cx.notify();
                }),
            )
    }

    fn render_time_pill(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let bg = rgba_alpha(0x4DA3FF, 0.9);
        let label = format!("{}  {}", ICON_TIME, self.runtime.view().time);
        div()
            .id("time")
            .h(px(22.))
            .px(px(7.))
            .py(px(2.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(10.))
            .bg(bg)
            .border_1()
            .border_color(bg)
            .text_color(rgba_alpha(0xFFFFFF, 1.0))
            .text_size(px(11.))
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.dispatch(UserAction::ToggleSeconds);
                    cx.notify();
                }),
            )
    }

    fn render_layout_button(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let open = self.runtime.view().layout_selector_open;
        let pill_color = if open { 0x3CB371 } else { 0xD35400 };
        let bg = rgba_alpha(pill_color, 0.85);
        let border = rgba_alpha(pill_color, 1.0);
        div()
            .id("layout-toggle")
            .h(px(22.))
            .px(px(7.))
            .py(px(2.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(10.))
            .bg(bg)
            .border_1()
            .border_color(border)
            .text_color(rgba_alpha(0xFFFFFF, 1.0))
            .text_size(px(11.))
            .child(self.runtime.view().layout_symbol.to_owned())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| {
                    this.dispatch(UserAction::ToggleLayoutSelector);
                    cx.notify();
                }),
            )
    }

    fn render_layout_options(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let opts: [(&str, u32); 3] = [("[]=", 0), ("><>", 1), ("[M]", 2)];
        let current = self.runtime.view().layout_symbol.to_owned();

        let mut row = div().flex().flex_row().gap(px(3.));
        for (sym, idx) in opts {
            let is_current = sym == current.as_str();
            let base = if is_current { 0x3CB371 } else { 0x4169E1 };
            let bg = rgba_alpha(base, 0.85);
            let border = rgba_alpha(base, 1.0);
            let item = div()
                .id(SharedString::from(format!("layout-opt-{}", idx)))
                .h(px(26.))
                .px(px(10.))
                .py(px(3.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(10.))
                .bg(bg)
                .border_1()
                .border_color(border)
                .text_color(rgba_alpha(0xFFFFFF, 1.0))
                .text_size(px(11.))
                .child(sym)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev, _w, cx| {
                        this.dispatch_wm(UserAction::SetLayout(LayoutId(idx)));
                        cx.notify();
                    }),
                );
            row = row.child(item);
        }
        row
    }

    fn render_monitor_pill(&self) -> impl IntoElement {
        let bg = rgba_alpha(0x9B59B6, 0.9);
        self.render_pill(
            "monitor",
            bg,
            bg,
            rgba_alpha(0xFFFFFF, 1.0),
            format!(
                "{}  {}",
                ICON_MON,
                monitor_num_to_icon(self.runtime.view().monitor.0)
            ),
        )
    }

    fn render_scale_pill(&self) -> impl IntoElement {
        let bg = rgba_alpha(0x787878, 0.88);
        self.render_pill(
            "scale",
            bg,
            bg,
            rgba_alpha(0xFFFFFF, 1.0),
            "s: 1.00".to_string(),
        )
    }
}

impl Render for GpuiBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scale_factor = window.scale_factor().max(f32::EPSILON);
        self.default_size
            .get_or_insert_with(|| window.bounds().size);
        let scale_changed = self
            .last_scale_factor
            .is_none_or(|previous| previous.to_bits() != scale_factor.to_bits());
        if self.geometry_dirty || scale_changed {
            let default_size = self.default_size.expect("default size is initialized");
            let target_size = self.active_geometry.map_or(default_size, |geometry| {
                size(
                    px(geometry.width as f32 / scale_factor),
                    default_size.height,
                )
            });
            // GPUI has no public window-position API. JWM remains responsible
            // for applying geometry.x/y; this frontend applies the logical size.
            window.resize(target_size);
            self.geometry_dirty = false;
            self.last_scale_factor = Some(scale_factor);
        }
        let system = self.runtime.view().system;
        let cpu = system.cpu_percent.map_or(0.0, |value| value.as_f32());
        let mem = system.memory_percent.map_or(0.0, |value| value.as_f32());

        // Workspace row
        let mut tags = div().flex().flex_row().gap(px(2.));
        for i in 0..9 {
            tags = tags.child(self.render_tag(i, cx));
        }

        // Layout selector area
        let layout_btn = self.render_layout_button(cx);
        let layout_options_el = if self.runtime.view().layout_selector_open {
            Some(self.render_layout_options(cx))
        } else {
            None
        };

        // Right side pills
        let right_pills = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .child(self.render_usage_pill("cpu", ICON_CPU, cpu))
            .child(self.render_usage_pill("mem", ICON_MEM, mem))
            .child(self.render_battery_pill())
            .child(self.render_brightness_pill(cx))
            .child(self.render_volume_pill(cx))
            .child(self.render_shell_pill(cx))
            .child(self.render_screenshot_pill(cx))
            .child(self.render_time_pill(cx))
            .child(self.render_monitor_pill())
            .child(self.render_scale_pill());

        // Left cluster: tags + spacing + layout toggle (+ optional options)
        let mut left = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .child(tags)
            .child(layout_btn);
        if let Some(opts) = layout_options_el {
            left = left.child(opts);
        }

        let root = div()
            .relative()
            .w_full()
            .h_full()
            .overflow_hidden()
            .font_family(NERD_FONT)
            .text_color(rgba_alpha(0xFFFFFF, 1.0));
        root.child(
            div()
                .relative()
                .w_full()
                .h_full()
                .p(px(2.))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .bg(self.background)
                .child(left)
                .child(right_pills),
        )
    }
}

// -------- main ---------------------------------------------------------------

fn main() {
    let args: Vec<String> = env::args().collect();
    let shared_path = args.iter().skip(1).last().cloned().unwrap_or_default();
    let _ = initialize_logging("gpui_bar", &shared_path);

    // Only `theme` and `background_opacity` of the shared bar config apply
    // here; the rest of this bar's appearance is its own theme.
    let config = xbar_core::config::BarConfig::load_default().unwrap_or_else(|error| {
        warn!("falling back to the default bar config: {error}");
        xbar_core::config::BarConfig::default()
    });

    let translucent = detect_translucency();
    let [red, green, blue] = fallback_rgb(config.theme);
    let opacity = if translucent {
        config.background_opacity.unwrap_or(DEFAULT_BACKGROUND_OPACITY)
    } else {
        1.0
    };
    let background = Rgba {
        r: f32::from(red) / 255.0,
        g: f32::from(green) / 255.0,
        b: f32::from(blue) / 255.0,
        a: opacity as f32,
    };

    application().run(move |cx: &mut App| {
        // Match JWM's configured status_bar_height; width spans the primary
        // display so the bar covers the screen until JWM repositions it.
        let height: Pixels = px(42.);
        let width: Pixels = cx
            .primary_display()
            .map(|d| d.bounds().size.width)
            .unwrap_or(px(1920.));
        let bounds = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(width, height),
        };

        let opts = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            window_background: if translucent {
                WindowBackgroundAppearance::Transparent
            } else {
                WindowBackgroundAppearance::Opaque
            },
            kind: WindowKind::Normal,
            is_resizable: false,
            is_minimizable: false,
            // WM_CLASS = "gpui_bar" — JWM detects this as its status bar
            // (see config_x11.toml: [status_bar] name = "gpui_bar").
            app_id: Some("gpui_bar".into()),
            ..Default::default()
        };

        cx.open_window(opts, move |_w, cx| {
            cx.new(move |cx| GpuiBar::new(background, cx))
        })
        .expect("failed to open window");
        cx.activate(true);
    });
}

// -------- Compositor coupling ------------------------------------------------

/// The startup mode decision: translucent only when a compositing manager is
/// running AND gpui's renderer can actually deliver per-pixel alpha; anything
/// less paints the solid fallback. Both checks ride one side connection that
/// is gone before gpui opens its own.
fn detect_translucency() -> bool {
    if x11rb::xcb_ffi::load_libxcb().is_err() {
        return false;
    }
    // No X server to ask (a native-Wayland session, say) means nobody honors
    // the CM-selection contract either way: solid.
    let Ok((conn, screen_num)) = XCBConnection::connect(None) else {
        return false;
    };
    compositor_active(&conn, screen_num) && renderer_alpha_capable(&conn, screen_num)
}

/// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
/// selection.
///
/// Sampled once, before gpui runs: per-pixel alpha is a window-creation
/// decision, so a compositor started or stopped later goes unnoticed until
/// the bar restarts. Ownership also only promises compositing — whether the
/// compositor blurs behind the bar is its own affair.
fn compositor_active(conn: &XCBConnection, screen_num: usize) -> bool {
    use x11rb::protocol::xproto::ConnectionExt as _;

    let name = format!("_NET_WM_CM_S{screen_num}");
    let Ok(cookie) = conn.intern_atom(false, name.as_bytes()) else {
        return false;
    };
    let Ok(atom) = cookie.reply() else {
        return false;
    };
    conn.get_selection_owner(atom.atom)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| reply.owner != x11rb::NONE)
        .unwrap_or(false)
}

/// A 32-bit TrueColor visual to probe with, if the server has one.
fn find_argb_visual(screen: &x11rb::protocol::xproto::Screen) -> Option<u32> {
    use x11rb::protocol::xproto::VisualClass;

    screen
        .allowed_depths
        .iter()
        .find(|depth| depth.depth == 32)
        .and_then(|depth| {
            depth
                .visuals
                .iter()
                .find(|visual| visual.class == VisualClass::TRUE_COLOR)
        })
        .map(|visual| visual.visual_id)
}

/// Whether gpui's wgpu renderer could actually hand per-pixel alpha to the
/// server.
///
/// gpui negotiates its surface alpha mode internally and never reports the
/// outcome, so this asks wgpu the same question ahead of time: an unmapped
/// 1x1 ARGB window, a surface over it, and the adapter's capabilities, on the
/// same backends gpui itself enables (`gpui_wgpu::WgpuContext::instance`).
/// The acceptance set mirrors gpui_wgpu's transparent-window preference list
/// (`wgpu_renderer.rs`): `PreMultiplied` or `Inherit` — anything else and the
/// renderer would quietly composite opaque.
fn renderer_alpha_capable(conn: &XCBConnection, screen_num: usize) -> bool {
    use x11rb::protocol::xproto::{ColormapAlloc, ConnectionExt as _, CreateWindowAux, WindowClass};

    let screen = &conn.setup().roots[screen_num];
    let Some(visual_id) = find_argb_visual(screen) else {
        return false;
    };
    let (Ok(colormap), Ok(window)) = (conn.generate_id(), conn.generate_id()) else {
        return false;
    };
    if !conn
        .create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual_id)
        .map(|cookie| cookie.check().is_ok())
        .unwrap_or(false)
    {
        return false;
    }
    let created = conn
        .create_window(
            32,
            window,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            visual_id,
            &CreateWindowAux::new()
                .background_pixel(0)
                .border_pixel(0)
                .colormap(colormap)
                .override_redirect(1),
        )
        .map(|cookie| cookie.check().is_ok())
        .unwrap_or(false);

    let capable = created && {
        let target = Arc::new(ProbeTarget {
            conn: conn.get_raw_xcb_connection(),
            screen: screen_num as i32,
            window,
            visual_id,
        });
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            ..wgpu::InstanceDescriptor::new_with_display_handle(Box::new(target.clone()))
        });
        instance
            .create_surface(target)
            .ok()
            .and_then(|surface| {
                let adapter =
                    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                        power_preference: wgpu::PowerPreference::LowPower,
                        force_fallback_adapter: false,
                        compatible_surface: Some(&surface),
                    }))
                    .ok()?;
                let modes = surface.get_capabilities(&adapter).alpha_modes;
                Some(
                    modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied)
                        || modes.contains(&wgpu::CompositeAlphaMode::Inherit),
                )
            })
            .unwrap_or(false)
    };

    if created {
        let _ = conn.destroy_window(window);
    }
    let _ = conn.free_colormap(colormap);
    let _ = conn.flush();
    capable
}

/// Raw-handle wrapper the capability probe hands to wgpu: the side XCB
/// connection plus the throwaway ARGB window created on it.
#[derive(Debug)]
struct ProbeTarget {
    conn: *mut std::ffi::c_void,
    screen: i32,
    window: u32,
    visual_id: u32,
}

// The raw connection pointer is only ever read on this thread; wgpu's surface
// types nonetheless insist on Send + Sync targets.
unsafe impl Send for ProbeTarget {}
unsafe impl Sync for ProbeTarget {}

impl raw_window_handle::HasDisplayHandle for ProbeTarget {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        let handle =
            raw_window_handle::XcbDisplayHandle::new(std::ptr::NonNull::new(self.conn), self.screen);
        Ok(unsafe {
            raw_window_handle::DisplayHandle::borrow_raw(raw_window_handle::RawDisplayHandle::Xcb(
                handle,
            ))
        })
    }
}

impl raw_window_handle::HasWindowHandle for ProbeTarget {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let mut handle = raw_window_handle::XcbWindowHandle::new(
            std::num::NonZeroU32::new(self.window).expect("X11 resource ids are never zero"),
        );
        handle.visual_id = std::num::NonZeroU32::new(self.visual_id);
        Ok(unsafe {
            raw_window_handle::WindowHandle::borrow_raw(raw_window_handle::RawWindowHandle::Xcb(
                handle,
            ))
        })
    }
}
