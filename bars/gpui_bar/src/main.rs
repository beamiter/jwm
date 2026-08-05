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
    App, Bounds, Context, IntoElement, MouseButton, ObjectFit, ParentElement, Pixels, Render,
    RenderImage, Rgba, ScrollDelta, ScrollWheelEvent, SharedString, Styled, Task, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, img, point,
    prelude::*, px, rgba, size,
};
use gpui_platform::application;
use std::sync::Arc;
use xbar_core::glass::wallpaper::WallpaperFile;
use xbar_core::glass::{GlassStrip, fallback_rgb};

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

    // --- Frosted glass ---
    /// Wallpaper source and frosting cache, absent when no wallpaper is
    /// configured. The bar then stays fully transparent and leaves the effect
    /// to whatever compositor is running.
    glass: Option<GlassStrip<WallpaperFile>>,
    glass_image: Option<Arc<RenderImage>>,
    glass_generation: u64,
}

impl GpuiBar {
    fn new(cx: &mut Context<Self>) -> Self {
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
            glass: glass_strip(),
            glass_image: None,
            glass_generation: 0,
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

impl GpuiBar {
    /// Re-frost the wallpaper under the bar and rebuild the render image when
    /// it changed. Cheap on an unchanged wallpaper and geometry.
    fn refresh_glass(&mut self, window: &Window) {
        let scale = window.scale_factor().max(f32::EPSILON);
        let bounds = window.bounds();
        let width = (f32::from(bounds.size.width) * scale).round().max(1.0) as u32;
        let height = (f32::from(bounds.size.height) * scale).round().max(1.0) as u32;
        // GPUI exposes no window position on X11, so the origin is the one the
        // window manager assigned; the wallpaper is laid out on that monitor.
        let geometry = self.active_geometry;
        let Some(strip) = self.glass.as_mut() else {
            return;
        };
        if let Some(geometry) = geometry {
            strip
                .source_mut()
                .set_screen(geometry.width.max(1), geometry.height.max(1));
        }
        let (x, y) = geometry.map_or((0, 0), |geometry| (geometry.x, geometry.y));

        let Some((generation, image)) = strip.ensure(x, y, width, height) else {
            return;
        };
        if generation == self.glass_generation && self.glass_image.is_some() {
            return;
        }
        // GPUI's `RenderImage` carries BGRA despite its `RgbaImage` container,
        // which is exactly the byte order a `GlassImage` already holds.
        let Some(buffer) =
            image::RgbaImage::from_raw(image.width(), image.height(), image.data().to_vec())
        else {
            return;
        };
        self.glass_image = Some(Arc::new(RenderImage::new([image::Frame::new(buffer)])));
        self.glass_generation = generation;
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
        self.refresh_glass(window);
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

        // With a backdrop the bar tints it; without one it stays fully
        // transparent and whatever compositor is running decides what shows.
        let background = match self.glass_image {
            Some(_) => rgba_alpha(0x1C1C1E, GLASS_TINT_ALPHA),
            None => rgba(0x00000000),
        };
        let mut root = div()
            .relative()
            .w_full()
            .h_full()
            .overflow_hidden()
            .font_family(NERD_FONT)
            .text_color(rgba_alpha(0xFFFFFF, 1.0));
        if let Some(backdrop) = self.glass_image.clone() {
            // Built at the window's own pixel size, so it must land on it
            // one-to-one rather than be letterboxed.
            root = root.child(
                img(backdrop)
                    .absolute()
                    .top_0()
                    .left_0()
                    .w_full()
                    .h_full()
                    .object_fit(ObjectFit::Fill),
            );
        }
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
                .bg(background)
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

    application().run(|cx: &mut App| {
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
            window_background: WindowBackgroundAppearance::Transparent,
            kind: WindowKind::Normal,
            is_resizable: false,
            is_minimizable: false,
            // WM_CLASS = "gpui_bar" — JWM detects this as its status bar
            // (see config_x11.toml: [status_bar] name = "gpui_bar").
            app_id: Some("gpui_bar".into()),
            ..Default::default()
        };

        cx.open_window(opts, |_w, cx| cx.new(GpuiBar::new))
            .expect("failed to open window");
        cx.activate(true);
    });
}

/// Alpha the bar's background keeps once a frosted backdrop shows through it.
const GLASS_TINT_ALPHA: f32 = 0.55;

/// The wallpaper source this bar's configuration asks for, if any.
///
/// Only the `[glass]` section of the shared bar config applies here; the rest
/// of this bar's appearance is its own theme.
fn glass_strip() -> Option<GlassStrip<WallpaperFile>> {
    let config = xbar_core::config::BarConfig::load_default().unwrap_or_else(|error| {
        warn!("falling back to the default bar config: {error}");
        xbar_core::config::BarConfig::default()
    });
    // A provisional screen size; `refresh_glass` corrects it from the window
    // manager's monitor geometry as soon as one arrives.
    config
        .glass
        .file_strip(1920, 1080, fallback_rgb(config.theme))
}
