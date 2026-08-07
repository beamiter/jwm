use std::env;
use std::time::{Duration, Instant};

use gpui::{
    App, Application, Bounds, Context, IntoElement, MouseButton, ParentElement, Pixels, Render,
    Rgba, ScrollDelta, ScrollWheelEvent, SharedString, Styled, Task, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point, prelude::*,
    px, rgb, size,
};
use gpui_component::{
    Root, Selectable, Sizable, Size, Theme, black, blue_400, blue_500, cyan_500, emerald_500,
    emerald_600, gray_500, green_500, indigo_500, orange_500, red_500, rose_500, slate_100,
    slate_300, slate_700, slate_800, slate_900, tag::Tag, white,
};
use gpui_component::{
    button::{Button, ButtonCustomVariant, ButtonVariants},
    init as init_components,
    tooltip::Tooltip,
};
use log::{debug, warn};
use xbar_core::glass::{DEFAULT_BACKGROUND_OPACITY, fallback_rgb};
use xbar_core::logging::init as initialize_logging;
use xbar_core::{
    BarEffect, BarRuntime, DOCK_ITEM_HEIGHT, DOCK_ITEM_WIDTH, DOCK_SLOT_WIDTH, DockBridge,
    LayoutId, ModelConfig, MonitorGeometry, PlatformEffectHandler, RuntimeUpdate, ShellRoute,
    TagId, TransportRecoveryConfig, UserAction, WindowToken,
};
use xbar_linux_actions::ProcessActionHandler;

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

const TAG_ACCENTS: [u32; 9] = [
    0xEF4444, 0x14B8A6, 0x0EA5E9, 0x22C55E, 0xF59E0B, 0xEC4899, 0x3B82F6, 0x6366F1, 0x06B6D4,
];
const TRANSPORT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

struct GpuiComponentBar {
    runtime: BarRuntime,
    process_actions: ProcessActionHandler,
    active_geometry: Option<MonitorGeometry>,
    default_size: Option<gpui::Size<Pixels>>,
    last_scale_factor: Option<f32>,
    geometry_dirty: bool,
    dock: DockBridge,
    _timer_task: Option<Task<()>>,
    _transport_task: Option<Task<()>>,

    // --- Compositor coupling ---
    /// The panel's one background: the shared fallback color at the
    /// configured opacity when a compositor blends the bar into the desktop,
    /// fully opaque when nothing would.
    panel_background: Rgba,
}

impl GpuiComponentBar {
    fn new(panel_background: Rgba, cx: &mut Context<Self>) -> Self {
        let shared_path = env::args().skip(1).last().unwrap_or_default();
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
        .expect("gpui component bar model configuration is valid");

        let mut this = Self {
            runtime,
            process_actions: ProcessActionHandler::default(),
            active_geometry: None,
            panel_background,
            default_size: None,
            last_scale_factor: None,
            geometry_dirty: false,
            dock: DockBridge::new(),
            _timer_task: None,
            _transport_task: None,
        };
        let update = this.runtime.tick();
        this.handle_runtime_update(update);
        this.spawn_clock(cx);
        this.spawn_transport_poller(cx);
        this
    }

