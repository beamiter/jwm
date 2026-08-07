use std::{cell::Cell, rc::Rc};

use gloo_console::error;
use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;
use wasm_bindgen::prelude::*;
use web_sys::{MouseEvent, WheelEvent};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke, catch)]
    async fn tauri_invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "event"], js_name = listen, catch)]
    async fn tauri_listen(
        event: &str,
        handler: &Closure<dyn FnMut(JsValue)>,
    ) -> Result<JsValue, JsValue>;

    type TauriWindow;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "window"], js_name = getCurrentWindow)]
    fn get_current_window() -> TauriWindow;

    #[wasm_bindgen(method, js_name = scaleFactor, catch)]
    async fn scale_factor(this: &TauriWindow) -> Result<JsValue, JsValue>;
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct TagState {
    selected: bool,
    urgent: bool,
    filled: bool,
    occupied: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct AudioDeviceInfo {
    name: String,
    volume: i32,
    is_muted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
struct SystemDetails {
    cpu_average: f32,
    memory_used: u64,
    memory_total: u64,
    memory_usage_percent: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
struct BrightnessState {
    percent: Option<f32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
struct BatteryState {
    percent: Option<f32>,
    charging: bool,
    present: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct BarSnapshot {
    tags: Vec<TagState>,
    monitor: i32,
    layout_symbol: String,
    client_name: String,
    time: String,
    show_seconds: bool,
    layout_selector_open: bool,
    audio_device: Option<AudioDeviceInfo>,
    system_details: SystemDetails,
    brightness: BrightnessState,
    battery: BatteryState,
}

#[derive(Deserialize)]
struct FrontendEnvelope {
    revision: u64,
    snapshot: BarSnapshot,
}

#[derive(Deserialize)]
struct EventPayload<T> {
    payload: T,
}

#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ActionRequest {
    ViewTagOn { tag_index: usize, monitor_id: i32 },
    ToggleLayoutSelector,
    SetLayoutOn { layout_id: u32, monitor_id: i32 },
    ToggleSeconds,
    ToggleMute,
    AdjustVolume { delta: i32 },
    AdjustBrightness { delta: i32 },
    Screenshot,
    OpenShellHub { route: ShellRoute },
}

/// Pages of JWM's own shell surface. The bar renders none of them: each entry
/// is one request naming the page the window manager should open.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ShellRoute {
    Hub,
    Applications,
    Notifications,
    Clipboard,
    Calendar,
    Wallpaper,
}

const SHELL_ROUTES: [(ShellRoute, &str, &str); 6] = [
    (ShellRoute::Hub, "\u{F0F2A}", "Shell Hub"),
    (ShellRoute::Applications, "\u{F0D22}", "Applications"),
    (ShellRoute::Notifications, "\u{F009A}", "Notifications"),
    (ShellRoute::Clipboard, "\u{F0192}", "Clipboard"),
    (ShellRoute::Calendar, "\u{F00ED}", "Calendar"),
    (ShellRoute::Wallpaper, "\u{F02E9}", "Wallpaper"),
];

#[derive(Serialize)]
struct DispatchArgs {
    request: ActionRequest,
}

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

fn button_class(tag: &TagState) -> &'static str {
    if tag.filled {
        "emoji-button state-filtered"
    } else if tag.selected {
        "emoji-button state-selected"
    } else if tag.urgent {
        "emoji-button state-urgent"
    } else if tag.occupied {
        "emoji-button state-occupied"
    } else {
        "emoji-button state-default"
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes == 0 {
        return "0B".to_owned();
    }
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let index = ((bytes as f64).ln() / 1024_f64.ln()).floor() as usize;
    let index = index.min(UNITS.len() - 1);
    let size = bytes as f64 / 1024_f64.powi(index as i32);
    if index == 0 {
        format!("{size:.0}{}", UNITS[index])
    } else {
        format!("{size:.1}{}", UNITS[index])
    }
}

fn monitor_icon(monitor: i32) -> String {
    match monitor {
        0 => "\u{F02DA}".to_owned(),
        1 => "\u{F02DB}".to_owned(),
        _ => format!("M{monitor}"),
    }
}

fn severity(percent: f32) -> &'static str {
    if percent <= 30.0 {
        "usage-good"
    } else if percent <= 60.0 {
        "usage-warn"
    } else if percent <= 80.0 {
        "usage-caution"
    } else {
        "usage-danger"
    }
}

fn volume_icon(device: Option<&AudioDeviceInfo>) -> &'static str {
    match device {
        None => ICON_VOL_MUTE,
        Some(device) if device.is_muted || device.volume <= 0 => ICON_VOL_MUTE,
        Some(device) if device.volume < 34 => ICON_VOL_LOW,
        Some(device) if device.volume < 67 => ICON_VOL_MID,
        Some(_) => ICON_VOL_HIGH,
    }
}

fn dispatch_args(request: ActionRequest) -> JsValue {
    serde_wasm_bindgen::to_value(&DispatchArgs { request }).unwrap_or(JsValue::NULL)
}

fn dispatch_action(request: ActionRequest) {
    let args = dispatch_args(request);
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(error) = tauri_invoke("dispatch_action", args).await {
            error!(format!("dispatch_action failed: {error:?}"));
        }
    });
}

