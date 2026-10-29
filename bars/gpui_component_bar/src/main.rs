#[path = "../../gpui_alpha.rs"]
mod gpui_alpha;
#[path = "../../gpui_header.rs"]
mod gpui_header;
#[path = "../../gpui_sidebar.rs"]
mod gpui_sidebar;

use std::env;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, Context, IntoElement, MouseButton, ParentElement, Pixels, Render, Rgba,
    ScrollEvent, SharedString, Styled, Task, Window, WindowBackgroundAppearance, WindowBounds,
    WindowKind, WindowOptions, div, point, prelude::*, px, rgb, size,
};
use gpui_component::{
    Root, Selectable, Sizable, Size, Theme, black, blue_400, blue_500, cyan_500, emerald_500,
    emerald_600, gray_500, green_500, indigo_500, orange_500, red_500, rose_500, slate_100,
    slate_300, slate_700, slate_800, slate_900, tag::Tag, white,
};
use gpui_component::{
    button::{Button, ButtonCustomVariant, ButtonVariants},
    init as init_components,
};
use log::{debug, warn};
use xbar_core::glass::fallback_rgb;
use xbar_core::logging::init as initialize_logging;
use xbar_core::{
    BarEffect, BarRuntime, DockBridge, DockItemBinding, LayoutId, ModelConfig, MonitorGeometry,
    PlatformEffectHandler, RuntimeUpdate, ShellRoute, TagId, TransportRecoveryConfig, UserAction,
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

#[derive(Default)]
struct ClickLatch(bool);

impl ClickLatch {
    fn press(&mut self) {
        self.0 = true;
    }

    fn release(&mut self) -> bool {
        std::mem::take(&mut self.0)
    }

    fn cancel(&mut self) {
        self.0 = false;
    }
}

struct GpuiComponentBar {
    runtime: BarRuntime,
    scroll: xbar_core::ControlScroll,
    presentation: xbar_core::presentation::PresentationConfig,
    process_actions: ProcessActionHandler,
    active_geometry: Option<MonitorGeometry>,
    default_size: Option<gpui::Size<Pixels>>,
    last_scale_factor: Option<f32>,
    geometry_dirty: bool,
    brightness_secondary_click: ClickLatch,
    dock: DockBridge,
    _timer_task: Option<Task<()>>,
    _transport_task: Option<Task<()>>,

    // --- Compositor coupling ---
    /// The panel's one background: the shared fallback color at the
    /// configured opacity when a compositor blends the bar into the desktop,
    /// fully opaque when nothing would.
    panel_background: Rgba,
    background_opacity: Option<f64>,
    translucent: bool,
}

impl gpui_sidebar::ScrollOwner for GpuiComponentBar {
    fn control_scroll(&mut self) -> &mut xbar_core::ControlScroll {
        &mut self.scroll
    }
    fn scroll_edge(&self) -> xbar_core::BarEdge {
        self.presentation.edge
    }
}

impl GpuiComponentBar {
    fn new(
        panel_background: Rgba,
        background_opacity: Option<f64>,
        translucent: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let shared_path = env::args().skip(1).last().unwrap_or_default();
        let mut bar_config = xbar_core::config::BarConfig::load_default().unwrap_or_default();
        let header = bar_config.presentation.apply_desktop_header_profile();
        let background_opacity = if header {
            bar_config.background_opacity.or(Some(0.58))
        } else {
            background_opacity
        };
        let config = ModelConfig {
            initial_theme: bar_config.theme,
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

        let mut dock = DockBridge::new();
        dock.set_edge(bar_config.presentation.edge);
        let mut this = Self {
            runtime,
            scroll: xbar_core::ControlScroll::with_policy(bar_config.presentation.scroll),
            process_actions: ProcessActionHandler::default(),
            active_geometry: None,
            panel_background,
            background_opacity,
            translucent,
            default_size: None,
            last_scale_factor: None,
            geometry_dirty: false,
            brightness_secondary_click: ClickLatch::default(),
            dock,
            presentation: bar_config.presentation,
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
                let mut wait = TRANSPORT_POLL_INTERVAL;
                if this
                    .update(cx, |this, _| {
                        wait = this
                            .dock
                            .next_wake_delay(Instant::now(), TRANSPORT_POLL_INTERVAL);
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(wait).await;
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
            self.presentation.logical_thickness(scale_factor) as f32,
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

    fn dock_hover(&mut self, binding: DockItemBinding, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            let _ = self.dock.enter(binding);
        } else {
            let _ = self.dock.leave(binding);
        }
        let update = self.service_dock();
        self.handle_runtime_update(update);
        // Replacing the owned task cancels its old 100ms fallback sleep so a
        // newly-created 50ms anchor deadline takes effect immediately.
        self.spawn_transport_poller(cx);
    }

    fn dock_restore(&mut self, binding: DockItemBinding, cx: &mut Context<Self>) {
        let _ = self.dock.request_restore(binding);
        let update = self.service_dock();
        self.handle_runtime_update(update);
        self.spawn_transport_poller(cx);
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
                effect @ (BarEffect::Screenshot
                | BarEffect::OpenAudioControl
                | BarEffect::MediaPlayPause) => {
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
            .border_1()
            .border_color(border)
            .selected(selected)
            .custom(
                ButtonCustomVariant::new(cx)
                    .color(color)
                    .foreground(foreground)
                    .hover(hover)
                    .active(border),
            )
    }

    fn chip_button(
        id: impl Into<gpui::ElementId>,
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
            .border_1()
            .border_color(border)
            .custom(
                ButtonCustomVariant::new(cx)
                    .color(color)
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
                    SharedString::from(format!("layout-option-{layout_index}")),
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
        let percent = battery
            .percent
            .filter(|_| battery.present)
            .map(|value| value.as_f32());
        let charging = battery.charging;
        let icon = if charging {
            ICON_BAT_CHG
        } else {
            ICON_BAT_FULL
        };
        let (bg, fg) = match percent {
            None => (gray_500(), white()),
            Some(percent) if percent > 50.0 => (green_500(), white()),
            Some(percent) if percent > 20.0 => (rgb(0xFACC15).into(), black()),
            Some(_) => (red_500(), white()),
        };
        let label = percent.map_or_else(|| "--".to_owned(), |value| format!("{value:.0}%"));
        self.chip_tag(format!("{icon}  {label}"), bg, fg, bg)
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
            .id("brightness-actions")
            .on_hover(cx.listener(|this, hovered: &bool, _, _| {
                gpui_sidebar::hover_control(
                    this,
                    xbar_core::presentation::NodeId::Brightness,
                    *hovered,
                )
            }))
            .on_scroll(cx.listener(|this, event: &ScrollEvent, _, cx| {
                cx.stop_propagation();
                if let Some(input) = gpui_sidebar::scroll_action(
                    this,
                    xbar_core::presentation::NodeId::Brightness,
                    event,
                ) {
                    this.dispatch(
                        if input == xbar_core::presentation::PointerAction::ScrollUp {
                            UserAction::BrightnessUp
                        } else {
                            UserAction::BrightnessDown
                        },
                    );
                    cx.notify();
                }
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.scroll.reset();
                this.dispatch(UserAction::BrightnessUp);
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, _| {
                    this.scroll.reset();
                    this.brightness_secondary_click.press();
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    if this.brightness_secondary_click.release() {
                        this.dispatch(UserAction::BrightnessDown);
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Right,
                cx.listener(|this, _, _, _| {
                    this.brightness_secondary_click.cancel();
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
            .id("volume-actions")
            .on_hover(cx.listener(|this, hovered: &bool, _, _| {
                gpui_sidebar::hover_control(this, xbar_core::presentation::NodeId::Audio, *hovered)
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.scroll.reset();
                this.dispatch(UserAction::ToggleMute);
                cx.notify();
            }))
            .on_scroll(cx.listener(|this, event: &ScrollEvent, _, cx| {
                cx.stop_propagation();
                if let Some(input) =
                    gpui_sidebar::scroll_action(this, xbar_core::presentation::NodeId::Audio, event)
                {
                    this.dispatch(
                        if input == xbar_core::presentation::PointerAction::ScrollUp {
                            UserAction::VolumeUp
                        } else {
                            UserAction::VolumeDown
                        },
                    );
                    cx.notify();
                }
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

    fn render_minimized_dock(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        gpui_sidebar::render_dock(
            &self.dock,
            self.runtime.view().theme,
            Self::dock_hover,
            Self::dock_restore,
            cx,
        )
    }
}

impl Render for GpuiComponentBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.panel_background.a = xbar_core::background::bar_background_opacity(
            self.runtime.view().blur_status_bar,
            self.background_opacity,
            self.translucent,
        ) as f32;
        let scale_factor = window.scale_factor().max(f32::EPSILON);
        self.default_size
            .get_or_insert_with(|| window.bounds().size);
        let scale_changed = self
            .last_scale_factor
            .is_none_or(|previous| previous.to_bits() != scale_factor.to_bits());
        if self.geometry_dirty || scale_changed {
            self.scroll.reset();
            let default_size = self.default_size.expect("default size is initialized");
            let target_size = self.active_geometry.map_or(default_size, |geometry| {
                xbar_core::BarPlacement::from_bar_geometry(
                    geometry,
                    self.presentation.logical_thickness(f64::from(scale_factor)),
                    f64::from(scale_factor),
                    self.presentation.edge,
                )
                .map_or(default_size, |placement| {
                    size(
                        px(placement.width as f32 / scale_factor),
                        px(placement.height as f32 / scale_factor),
                    )
                })
            });
            // GPUI has no public window-position API. JWM remains responsible
            // for applying geometry.x/y; this frontend applies the logical size.
            window.resize(target_size);
            self.geometry_dirty = false;
            self.last_scale_factor = Some(scale_factor);
        }
        let dock_update = self.service_dock();
        self.handle_runtime_update(dock_update);
        if self.presentation.edge.is_vertical() {
            let dock = self.render_minimized_dock(cx);
            return gpui_sidebar::render(
                &self.runtime.snapshot(),
                &self.presentation,
                self.panel_background,
                dock,
                self.dock.shelf_bounds(),
                Self::dispatch,
                cx,
            )
            .into_any_element();
        }
        if self.presentation.desktop_header {
            return gpui_header::render(
                &self.runtime.snapshot(),
                &self.presentation,
                self.panel_background.a,
                Self::dispatch,
                window,
                cx,
            );
        }
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
            .pr(px(self
                .dock
                .shelf_bounds()
                .map_or(10.0, |bounds| bounds.width + 8.0)))
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
            .child(self.render_minimized_dock(cx))
            .into_any_element()
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

const fn should_quit_after_window_close(remaining_windows: usize) -> bool {
    remaining_windows == 0
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

    let translucent = gpui_alpha::detect_translucency();
    let [red, green, blue] = fallback_rgb(config.theme);
    let opacity =
        xbar_core::background::bar_background_opacity(None, config.background_opacity, translucent);
    let panel_background = Rgba {
        r: f32::from(red) / 255.0,
        g: f32::from(green) / 255.0,
        b: f32::from(blue) / 255.0,
        a: opacity as f32,
    };

    gpui_platform::application().run(move |cx: &mut App| {
        init_components(cx);
        cx.on_window_closed(|cx, _closed_window| {
            if should_quit_after_window_close(cx.windows().len()) {
                cx.quit();
            }
        })
        .detach();
        // Root paints the whole window in the component theme's background.
        // Solid mode recolors that sheet to the shared fallback; translucent
        // mode clears it entirely so the panel's own wash is the only alpha
        // the compositor gets to see. Keep the root's background token in sync
        // with the theme color through the component theme update API.
        Theme::update(cx, |theme| {
            theme.background = if translucent {
                gpui::transparent_black()
            } else {
                panel_background.into()
            };
        });
        let mut height: Pixels = px(config.presentation.logical_thickness(1.0) as f32);
        let mut width: Pixels = cx
            .primary_display()
            .map(|display| display.bounds().size.width)
            .unwrap_or(px(1920.));
        if config.presentation.edge.is_vertical() {
            height = cx
                .primary_display()
                .map(|display| display.bounds().size.height)
                .unwrap_or(px(1080.));
            width = px(config.presentation.logical_thickness(1.0) as f32);
        }

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
            let view = cx.new(move |cx| {
                GpuiComponentBar::new(panel_background, config.background_opacity, translucent, cx)
            });
            cx.new(|cx| Root::new(view, window, cx))
        })
        .expect("failed to open gpui_component_bar window");
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::{ClickLatch, should_quit_after_window_close};

    #[test]
    fn click_latch_requires_an_uncancelled_press() {
        let mut latch = ClickLatch::default();
        assert!(!latch.release());

        latch.press();
        assert!(latch.release());
        assert!(!latch.release());

        latch.press();
        latch.cancel();
        assert!(!latch.release());
    }

    #[test]
    fn application_quits_only_after_the_last_window_closes() {
        assert!(!should_quit_after_window_close(1));
        assert!(should_quit_after_window_close(0));
    }
}