    fn spawn_clock(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let _ = this.update(cx, |this, cx| {
                    let mut update = this.runtime.tick();
                    let dock_update = this.service_dock();
                    update.merge(dock_update);
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
                    let mut update = this.runtime.poll_transport();
                    let dock_update = this.service_dock();
                    update.merge(dock_update);
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

    fn service_dock(&mut self) -> RuntimeUpdate {
        let scale_factor = f64::from(self.last_scale_factor.unwrap_or(1.0));
        let snapshot = self.runtime.snapshot();
        self.dock.synchronize(
            &snapshot,
            self.runtime.transport_generation(),
            scale_factor,
            42.0,
            10.0,
        );
        let now = Instant::now();
        let mut combined = RuntimeUpdate::default();
        for action in self.dock.pending_actions(now) {
            let update = self.runtime.dispatch(action);
            let accepted = !update.has_issues();
            combined.merge(update);
            if accepted {
                self.dock.acknowledge(action, now);
            } else {
                self.dock.record_failure(now);
                break;
            }
        }
        combined
    }

    fn dock_hover(&mut self, token: WindowToken, session: u64, hovered: bool) {
        if hovered {
            let _ = self.dock.enter(token, session);
        } else {
            let _ = self.dock.leave(token, session);
        }
        let update = self.service_dock();
        self.handle_runtime_update(update);
    }

    fn dock_restore(&mut self, token: WindowToken, session: u64) {
        let _ = self.dock.request_restore(token, session);
        let update = self.service_dock();
        self.handle_runtime_update(update);
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

    fn workspace_button(&self, index: usize, cx: &App) -> Button {
        let accent = TAG_ACCENTS[index];
        let label = TAG_ICONS[index];

        let view = self.runtime.view();
        let (color, border, foreground, hover, selected) = if view.wm_available
            && let Some(status) = view.tags.get(index)
        {
            if status.urgent {
                (red_500(), rose_500(), white(), rose_500(), true)
            } else if status.filled || status.selected {
                (
                    rgb(accent).into(),
                    rgb(accent).into(),
                    white(),
                    rgb(accent).into(),
                    true,
                )
            } else if status.occupied {
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
                    if let Some(tag) = TagId::new(index) {
                        this.dispatch_wm(UserAction::ViewTag(tag));
                    }
                    cx.notify();
                },
            )));
        }
        row
    }

    fn render_layouts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let toggle = Self::chip_button(
            "layout-toggle",
            self.runtime.view().layout_symbol.to_owned(),
            orange_500(),
            orange_500(),
            white(),
            cx,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.dispatch(UserAction::ToggleLayoutSelector);
            cx.notify();
        }));

        let mut root = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(toggle);
        if self.runtime.view().layout_selector_open {
            let options = [("[]=", 0_u32), ("><>", 1_u32), ("[M]", 2_u32)];
            let mut row = div().flex().flex_row().items_center().gap(px(4.));
            for (symbol, layout_index) in options {
                let selected = symbol == self.runtime.view().layout_symbol;
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
                    this.dispatch_wm(UserAction::SetLayout(LayoutId(layout_index)));
                    cx.notify();
                }));
                row = row.child(button);
            }
            root = root.child(row);
        }
        root
    }

    fn render_usage_pills(&self) -> impl IntoElement {
        let system = self.runtime.view().system;
        let cpu = system.cpu_percent.map_or(0.0, |value| value.as_f32());
        let mem = system.memory_percent.map_or(0.0, |value| value.as_f32());

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
        let battery = self.runtime.view().battery;
        let percent = battery.percent.map_or(100.0, |value| value.as_f32());
        let charging = battery.charging;
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
        let brightness = match self.runtime.view().brightness.percent {
            Some(value) => format!("{ICON_BRIGHT}  {}%", value.rounded()),
            None => format!("{ICON_BRIGHT}  --"),
        };

        let audio = self.runtime.view().audio;
        let (volume, has_device) = audio
            .volume_percent
            .map_or((0, false), |value| (i32::from(value.rounded()), true));
        let muted = audio.muted;
        let volume_icon = volume_icon(volume, muted, has_device);
        let volume_label = if has_device {
            format!("{volume_icon}  {volume}%")
        } else {
            format!("{volume_icon}  --")
        };

        let time = self.runtime.view().time;
        let time = format!("{ICON_TIME}  {time}");
        let monitor = format!(
            "{ICON_MON}  {}",
            monitor_num_to_icon(self.runtime.view().monitor.0)
        );

        let brightness_chip = div()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.dispatch(UserAction::BrightnessUp);
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    this.dispatch(UserAction::BrightnessDown);
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
                    this.dispatch(UserAction::ToggleMute);
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

                let action = if delta_y > 0.0 {
                    UserAction::VolumeUp
                } else {
                    UserAction::VolumeDown
                };
                this.dispatch(action);

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

        // Entry point into JWM's own shell surface. It opens the hub, which
        // is itself the page that routes to applications, notifications,
        // clipboard, calendar and wallpaper; the bar renders none of them.
        let shell_available = self.runtime.view().wm_available;
        let shell_chip = Self::chip_button(
            "shell",
            ICON_SHELL,
            // Grayed rather than hidden: the shell lives in the window
            // manager, so an unreachable one has to look unreachable.
            if shell_available {
                indigo_500()
            } else {
                gray_500()
            },
            if shell_available {
                indigo_500()
            } else {
                slate_700()
            },
            white(),
            cx,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            if this.runtime.view().wm_available {
                this.dispatch(UserAction::OpenShellHub(ShellRoute::Hub));
                cx.notify();
            }
        }));

        let screenshot_chip =
            Self::chip_button("screenshot", ICON_SHOT, cyan_500(), blue_500(), white(), cx)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.dispatch(UserAction::Screenshot);
                    cx.notify();
                }));

        let time_chip = Self::chip_button("time", time, blue_400(), blue_500(), white(), cx)
            .on_click(cx.listener(|this, _, _, cx| {
                this.dispatch(UserAction::ToggleSeconds);
                let update = this.runtime.tick();
                this.handle_runtime_update(update);
                cx.notify();
            }));

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(brightness_chip)
            .child(volume_chip)
            .child(shell_chip)
            .child(screenshot_chip)
            .child(time_chip)
            .child(self.chip_tag(monitor, indigo_500(), white(), indigo_500()))
            .child(self.chip_tag("s: 1.00", slate_700(), slate_100(), slate_700()))
    }

    fn render_minimized_dock(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.runtime.view().wm_session_id;
        let windows: Vec<_> = self.dock.visible_windows().cloned().collect();
        let mut shelf = div()
            .id("minimized-dock")
            .w(px(self.dock.shelf_width()))
            .h_full()
            .pl(px(5.0))
            .pr(px(2.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .border_l_1()
            .border_color(slate_700());
        for minimized in windows {
            let token = minimized.token;
            let scale = self.dock.scale_for(token);
            let title = if minimized.title.trim().is_empty() {
                minimized.app_id.clone()
            } else {
                minimized.title.clone()
            };
            let hover_title = title.clone();
            let tooltip_title = SharedString::from(title);
            let background = if minimized.urgent() {
                red_500()
            } else {
                blue_500()
            };
            let card = div()
                .id(SharedString::from(format!(
                    "minimized-card-{}",
                    token.get()
                )))
                .w(px(DOCK_ITEM_WIDTH * scale))
                .h(px(DOCK_ITEM_HEIGHT * scale))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.0))
                .border_1()
                .border_color(if minimized.urgent() {
                    rose_500()
                } else {
                    slate_300()
                })
                .bg(background)
                .text_color(white())
                .text_size(px(11.0))
                .child(minimized.initial().to_string())
                .tooltip(move |window, cx| Tooltip::new(tooltip_title.clone()).build(window, cx))
                .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                    this.dock_hover(token, session, *hovered);
                    if *hovered {
                        debug!("minimized Dock hover: {hover_title}");
                    }
                    cx.notify();
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        this.dock_restore(token, session);
                        cx.notify();
                    }),
                );
            shelf = shelf.child(
                div()
                    .w(px(DOCK_SLOT_WIDTH))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(card),
            );
        }
        if self.dock.overflow() || self.dock.collapsed() {
            shelf = shelf.child(div().w(px(12.0)).text_color(slate_300()).child("+"));
        }
        shelf
    }
}

