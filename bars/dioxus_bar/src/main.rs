use dioxus::{
    desktop::{
        Config, DesktopContext, LogicalPosition, LogicalSize, WindowBuilder,
        tao::dpi::{PhysicalPosition, PhysicalSize},
        use_window,
    },
    prelude::*,
};
use log::{error, info, warn};
use std::{
    env,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use xbar_core::glass::{DEFAULT_BACKGROUND_OPACITY, fallback_rgb};
use xbar_core::logging::init as initialize_logging;
use xbar_core::presentation::{Palette, Rgba};
use xbar_core::{
    AudioDeviceInfo, BarEffect, BarRuntime, BarSnapshot, LayoutId, ModelConfig, MonitorGeometry,
    Percent, PlatformEffectHandler, RuntimeUpdate, ShellRoute, SystemDetails, TagId, TagState,
    TransportRecoveryConfig, UserAction,
};
use xbar_linux_actions::{CommandRunner, CommandSpec, ProcessActionHandler};

const STYLE_CSS: &str = include_str!("../assets/style.css");

const BAR_LOGICAL_HEIGHT: f64 = 40.0;
const TRANSPORT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

// ===== Nerd Font 图标（与 tauri_react_bar 对齐） =====
const TAG_ICONS: &[&str] = &[
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

fn monitor_icon_glyph(num: i32) -> &'static str {
    match num {
        0 => "\u{F02DA}",
        1 => "\u{F02DB}",
        _ => "?",
    }
}

fn volume_icon(snap: Option<&AudioDeviceInfo>) -> &'static str {
    match snap {
        None => ICON_VOL_MUTE,
        Some(s) if s.is_muted || s.volume <= 0 => ICON_VOL_MUTE,
        Some(s) if s.volume < 34 => ICON_VOL_LOW,
        Some(s) if s.volume < 67 => ICON_VOL_MID,
        Some(_) => ICON_VOL_HIGH,
    }
}

struct RuntimeOwner {
    runtime: BarRuntime,
}

impl RuntimeOwner {
    fn new(shared_path: String) -> Self {
        let config = ModelConfig {
            show_seconds: true,
            ..ModelConfig::default()
        };
        let runtime = if shared_path.is_empty() {
            BarRuntime::new(config)
        } else {
            let recovery = TransportRecoveryConfig::new(shared_path, TRANSPORT_RETRY_INTERVAL)
                .expect("static transport recovery config is valid");
            BarRuntime::with_managed_transport(config, recovery)
        }
        .expect("dioxus bar model configuration is valid");

        Self { runtime }
    }

    fn snapshot(&self) -> BarSnapshot {
        self.runtime.snapshot()
    }

    fn tick(&mut self) -> (RuntimeUpdate, BarSnapshot) {
        let update = self.runtime.tick();
        (update, self.runtime.snapshot())
    }

    fn poll_transport(&mut self) -> (RuntimeUpdate, BarSnapshot) {
        let update = self.runtime.poll_transport();
        (update, self.runtime.snapshot())
    }

    fn dispatch(&mut self, action: UserAction) -> (RuntimeUpdate, BarSnapshot) {
        let update = self.runtime.dispatch(action);
        (update, self.runtime.snapshot())
    }
}

/// The bar's side X11 connection, shared by compositor detection and the
/// window depth probe.
///
/// dioxus pins GDK to X11 before its event loop starts, so this connection
/// reaches the same server the bar window lives on; the webview stack never
/// exposes its own connection, hence the dedicated one. A session where it
/// cannot open — say native Wayland with no Xwayland — simply reads as "no
/// compositor" and lands on the safe opaque bar.
static X11_SIDE: OnceLock<Option<(x11rb::rust_connection::RustConnection, usize)>> =
    OnceLock::new();

fn x11_side() -> Option<&'static (x11rb::rust_connection::RustConnection, usize)> {
    X11_SIDE.get_or_init(|| x11rb::connect(None).ok()).as_ref()
}

/// The launch decision, carried from `main` into the component tree; it is
/// made before the window exists because transparency is a creation-time
/// property dioxus offers no later way to change.
static TRANSLUCENT_AT_LAUNCH: OnceLock<bool> = OnceLock::new();