#[component]
fn App() -> impl IntoView {
    let (snapshot, set_snapshot) = signal(None::<BarSnapshot>);
    let (scale_factor, set_scale_factor) = signal(None::<f64>);
    let (pressed, set_pressed) = signal(None::<usize>);
    let (is_taking, set_is_taking) = signal(false);

    Effect::new(move |_| {
        let latest_revision = Rc::new(Cell::new(None::<u64>));
        let callback_revision = Rc::clone(&latest_revision);
        let state_callback = Closure::<dyn FnMut(JsValue)>::new(move |event: JsValue| {
            match serde_wasm_bindgen::from_value::<EventPayload<FrontendEnvelope>>(event) {
                Ok(event) => {
                    let envelope = event.payload;
                    if callback_revision
                        .get()
                        .is_some_and(|revision| envelope.revision < revision)
                    {
                        return;
                    }
                    callback_revision.set(Some(envelope.revision));
                    set_snapshot.set(Some(envelope.snapshot));
                }
                Err(error) => error!(format!("failed to decode xbar-state: {error}")),
            }
        });

        wasm_bindgen_futures::spawn_local(async move {
            let registration = async {
                tauri_listen("xbar-state", &state_callback).await?;

                let window = get_current_window();
                match window.scale_factor().await {
                    Ok(value) => set_scale_factor.set(value.as_f64()),
                    Err(error) => error!(format!("failed to query scale factor: {error:?}")),
                }

                tauri_invoke("frontend_ready", JsValue::NULL).await?;
                Ok::<(), JsValue>(())
            }
            .await;
            if let Err(error) = registration {
                error!(format!("failed to initialize xbar Tauri bridge: {error:?}"));
            }
            state_callback.forget();
        });
    });

    let take_screenshot = move |_| {
        if is_taking.get() {
            return;
        }
        set_is_taking.set(true);
        let args = dispatch_args(ActionRequest::Screenshot);
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(error) = tauri_invoke("dispatch_action", args).await {
                error!(format!("screenshot failed: {error:?}"));
            }
            gloo_timers::future::TimeoutFuture::new(500).await;
            set_is_taking.set(false);
        });
    };

    view! {
        <Show
            when=move || snapshot.get().is_some()
            fallback=|| view! { <div class="button-row">"Loading..."</div> }
        >
            {move || {
                let current = snapshot.get().expect("snapshot is present inside Show");
                let monitor = current.monitor;
                let tags = current.tags;
                let layout_symbol = current.layout_symbol;
                let layout_open = current.layout_selector_open;
                let system = current.system_details;
                let battery = current.battery;
                let audio = current.audio_device;
                let brightness = current.brightness.percent;
                let time = current.time;
                let show_seconds = current.show_seconds;
                let monitor_title = if current.client_name.is_empty() {
                    "显示器".to_owned()
                } else {
                    current.client_name
                };

                let cpu_class = format!("pill usage-pill {}", severity(system.cpu_average));
                let memory_class =
                    format!("pill usage-pill {}", severity(system.memory_usage_percent));
                let memory_title = format!(
                    "内存使用: {} / {}",
                    format_bytes(system.memory_used),
                    format_bytes(system.memory_total),
                );

                let battery_percent = if battery.present { battery.percent } else { None };
                let battery_class = format!(
                    "pill usage-pill {}",
                    match battery_percent {
                        None => "usage-warn",
                        Some(percent) if percent > 50.0 => "usage-good",
                        Some(percent) if percent > 20.0 => "usage-warn",
                        Some(_) => "usage-danger",
                    },
                );
                let battery_icon = if battery.charging { ICON_BAT_CHG } else { ICON_BAT_FULL };
                let battery_title = match battery_percent {
                    None => "未检测到电池".to_owned(),
                    Some(percent) if battery.charging => format!("电池充电中: {percent:.1}%"),
                    Some(percent) => format!("电池电量: {percent:.1}%"),
                };
                let battery_label = battery_percent
                    .map_or_else(|| " --".to_owned(), |percent| format!(" {percent:.0}%"));

                let audio_muted = audio.as_ref().is_none_or(|device| device.is_muted);
                let audio_class = if audio_muted {
                    "pill volume-pill muted"
                } else {
                    "pill volume-pill"
                };
                let audio_icon = volume_icon(audio.as_ref());
                let audio_label = audio
                    .as_ref()
                    .map_or_else(|| " --".to_owned(), |device| format!(" {}%", device.volume));
                let audio_title = audio.as_ref().map_or_else(
                    || "左键静音 / 滚轮调节".to_owned(),
                    |device| device.name.clone(),
                );
                let brightness_label = brightness
                    .map_or_else(|| " --".to_owned(), |percent| format!(" {percent:.0}%"));
                let time_title = if show_seconds { "点击隐藏秒" } else { "点击显示秒" };
                let layout_toggle_class = if layout_open {
                    "pill layout-toggle open"
                } else {
                    "pill layout-toggle closed"
                };

                view! {
                    <div class="button-row">
                        <div class="buttons-container">
                            {
                                TAG_ICONS.iter().enumerate().map(|(index, icon)| {
                                    let tag = tags.get(index).cloned().unwrap_or_default();
                                    let base_class = button_class(&tag);
                                    let class = move || {
                                        if pressed.get() == Some(index) {
                                            format!("{base_class} pressed")
                                        } else {
                                            base_class.to_owned()
                                        }
                                    };
                                    view! {
                                        <button
                                            class=class
                                            on:mousedown=move |_| set_pressed.set(Some(index))
                                            on:mouseup=move |_| {
                                                set_pressed.set(None);
                                                dispatch_action(ActionRequest::ViewTagOn {
                                                    tag_index: index,
                                                    monitor_id: monitor,
                                                });
                                            }
                                            on:mouseleave=move |_| set_pressed.set(None)
                                            title=format!("Tag {}", index + 1)
                                        >
                                            <span class="nf-icon">{*icon}</span>
                                        </button>
                                    }
                                }).collect_view()
                            }

                            <div class="layout-controls">
                                <div
                                    class=layout_toggle_class
                                    on:click=move |_| {
                                        dispatch_action(ActionRequest::ToggleLayoutSelector)
                                    }
                                    title="切换布局"
                                >
                                    {layout_symbol.clone()}
                                </div>
                                <Show when=move || layout_open fallback=|| ()>
                                    <div class="layout-selector">
                                        {
                                            [("[]=", 0_u32), ("><>", 1_u32), ("[M]", 2_u32)]
                                                .into_iter()
                                                .map(|(label, layout_id)| {
                                                    let class = if layout_symbol == label {
                                                        "pill layout-option current"
                                                    } else {
                                                        "pill layout-option"
                                                    };
                                                    view! {
                                                        <div
                                                            class=class
                                                            on:click=move |_| {
                                                                dispatch_action(ActionRequest::SetLayoutOn {
                                                                    layout_id,
                                                                    monitor_id: monitor,
                                                                });
                                                            }
                                                        >
                                                            {label}
                                                        </div>
                                                    }
                                                })
                                                .collect_view()
                                        }
                                    </div>
                                </Show>
                            </div>
                        </div>

                        <div class="spacer"></div>

                        <div class="right-info-container">
                            <div class="system-info-container">
                                <div class=cpu_class title="CPU 平均使用率">
                                    <span class="nf-icon">{ICON_CPU}</span>
                                    {format!(" {:.0}%", system.cpu_average)}
                                </div>
                                <div class=memory_class title=memory_title>
                                    <span class="nf-icon">{ICON_MEM}</span>
                                    {format!(" {:.0}%", system.memory_usage_percent)}
                                </div>
                                <div class=battery_class title=battery_title>
                                    <span class="nf-icon">{battery_icon}</span>
                                    {battery_label}
                                </div>
                            </div>

                            <div
                                class="pill brightness-pill"
                                on:click=move |_| {
                                    dispatch_action(ActionRequest::AdjustBrightness { delta: 5 })
                                }
                                on:wheel=move |event: WheelEvent| {
                                    event.prevent_default();
                                    let delta = if event.delta_y() < 0.0 { 5 } else { -5 };
                                    dispatch_action(ActionRequest::AdjustBrightness { delta });
                                }
                                on:contextmenu=move |event: MouseEvent| {
                                    event.prevent_default();
                                    dispatch_action(ActionRequest::AdjustBrightness { delta: -5 });
                                }
                                title="左键加亮 / 右键减暗 / 滚轮调节"
                            >
                                <span class="nf-icon">{ICON_BRIGHT}</span>
                                {brightness_label}
                            </div>

                            <div
                                class=audio_class
                                on:click=move |_| dispatch_action(ActionRequest::ToggleMute)
                                on:wheel=move |event: WheelEvent| {
                                    event.prevent_default();
                                    let delta = if event.delta_y() < 0.0 { 5 } else { -5 };
                                    dispatch_action(ActionRequest::AdjustVolume { delta });
                                }
                                title=audio_title
                            >
                                <span class="nf-icon">{audio_icon}</span>
                                {audio_label}
                            </div>

                            // JWM shell entry. Hovering reveals the pages;
                            // clicking the pill opens the hub, which is itself
                            // the page that lists the rest.
                            <div class="shell-menu">
                                <div
                                    class="pill shell-pill"
                                    on:click=move |_| dispatch_action(
                                        ActionRequest::OpenShellHub { route: ShellRoute::Hub },
                                    )
                                    title="JWM shell"
                                >
                                    <span class="nf-icon">{SHELL_ROUTES[0].1}</span>
                                </div>
                                <div class="shell-dropdown">
                                    {SHELL_ROUTES
                                        .into_iter()
                                        .map(|(route, icon, label)| {
                                            view! {
                                                <div
                                                    class="shell-route"
                                                    on:click=move |_| dispatch_action(
                                                        ActionRequest::OpenShellHub { route },
                                                    )
                                                >
                                                    <span class="nf-icon">{icon}</span>
                                                    <span>{label}</span>
                                                </div>
                                            }
                                        })
                                        .collect_view()}
                                </div>
                            </div>

                            <div
                                class=move || {
                                    if is_taking.get() {
                                        "pill screenshot-pill taking"
                                    } else {
                                        "pill screenshot-pill"
                                    }
                                }
                                on:click=take_screenshot
                                title="截图 (jwm)"
                            >
                                <span class="nf-icon">{ICON_SHOT}</span>
                            </div>

                            <div
                                class="pill time-pill"
                                on:click=move |_| dispatch_action(ActionRequest::ToggleSeconds)
                                title=time_title
                            >
                                <span class="nf-icon">{ICON_TIME}</span>
                                {format!(" {time}")}
                            </div>

                            <div class="pill monitor-pill" title=monitor_title>
                                <span class="nf-icon">{ICON_MON}</span>
                                {format!(" {}", monitor_icon(monitor))}
                            </div>

                            <div class="pill scale-pill" title="Scale Factor">
                                {move || scale_factor.get().map_or_else(
                                    || "s: --".to_owned(),
                                    |scale| format!("s: {scale:.2}"),
                                )}
                            </div>
                        </div>
                    </div>
                }
            }}
        </Show>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to(
        web_sys::window()
            .expect("browser window is available")
            .document()
            .expect("browser document is available")
            .get_element_by_id("root")
            .expect("root element is available")
            .unchecked_into(),
        App,
    )
    .forget();
}