impl Render for GpuiComponentBar {
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
        let dock_update = self.service_dock();
        self.handle_runtime_update(dock_update);
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
            .child(self.render_interactive_pills(cx))
            .child(self.render_minimized_dock(cx));

        let root = div()
            .relative()
            .size_full()
            .overflow_hidden()
            .font_family(NERD_FONT)
            .text_color(white());
        let panel = div()
            .relative()
            .size_full()
            .px(px(10.))
            .py(px(6.))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(slate_800())
            .bg(self.panel_background)
            .child(left)
            .child(right);
        root.child(panel)
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

    // Only `theme` and `background_opacity` of the shared bar config apply
    // here; the rest of this bar's appearance is gpui-component's own theme.
    let config = xbar_core::config::BarConfig::load_default().unwrap_or_else(|error| {
        warn!("falling back to the default bar config: {error}");
        xbar_core::config::BarConfig::default()
    });

    let translucent = compositor_active() && RENDERER_ALPHA_CAPABLE;
    let [red, green, blue] = fallback_rgb(config.theme);
    let opacity = if translucent {
        config
            .background_opacity
            .unwrap_or(DEFAULT_BACKGROUND_OPACITY)
    } else {
        1.0
    };
    let panel_background = Rgba {
        r: f32::from(red) / 255.0,
        g: f32::from(green) / 255.0,
        b: f32::from(blue) / 255.0,
        a: opacity as f32,
    };

    Application::new().run(move |cx: &mut App| {
        init_components(cx);
        // Root paints the whole window in the component theme's background.
        // Solid mode recolors that sheet to the shared fallback; translucent
        // mode clears it entirely so the panel's own wash is the only alpha
        // the compositor gets to see.
        Theme::global_mut(cx).background = if translucent {
            gpui::transparent_black()
        } else {
            panel_background.into()
        };
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
            window_background: if translucent {
                WindowBackgroundAppearance::Transparent
            } else {
                WindowBackgroundAppearance::Opaque
            },
            kind: WindowKind::Normal,
            is_resizable: false,
            is_minimizable: false,
            app_id: Some("gpui_component_bar".into()),
            ..Default::default()
        };

        cx.open_window(options, move |window, cx| {
            let view = cx.new(move |cx| GpuiComponentBar::new(panel_background, cx));
            cx.new(|cx| Root::new(view, window, cx))
        })
        .expect("failed to open gpui_component_bar window");
        cx.activate(true);
    });
}

// -------- Compositor coupling ------------------------------------------------

/// Whether this bar's renderer can hand per-pixel alpha to the compositor at
/// all. It cannot: gpui's blade renderer (blade-graphics 0.7.1,
/// `src/vulkan/surface.rs:174-206`) only takes `POST_MULTIPLIED` or
/// `PRE_MULTIPLIED` swapchain alpha — never `INHERIT`, which is all the usual
/// X11 Vulkan drivers advertise — and gpui gives the app no way to learn what
/// was negotiated. So the bar ships solid until a gpui/blade upgrade changes
/// that; every mode seam is already in place, and flipping this const is the
/// whole migration.
const RENDERER_ALPHA_CAPABLE: bool = false;

/// True when a compositing manager owns the conventional `_NET_WM_CM_Sn`
/// selection.
///
/// Sampled once, on a throwaway side connection, before gpui runs: per-pixel
/// alpha is a window-creation decision, so a compositor started or stopped
/// later goes unnoticed until the bar restarts. Ownership also only promises
/// compositing — whether the compositor blurs behind the bar is its own
/// affair.
fn compositor_active() -> bool {
    use x11rb::protocol::xproto::ConnectionExt as _;

    let Ok((conn, screen_num)) = x11rb::connect(None) else {
        return false;
    };
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