/// Whether a compositor owns `_NET_WM_CM_S{screen}` on the default screen.
///
/// Sampled once, at startup: a compositor started or stopped after launch is
/// not followed until the bar restarts. Ownership also only proves
/// compositing is on — jwm claims the selection even with blur disabled, so a
/// translucent bar can sit over a perfectly sharp desktop. Any failure along
/// the way reads as "no compositor", the conservative solid mode.
fn compositor_active() -> bool {
    use x11rb::protocol::xproto::ConnectionExt as _;
    let Some((conn, screen_num)) = x11_side() else {
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

/// Whether the window tao actually created carries an alpha channel.
///
/// `with_transparent` is a request, not a guarantee: without an RGBA visual
/// GTK silently falls back to an opaque window, and translucent CSS over that
/// window would land on undefined bytes. Depth 32 is the proof the request
/// was honored.
fn window_is_argb(window: &DesktopContext) -> bool {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    use x11rb::protocol::xproto::ConnectionExt as _;
    let Some((conn, _)) = x11_side() else {
        return false;
    };
    let Ok(handle) = window.window_handle() else {
        return false;
    };
    let xid = match handle.as_raw() {
        RawWindowHandle::Xlib(xlib) => match u32::try_from(xlib.window) {
            Ok(xid) => xid,
            Err(_) => return false,
        },
        RawWindowHandle::Xcb(xcb) => xcb.window.get(),
        _ => return false,
    };
    conn.get_geometry(xid)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|geometry| geometry.depth == 32)
        .unwrap_or(false)
}

fn css_rgba(color: Rgba) -> String {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "rgba({}, {}, {}, {:.3})",
        channel(color.red),
        channel(color.green),
        channel(color.blue),
        color.alpha.clamp(0.0, 1.0)
    )
}

/// The part of the bar's appearance that the stylesheet cannot know: the
/// compositor mode and the configured theme.
///
/// The stylesheet was written for the near-white row this bar used to paint
/// unconditionally, so the theme's background alone would leave dark glyphs on
/// a dark bar. Everything here that is not the row background therefore
/// re-states the light-authored rules in terms of the shared palette — the same
/// one every other bar renders from, which is what keeps the family looking
/// like one family. The saturated pills (usage, layout, clock) already carry
/// their own contrast and are left alone.
///
/// Injected after the stylesheet, so equal specificity is enough to win; the
/// per-tag rules are `!important` at three class selectors deep, so the two
/// overrides that fight them match that weight exactly.
fn mode_style(translucent: bool) -> String {
    let config = xbar_core::config::BarConfig::load_default().unwrap_or_else(|error| {
        warn!("falling back to the default bar config: {error}");
        xbar_core::config::BarConfig::default()
    });
    let [r, g, b] = fallback_rgb(config.theme);
    let palette = Palette::for_theme(config.theme);
    let background = if translucent {
        let alpha = config
            .background_opacity
            .unwrap_or(DEFAULT_BACKGROUND_OPACITY);
        format!("rgba({r}, {g}, {b}, {alpha})")
    } else {
        format!("rgb({r}, {g}, {b})")
    };
    let text = css_rgba(palette.text);
    let muted = css_rgba(palette.muted_text);
    let idle = css_rgba(palette.occupied);
    let hovered = css_rgba(palette.hovered);

    format!(
        ".button-row {{ background: {background}; }}
body {{ color: {text}; }}
.emoji-button.state-default {{ background: {idle}; border-color: {hovered}; color: {muted}; }}
.emoji-button.state-default:hover:not(:disabled):not(.pressed):not(:active) {{ background: {hovered}; border-color: {hovered}; }}
.emoji-button.state-default.pressed, .emoji-button.state-default:active {{ background: {hovered} !important; border-color: {hovered} !important; }}
.button-row .emoji-button.state-occupied {{ color: {text} !important; }}
.button-row .emoji-button.state-occupied::after {{ color: {muted}; }}"
    )
}

fn apply_monitor_geometry(geometry: MonitorGeometry, window: &DesktopContext) {
    let physical_height = (window.scale_factor() * BAR_LOGICAL_HEIGHT)
        .round()
        .clamp(1.0, f64::from(u32::MAX)) as u32;
    window.set_outer_position(PhysicalPosition::new(geometry.x, geometry.y));
    window.set_inner_size(PhysicalSize::new(geometry.width, physical_height));
}

#[derive(Clone, Copy)]
struct WindowBaseline {
    position: Option<PhysicalPosition<i32>>,
    logical_size: LogicalSize<f64>,
}

fn restore_window_baseline(baseline: WindowBaseline, window: &DesktopContext) {
    if let Some(position) = baseline.position {
        window.set_outer_position(position);
    }
    let physical_size = baseline
        .logical_size
        .to_physical::<u32>(window.scale_factor());
    window.set_inner_size(PhysicalSize::new(
        physical_size.width.max(1),
        physical_size.height.max(1),
    ));
}

fn handle_runtime_update(update: RuntimeUpdate, window: &DesktopContext, baseline: WindowBaseline) {
    for issue in update.issues {
        warn!("xbar runtime issue: {issue:?}");
    }
    for effect in update.platform_effects {
        match effect {
            BarEffect::ApplyMonitorGeometry(geometry) => {
                apply_monitor_geometry(geometry, window);
            }
            BarEffect::ClearMonitorGeometry => restore_window_baseline(baseline, window),
            effect @ (BarEffect::Screenshot | BarEffect::OpenAudioControl) => {
                static ACTIONS: OnceLock<Mutex<ProcessActionHandler>> = OnceLock::new();
                let actions = ACTIONS.get_or_init(|| Mutex::new(ProcessActionHandler::default()));
                match actions.lock() {
                    Ok(mut actions) => {
                        if let Err(error) = actions.handle(effect) {
                            warn!("failed to handle platform effect: {error}");
                        }
                    }
                    Err(error) => warn!("platform action handler mutex was poisoned: {error}"),
                }
            }
            unhandled => warn!("unhandled xbar platform effect: {unhandled:?}"),
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;
    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }
    if unit_index == 0 {
        format!("{:.0}{}", size, UNITS[unit_index])
    } else {
        format!("{:.1}{}", size, UNITS[unit_index])
    }
}

/// JWM 自带 shell 的入口。
///
/// 每一行只是向窗口管理器请求打开对应页面：应用、通知、剪贴板、日历、壁纸都
/// 由窗口管理器绘制，状态栏不持有任何 shell 状态。
#[component]
fn ShellButton(wm_available: bool, on_action: EventHandler<UserAction>) -> Element {
    const ROUTES: [(ShellRoute, &str, &str); 6] = [
        (ShellRoute::Hub, "\u{F0F2A}", "Shell Hub"),
        (ShellRoute::Applications, "\u{F0D22}", "Applications"),
        (ShellRoute::Notifications, "\u{F009A}", "Notifications"),
        (ShellRoute::Clipboard, "\u{F0192}", "Clipboard"),
        (ShellRoute::Calendar, "\u{F00ED}", "Calendar"),
        (ShellRoute::Wallpaper, "\u{F02E9}", "Wallpaper"),
    ];

    let class = if wm_available {
        "pill shell-pill"
    } else {
        "pill shell-pill shell-pill-offline"
    };

    rsx! {
        div { class: "shell-menu",
            div {
                class: "{class}",
                // 主键直接打开 Hub；Hub 本身就是通往其它页面的目录。
                onclick: move |_| {
                    if wm_available {
                        on_action.call(UserAction::OpenShellHub(ShellRoute::Hub));
                    }
                },
                title: "JWM shell",
                span { class: "nf-icon", "{ROUTES[0].1}" }
            }
            if wm_available {
                div { class: "shell-dropdown",
                    for (route, icon, label) in ROUTES {
                        div {
                            key: "{label}",
                            class: "shell-route",
                            onclick: move |_| on_action.call(UserAction::OpenShellHub(route)),
                            span { class: "nf-icon", "{icon}" }
                            span { "{label}" }
                        }
                    }
                }
            }
        }
    }
}

// 截图按钮
#[component]
fn ScreenshotButton(on_action: EventHandler<UserAction>) -> Element {
    rsx! {
        div {
            class: "pill screenshot-pill",
            onclick: move |_| on_action.call(UserAction::Screenshot),
            title: "截图 (Flameshot)",
            span { class: "nf-icon", "{ICON_SHOT}" }
        }
    }
}

// 系统信息（CPU/MEM/电池）
#[component]
fn SystemInfoDisplay(
    snapshot: Option<SystemDetails>,
    battery_percent: Option<u8>,
    is_charging: bool,
) -> Element {
    let sev = |p: f32| {
        if p <= 30.0 {
            "usage-good"
        } else if p <= 60.0 {
            "usage-warn"
        } else if p <= 80.0 {
            "usage-caution"
        } else {
            "usage-danger"
        }
    };

    if let Some(ref s) = snapshot {
        let battery_percent = battery_percent.map(f32::from).unwrap_or(100.0);
        let cpu_class = sev(s.cpu_average);
        let mem_class = sev(s.memory_usage_percent);
        let batt_class = if battery_percent > 50.0 {
            "usage-good"
        } else if battery_percent > 20.0 {
            "usage-warn"
        } else {
            "usage-danger"
        };

        let cpu_cls = format!("pill usage-pill {}", cpu_class);
        let mem_cls = format!("pill usage-pill {}", mem_class);
        let batt_cls = format!("pill usage-pill {}", batt_class);

        let mem_title = format!(
            "内存使用: {} / {}",
            format_bytes(s.memory_used),
            format_bytes(s.memory_total)
        );
        let batt_title = if is_charging {
            format!("电池充电中: {:.1}%", battery_percent)
        } else {
            format!("电池电量: {:.1}%", battery_percent)
        };
        let batt_icon = if is_charging {
            ICON_BAT_CHG
        } else {
            ICON_BAT_FULL
        };
        let cpu_text = format!("{:.0}%", s.cpu_average);
        let mem_text = format!("{:.0}%", s.memory_usage_percent);
        let batt_text = format!("{:.0}%", battery_percent);

        rsx! {
            div { class: "system-info-container",
                div { class: "{cpu_cls}", title: "CPU 平均使用率",
                    span { class: "nf-icon", "{ICON_CPU}" }
                    "{cpu_text}"
                }
                div { class: "{mem_cls}", title: "{mem_title}",
                    span { class: "nf-icon", "{ICON_MEM}" }
                    "{mem_text}"
                }
                div { class: "{batt_cls}", title: "{batt_title}",
                    span { class: "nf-icon", "{batt_icon}" }
                    "{batt_text}"
                }
            }
        }
    } else {
        rsx! {
            div { class: "system-info-container",
                div { class: "pill usage-pill usage-warn",
                    span { class: "nf-icon", "{ICON_CPU}" }
                    "--%"
                }
                div { class: "pill usage-pill usage-warn",
                    span { class: "nf-icon", "{ICON_MEM}" }
                    "--%"
                }
                div { class: "pill usage-pill usage-warn",
                    span { class: "nf-icon", "{ICON_BAT_FULL}" }
                    "--%"
                }
            }
        }
    }
}

// 音量控制
#[component]
fn VolumeControl(
    snapshot: Option<AudioDeviceInfo>,
    on_action: EventHandler<UserAction>,
) -> Element {
    let snap = snapshot.clone();
    let muted = snap.as_ref().map(|s| s.is_muted).unwrap_or(true);
    let has_device = snap.is_some();
    let vol = snap.as_ref().map(|s| s.volume).unwrap_or(0);
    let icon = volume_icon(snap.as_ref());
    let label = if has_device {
        format!("{}%", vol)
    } else {
        "--".to_string()
    };
    let cls = if muted {
        "pill volume-pill muted"
    } else {
        "pill volume-pill"
    };

    let on_click = move |_| {
        on_action.call(UserAction::ToggleMute);
    };
    let on_wheel = move |evt: Event<WheelData>| {
        evt.prevent_default();
        let dy = evt.data().delta().strip_units().y;
        let action = if dy < 0.0 {
            UserAction::VolumeUp
        } else {
            UserAction::VolumeDown
        };
        on_action.call(action);
    };

    rsx! {
        div {
            class: "{cls}",
            onclick: on_click,
            onwheel: on_wheel,
            title: "左键静音 / 滚轮调节",
            span { class: "nf-icon", "{icon}" }
            "{label}"
        }
    }
}

// 亮度控制
#[component]
fn BrightnessControl(percent: Option<Percent>, on_action: EventHandler<UserAction>) -> Element {
    let label = match percent {
        Some(value) => format!("{}%", value.rounded()),
        None => "--".to_string(),
    };

    let on_click = move |_| {
        on_action.call(UserAction::BrightnessUp);
    };
    let on_context = move |evt: Event<MouseData>| {
        evt.prevent_default();
        on_action.call(UserAction::BrightnessDown);
    };
    let on_wheel = move |evt: Event<WheelData>| {
        evt.prevent_default();
        let dy = evt.data().delta().strip_units().y;
        let action = if dy < 0.0 {
            UserAction::BrightnessUp
        } else {
            UserAction::BrightnessDown
        };
        on_action.call(action);
    };

    rsx! {
        div {
            class: "pill brightness-pill",
            onclick: on_click,
            oncontextmenu: on_context,
            onwheel: on_wheel,
            title: "左键加亮 / 右键减暗 / 滚轮调节",
            span { class: "nf-icon", "{ICON_BRIGHT}" }
            "{label}"
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ButtonState {
    Filtered,
    Selected,
    Urgent,
    Occupied,
    Default,
}

impl ButtonState {
    fn from_flags(is_filtered: bool, is_selected: bool, is_urg: bool, is_occ: bool) -> Self {
        if is_filtered {
            ButtonState::Filtered
        } else if is_selected {
            ButtonState::Selected
        } else if is_urg {
            ButtonState::Urgent
        } else if is_occ {
            ButtonState::Occupied
        } else {
            ButtonState::Default
        }
    }

    fn to_css_class(&self) -> &'static str {
        match self {
            ButtonState::Filtered => "emoji-button state-filtered",
            ButtonState::Selected => "emoji-button state-selected",
            ButtonState::Urgent => "emoji-button state-urgent",
            ButtonState::Occupied => "emoji-button state-occupied",
            ButtonState::Default => "emoji-button state-default",
        }
    }
}

fn get_button_class(index: usize, tags: &[TagState]) -> &'static str {
    tags.get(index)
        .map(|tag| {
            ButtonState::from_flags(tag.filled, tag.selected, tag.urgent, tag.occupied)
                .to_css_class()
        })
        .unwrap_or("emoji-button state-default")
}

fn shared_path_from_environment() -> String {
    env::args()
        .nth(1)
        .or_else(|| env::var("SHARED_MEMORY_PATH").ok())
        .unwrap_or_default()
}

fn main() {
    let shared_path = shared_path_from_environment();
    if let Err(e) = initialize_logging("dioxus_bar", &shared_path) {
        error!("Failed to initialize logging: {}", e);
        std::process::exit(1);
    }
    info!("=== Environment Debug Info ===");
    info!("DISPLAY: {:?}", env::var("DISPLAY"));
    info!("WAYLAND_DISPLAY: {:?}", env::var("WAYLAND_DISPLAY"));
    info!("XDG_SESSION_TYPE: {:?}", env::var("XDG_SESSION_TYPE"));
    info!("DESKTOP_SESSION: {:?}", env::var("DESKTOP_SESSION"));
    info!("XDG_CURRENT_DESKTOP: {:?}", env::var("XDG_CURRENT_DESKTOP"));
    match CommandRunner::output(&CommandSpec::new("xrandr").with_arg("--current")) {
        Ok(output) => {
            let output_str = String::from_utf8_lossy(&output.stdout);
            for line in output_str.lines() {
                if line.contains("primary") || line.contains("*") {
                    info!("Screen info: {}", line.trim());
                }
            }
        }
        Err(error) => log::debug!("xrandr diagnostics unavailable: {error}"),
    }
    info!("Starting dioxus_bar v{}", 1.0);

    // Transparency has to be decided before the window exists, so the
    // compositor check happens here rather than anywhere reactive.
    let compositor_active = compositor_active();
    let _ = TRANSLUCENT_AT_LAUNCH.set(compositor_active);
    info!("Compositor active at launch: {compositor_active}");

    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            Config::new().with_window(
                WindowBuilder::new()
                    .with_title("dioxus_bar")
                    .with_position(LogicalPosition::new(0, 0))
                    .with_inner_size(LogicalSize::new(800.0, BAR_LOGICAL_HEIGHT))
                    .with_maximizable(false)
                    .with_minimizable(false)
                    .with_resizable(true)
                    .with_always_on_top(true)
                    .with_visible_on_all_workspaces(true)
                    .with_decorations(false)
                    .with_transparent(compositor_active),
            ),
        )
        .launch(App);
}

#[component]
fn App() -> Element {
    let shared_path = shared_path_from_environment();

    let window = use_window();
    let window_baseline = {
        let window = window.clone();
        use_hook(move || WindowBaseline {
            position: window.outer_position().ok(),
            logical_size: window.inner_size().to_logical::<f64>(window.scale_factor()),
        })
    };
    let mut scale_factor = use_signal(|| window.scale_factor());
    // The launch decision is provisional until the depth probe below has seen
    // the window the toolkit actually produced.
    let translucent_at_launch = TRANSLUCENT_AT_LAUNCH.get().copied().unwrap_or(false);
    let mut mode_css = use_signal(|| mode_style(translucent_at_launch));
    let mut pressed_button = use_signal(|| None::<usize>);
    let runtime = use_signal(move || Arc::new(Mutex::new(RuntimeOwner::new(shared_path))));
    let initial_runtime = runtime.read().clone();
    let mut bar_snapshot = use_signal(move || {
        initial_runtime
            .lock()
            .expect("new dioxus bar runtime mutex is healthy")
            .snapshot()
    });

    // GTK only honors `with_transparent` when it finds an RGBA visual, and it
    // falls back silently. By the time effects run the window is realized, so
    // its actual depth settles the mode: no alpha channel means solid paint.
    use_effect({
        let window = window.clone();
        move || {
            if translucent_at_launch && !window_is_argb(&window) {
                mode_css.set(mode_style(false));
            }
        }
    });

    // Core owns every provider and the clock. Only its owned projection
    // crosses into Dioxus reactive state.
    use_effect({
        let runtime = runtime.read().clone();
        let window = window.clone();
        move || {
            let runtime = Arc::clone(&runtime);
            let window = window.clone();
            let mut observed_scale_factor = window.scale_factor();
            spawn(async move {
                loop {
                    let result = runtime.lock().ok().map(|mut runtime| runtime.tick());
                    if let Some((update, snapshot)) = result {
                        handle_runtime_update(update, &window, window_baseline);

                        let current_scale_factor = window.scale_factor();
                        if current_scale_factor.to_bits() != observed_scale_factor.to_bits() {
                            observed_scale_factor = current_scale_factor;
                            scale_factor.set(current_scale_factor);
                            snapshot.geometry.map_or_else(
                                || restore_window_baseline(window_baseline, &window),
                                |geometry| apply_monitor_geometry(geometry, &window),
                            );
                        }

                        bar_snapshot.set(snapshot);
                    } else {
                        error!("xbar runtime mutex was poisoned");
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            });
        }
    });

    // Polling is nonblocking. It also keeps the transport lifecycle in this
    // owner, so a replaced shared-memory endpoint is rediscovered promptly.
    use_effect({
        let runtime = runtime.read().clone();
        let window = window.clone();
        move || {
            let runtime = Arc::clone(&runtime);
            let window = window.clone();
            spawn(async move {
                loop {
                    let result = runtime
                        .lock()
                        .ok()
                        .map(|mut runtime| runtime.poll_transport());
                    if let Some((update, snapshot)) = result {
                        let needs_redraw = update.needs_redraw();
                        handle_runtime_update(update, &window, window_baseline);
                        if needs_redraw {
                            bar_snapshot.set(snapshot);
                        }
                    } else {
                        error!("xbar runtime mutex was poisoned");
                    }
                    tokio::time::sleep(TRANSPORT_POLL_INTERVAL).await;
                }
            });
        }
    });

    let dispatch_action = {
        let runtime = runtime.read().clone();
        let window = window.clone();
        use_callback(move |action: UserAction| {
            let result = runtime
                .lock()
                .ok()
                .map(|mut runtime| runtime.dispatch(action));
            if let Some((update, snapshot)) = result {
                handle_runtime_update(update, &window, window_baseline);
                bar_snapshot.set(snapshot);
            } else {
                error!("xbar runtime mutex was poisoned");
            }
        })
    };

    let state = bar_snapshot();
    let wm_available = state.wm_available;
    let wm_tags = if state.wm_available {
        state.tags.as_slice()
    } else {
        &[]
    };
    let layout_symbol = if state.wm_available {
        state.layout_symbol.as_str()
    } else {
        "[]="
    };

    let mut handle_button_press = move |index: usize| {
        info!("Button {} pressed", index);
        pressed_button.set(Some(index));
    };

    let mut handle_button_release = move |index: usize| {
        info!("Button {} released", index);
        pressed_button.set(None);
        if wm_available && let Some(tag) = TagId::new(index) {
            dispatch_action.call(UserAction::ViewTag(tag));
        }
    };

    let mut handle_button_leave = move |_index: usize| {
        pressed_button.set(None);
    };

    let toggle_layout_panel = move |_| {
        dispatch_action.call(UserAction::ToggleLayoutSelector);
    };

    let select_layout = move |index: u32| {
        if wm_available {
            dispatch_action.call(UserAction::SetLayout(LayoutId(index)));
        }
    };

    rsx! {
        document::Style { "{STYLE_CSS}" }
        document::Style { "{mode_css}" }

        div { class: "button-row",
            div { class: "buttons-container",
                for (i , icon) in TAG_ICONS.iter().enumerate() {
                    {
                        let base_class = get_button_class(i, wm_tags);
                        let is_pressed = pressed_button() == Some(i);
                        let button_class = if is_pressed {
                            format!("{} pressed", base_class)
                        } else {
                            base_class.to_string()
                        };
                        rsx! {
                            button {
                                key: "{i}",
                                class: "{button_class}",
                                title: "Tag {i + 1}",
                                onmousedown: move |_| handle_button_press(i),
                                onmouseup: move |_| handle_button_release(i),
                                onmouseleave: move |_| handle_button_leave(i),
                                onclick: move |_| {
                                    if pressed_button() == Some(i) {
                                        handle_button_release(i);
                                    }
                                },
                                span { class: "nf-icon", "{icon}" }
                            }
                        }
                    }
                }

                div { class: "layout-controls",
                    {
                        let toggle_class = format!(
                            "pill layout-toggle {}",
                            if state.layout_selector_open { "open" } else { "closed" },
                        );
                        rsx! {
                            div { class: "{toggle_class}", onclick: toggle_layout_panel, "{layout_symbol}" }
                        }
                    }

                    if state.layout_selector_open {
                        {
                            let lo0 = format!(
                                "pill layout-option {}",
                                if layout_symbol.contains("[]=") { "current" } else { "" },
                            );
                            let lo1 = format!(
                                "pill layout-option {}",
                                if layout_symbol.contains("><>") { "current" } else { "" },
                            );
                            let lo2 = format!(
                                "pill layout-option {}",
                                if layout_symbol.contains("[M]") { "current" } else { "" },
                            );
                            rsx! {
                                div { class: "layout-selector",
                                    div { class: "{lo0}", onclick: move |_| select_layout(0), "[]=" }
                                    div { class: "{lo1}", onclick: move |_| select_layout(1), "><>" }
                                    div { class: "{lo2}", onclick: move |_| select_layout(2), "[M]" }
                                }
                            }
                        }
                    }
                }
            }

            div { class: "spacer" }

            div { class: "right-info-container",
                SystemInfoDisplay {
                    snapshot: state.system.cpu_percent.is_some().then(|| state.system_details.clone()),
                    battery_percent: state.battery.percent.map(|value| value.rounded()),
                    is_charging: state.battery.charging,
                }
                BrightnessControl {
                    percent: state.brightness.percent,
                    on_action: move |action| dispatch_action.call(action),
                }
                VolumeControl {
                    snapshot: state.audio_device.clone(),
                    on_action: move |action| dispatch_action.call(action),
                }
                ShellButton {
                    wm_available,
                    on_action: move |action| dispatch_action.call(action),
                }
                ScreenshotButton { on_action: move |action| dispatch_action.call(action) }

                div {
                    class: "pill time-pill",
                    onclick: move |_| dispatch_action.call(UserAction::ToggleSeconds),
                    span { class: "nf-icon", "{ICON_TIME}" }
                    span { "{state.time}" }
                }

                div { class: "pill monitor-pill",
                    span { class: "nf-icon", "{ICON_MON}" }
                    {monitor_icon_glyph(state.monitor.0).to_string()}
                }

                div { class: "pill scale-pill", {format!("s: {:.2}", scale_factor())} }
            }
        }
    }
}
