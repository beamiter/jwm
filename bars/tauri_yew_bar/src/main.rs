use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

use gloo_console::error;
use gloo_timers::callback::{Interval, Timeout};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;
use wasm_bindgen::prelude::*;
use yew::prelude::*;

thread_local! {
    static PREVIEW_RENEWALS: RefCell<HashMap<u64, Interval>> = RefCell::new(HashMap::new());
    static BAR_ORIGIN: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
    static WM_SESSION_ID: Cell<u64> = const { Cell::new(0) };
    static DOCK_GEOMETRY_RETRY: RefCell<Option<Timeout>> = const { RefCell::new(None) };
}

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

    #[wasm_bindgen(method, js_name = innerPosition, catch)]
    async fn inner_position(this: &TauriWindow) -> Result<JsValue, JsValue>;
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
struct MinimizedWindow {
    token: u64,
    monitor: i32,
    title: String,
    app_id: String,
    #[serde(default)]
    flags: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct DockGeometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Deserialize)]
struct PhysicalPosition {
    x: i32,
    y: i32,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
struct BarSnapshot {
    wm_available: bool,
    #[serde(default)]
    wm_session_id: u64,
    #[serde(default)]
    geometry: Option<DockGeometry>,
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
    #[serde(default)]
    minimized_windows: Vec<MinimizedWindow>,
    #[serde(default)]
    minimized_overflow: bool,
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
    ViewTagOn {
        tag_index: usize,
        monitor_id: i32,
    },
    ToggleLayoutSelector,
    SetLayoutOn {
        layout_id: u32,
        monitor_id: i32,
    },
    ToggleSeconds,
    ToggleMute,
    AdjustVolume {
        delta: i32,
    },
    AdjustBrightness {
        delta: i32,
    },
    Screenshot,
    RestoreWindow {
        wm_session_id: u64,
        window_id: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        geometry: Option<DockGeometry>,
    },
    PreviewWindow {
        wm_session_id: u64,
        window_id: u64,
        visible: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        geometry: Option<DockGeometry>,
    },
    SetDockGeometry {
        wm_session_id: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        window_id: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        geometry: Option<DockGeometry>,
    },
    OpenShellHub {
        route: ShellRoute,
    },
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

fn minimized_label(window: &MinimizedWindow) -> String {
    let title = window.title.trim();
    if !title.is_empty() {
        title.to_owned()
    } else {
        let app_id = window.app_id.trim();
        if app_id.is_empty() {
            "Minimized window".to_owned()
        } else {
            app_id.to_owned()
        }
    }
}

fn minimized_initial(window: &MinimizedWindow) -> String {
    window
        .app_id
        .trim()
        .chars()
        .next()
        .or_else(|| window.title.trim().chars().next())
        .unwrap_or('•')
        .to_uppercase()
        .collect()
}

async fn window_metrics() -> Option<(PhysicalPosition, f64)> {
    let window = get_current_window();
    let scale = window.scale_factor().await.ok()?.as_f64()?;
    if let Some((x, y)) = BAR_ORIGIN.with(Cell::get) {
        return Some((PhysicalPosition { x, y }, scale));
    }
    let origin = window.inner_position().await.ok()?;
    let origin = serde_wasm_bindgen::from_value(origin).ok()?;
    Some((origin, scale))
}

fn project_dock_geometry(
    left: f64,
    top: f64,
    width: f64,
    height: f64,
    origin: &PhysicalPosition,
    scale: f64,
) -> DockGeometry {
    let x = f64::from(origin.x) + left * scale;
    let y = f64::from(origin.y) + top * scale;
    let width = (width * scale).round().clamp(0.0, f64::from(u32::MAX));
    let height = (height * scale).round().clamp(0.0, f64::from(u32::MAX));
    DockGeometry {
        x: x.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32,
        y: y.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32,
        width: width as u32,
        height: height as u32,
    }
}

fn physical_geometry(
    element: &web_sys::Element,
    origin: &PhysicalPosition,
    scale: f64,
) -> DockGeometry {
    let rect = element.get_bounding_client_rect();
    project_dock_geometry(
        rect.left(),
        rect.top(),
        rect.width(),
        rect.height(),
        origin,
        scale,
    )
}

fn resting_item_geometry(
    item: &web_sys::HtmlElement,
    dock: &web_sys::Element,
    origin: &PhysicalPosition,
    scale: f64,
) -> DockGeometry {
    let dock_rect = dock.get_bounding_client_rect();
    project_dock_geometry(
        dock_rect.left() + f64::from(item.offset_left()),
        dock_rect.top() + f64::from(item.offset_top()),
        f64::from(item.offset_width()),
        f64::from(item.offset_height()),
        origin,
        scale,
    )
}

fn event_element(event: &MouseEvent) -> Option<web_sys::Element> {
    event.current_target()?.dyn_into().ok()
}

fn restore_minimized(window_id: u64, event: MouseEvent) {
    let Some(element) = event_element(&event) else {
        return;
    };
    let wm_session_id = WM_SESSION_ID.with(Cell::get);
    wasm_bindgen_futures::spawn_local(async move {
        let geometry = window_metrics()
            .await
            .map(|(origin, scale)| physical_geometry(&element, &origin, scale));
        dispatch_action(ActionRequest::RestoreWindow {
            wm_session_id,
            window_id,
            geometry,
        });
    });
}

fn preview_element(window_id: u64, visible: bool, element: web_sys::Element) {
    let wm_session_id = WM_SESSION_ID.with(Cell::get);
    wasm_bindgen_futures::spawn_local(async move {
        let geometry = window_metrics()
            .await
            .map(|(origin, scale)| physical_geometry(&element, &origin, scale));
        if visible && !element.matches(":hover").unwrap_or(false) {
            return;
        }
        dispatch_action(ActionRequest::PreviewWindow {
            wm_session_id,
            window_id,
            visible,
            geometry,
        });
    });
}

fn begin_preview(window_id: u64, event: MouseEvent) {
    let Some(element) = event_element(&event) else {
        return;
    };
    PREVIEW_RENEWALS.with(|renewals| {
        renewals.borrow_mut().remove(&window_id);
    });
    preview_element(window_id, true, element.clone());
    let renewal_element = element;
    let renewal = Interval::new(2_000, move || {
        if renewal_element.matches(":hover").unwrap_or(false) {
            preview_element(window_id, true, renewal_element.clone());
        }
    });
    PREVIEW_RENEWALS.with(|renewals| {
        renewals.borrow_mut().insert(window_id, renewal);
    });
}

fn end_preview(window_id: u64, event: MouseEvent) {
    PREVIEW_RENEWALS.with(|renewals| {
        renewals.borrow_mut().remove(&window_id);
    });
    if let Some(element) = event_element(&event) {
        preview_element(window_id, false, element);
    }
}

fn dock_retry_allowed(expected_session: u64, current_session: u64, dock_connected: bool) -> bool {
    expected_session != 0 && expected_session == current_session && dock_connected
}

fn cancel_dock_geometry_retry() {
    DOCK_GEOMETRY_RETRY.with(|retry| {
        retry.borrow_mut().take();
    });
}

fn schedule_dock_geometry_retry(wm_session_id: u64) {
    let connected = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.query_selector(".minimized-dock").ok().flatten())
        .is_some_and(|dock| dock.is_connected());
    if !dock_retry_allowed(wm_session_id, WM_SESSION_ID.with(Cell::get), connected) {
        return;
    }
    DOCK_GEOMETRY_RETRY.with(|retry| {
        let mut retry = retry.borrow_mut();
        if retry.is_some() {
            return;
        }
        *retry = Some(Timeout::new(100, move || {
            cancel_dock_geometry_retry();
            let connected = web_sys::window()
                .and_then(|window| window.document())
                .and_then(|document| document.query_selector(".minimized-dock").ok().flatten())
                .is_some_and(|dock| dock.is_connected());
            if dock_retry_allowed(wm_session_id, WM_SESSION_ID.with(Cell::get), connected) {
                start_dock_geometry_publish(wm_session_id, false);
            }
        }));
    });
}

fn start_dock_geometry_publish(wm_session_id: u64, defer_one_turn: bool) {
    wasm_bindgen_futures::spawn_local(async move {
        if defer_one_turn {
            gloo_timers::future::TimeoutFuture::new(0).await;
        }
        let Some(document) = web_sys::window().and_then(|window| window.document()) else {
            return;
        };
        let Some(dock) = document.query_selector(".minimized-dock").ok().flatten() else {
            return;
        };
        let Some((origin, scale)) = window_metrics().await else {
            schedule_dock_geometry_retry(wm_session_id);
            return;
        };
        if !dock_retry_allowed(
            wm_session_id,
            WM_SESSION_ID.with(Cell::get),
            dock.is_connected(),
        ) {
            return;
        }
        if let Err(error) = dispatch_action_result(ActionRequest::SetDockGeometry {
            wm_session_id,
            window_id: None,
            geometry: Some(physical_geometry(&dock, &origin, scale)),
        })
        .await
        {
            error!(format!(
                "failed to publish minimized Dock geometry; retrying: {error:?}"
            ));
            schedule_dock_geometry_retry(wm_session_id);
            return;
        }
        let Ok(items) = dock.query_selector_all("[data-window-id]") else {
            return;
        };
        for index in 0..items.length() {
            let item = items
                .item(index)
                .and_then(|item| item.dyn_into::<web_sys::HtmlElement>().ok());
            let Some(item) = item else {
                continue;
            };
            let Some(window_id) = item
                .get_attribute("data-window-id")
                .and_then(|value| value.parse::<u64>().ok())
            else {
                continue;
            };
            if let Err(error) = dispatch_action_result(ActionRequest::SetDockGeometry {
                wm_session_id,
                window_id: Some(window_id),
                geometry: Some(resting_item_geometry(&item, &dock, &origin, scale)),
            })
            .await
            {
                error!(format!(
                    "failed to publish minimized Dock item geometry; retrying: {error:?}"
                ));
                schedule_dock_geometry_retry(wm_session_id);
                return;
            }
        }
    });
}

fn publish_dock_geometry_later() {
    cancel_dock_geometry_retry();
    start_dock_geometry_publish(WM_SESSION_ID.with(Cell::get), true);
}

fn install_geometry_resize_listener() {
    let callback = Closure::<dyn FnMut()>::new(publish_dock_geometry_later);
    if let Some(window) = web_sys::window() {
        let _ =
            window.add_event_listener_with_callback("resize", callback.as_ref().unchecked_ref());
        callback.forget();
    }
}

fn dispatch_args(request: ActionRequest) -> JsValue {
    serde_wasm_bindgen::to_value(&DispatchArgs { request }).unwrap_or(JsValue::NULL)
}

async fn dispatch_action_result(request: ActionRequest) -> Result<(), JsValue> {
    tauri_invoke("dispatch_action", dispatch_args(request))
        .await
        .map(|_| ())
}

fn dispatch_action(request: ActionRequest) {
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(error) = dispatch_action_result(request).await {
            error!(format!("dispatch_action failed: {error:?}"));
        }
    });
}

#[function_component(App)]
fn app() -> Html {
    let snapshot = use_state(|| None::<BarSnapshot>);
    let scale_factor = use_state(|| None::<f64>);
    let pressed = use_state(|| None::<usize>);
    let is_taking = use_state(|| false);

    {
        let snapshot = snapshot.clone();
        let scale_factor = scale_factor.clone();
        use_effect_with((), move |_| {
            let latest_revision = Rc::new(Cell::new(None::<u64>));
            let callback_revision = Rc::clone(&latest_revision);
            let dock_signature = Rc::new(RefCell::new(String::new()));
            let callback_dock_signature = Rc::clone(&dock_signature);
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
                        let session_changed = WM_SESSION_ID.with(|session| {
                            let changed = session.get() != envelope.snapshot.wm_session_id;
                            session.set(envelope.snapshot.wm_session_id);
                            changed
                        });
                        if session_changed {
                            cancel_dock_geometry_retry();
                            PREVIEW_RENEWALS.with(|renewals| renewals.borrow_mut().clear());
                        }
                        BAR_ORIGIN.with(|origin| {
                            origin.set(
                                envelope
                                    .snapshot
                                    .geometry
                                    .map(|geometry| (geometry.x, geometry.y)),
                            );
                        });
                        let signature = format!(
                            "{}|{:?}|{}|{}",
                            envelope.snapshot.wm_session_id,
                            envelope.snapshot.geometry,
                            envelope
                                .snapshot
                                .minimized_windows
                                .iter()
                                .map(|window| window.token.to_string())
                                .collect::<Vec<_>>()
                                .join(","),
                            envelope.snapshot.minimized_overflow,
                        );
                        let geometry_changed = *callback_dock_signature.borrow() != signature;
                        if geometry_changed {
                            *callback_dock_signature.borrow_mut() = signature;
                        }
                        PREVIEW_RENEWALS.with(|renewals| {
                            renewals.borrow_mut().retain(|window_id, _| {
                                envelope
                                    .snapshot
                                    .minimized_windows
                                    .iter()
                                    .any(|window| window.token == *window_id)
                            });
                        });
                        snapshot.set(Some(envelope.snapshot));
                        if geometry_changed {
                            publish_dock_geometry_later();
                        }
                    }
                    Err(error) => error!(format!("failed to decode xbar-state: {error}")),
                }
            });

            wasm_bindgen_futures::spawn_local(async move {
                let registration = async {
                    tauri_listen("xbar-state", &state_callback).await?;
                    install_geometry_resize_listener();

                    let window = get_current_window();
                    match window.scale_factor().await {
                        Ok(value) => scale_factor.set(value.as_f64()),
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
            || ()
        });
    }

    let Some(current) = (*snapshot).clone() else {
        return html! { <div class="button-row">{"Loading..."}</div> };
    };

    let wm_available = current.wm_available;
    let monitor = current.monitor;
    let tags = current.tags;
    let layout_symbol = current.layout_symbol;
    let layout_open = current.layout_selector_open;
    let system = current.system_details;
    let battery = current.battery;
    let audio = current.audio_device;
    let brightness = current.brightness.percent;
    let minimized_windows = current.minimized_windows;
    let minimized_overflow = current.minimized_overflow;

    let on_press = {
        let pressed = pressed.clone();
        move |index: usize| {
            let pressed = pressed.clone();
            Callback::from(move |_| pressed.set(Some(index)))
        }
    };
    let on_release = {
        let pressed = pressed.clone();
        move |index: usize| {
            let pressed = pressed.clone();
            Callback::from(move |_: MouseEvent| {
                pressed.set(None);
                dispatch_action(ActionRequest::ViewTagOn {
                    tag_index: index,
                    monitor_id: monitor,
                });
            })
        }
    };
    let on_leave = {
        let pressed = pressed.clone();
        Callback::from(move |_: MouseEvent| pressed.set(None))
    };

    let toggle_layout = Callback::from(move |_: MouseEvent| {
        dispatch_action(ActionRequest::ToggleLayoutSelector);
    });
    let select_layout = move |layout_id: u32| {
        Callback::from(move |_: MouseEvent| {
            dispatch_action(ActionRequest::SetLayoutOn {
                layout_id,
                monitor_id: monitor,
            });
        })
    };

    let take_screenshot = {
        let is_taking = is_taking.clone();
        Callback::from(move |_: MouseEvent| {
            if *is_taking {
                return;
            }
            is_taking.set(true);
            let is_taking = is_taking.clone();
            let args = dispatch_args(ActionRequest::Screenshot);
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(error) = tauri_invoke("dispatch_action", args).await {
                    error!(format!("screenshot failed: {error:?}"));
                }
                gloo_timers::future::TimeoutFuture::new(500).await;
                is_taking.set(false);
            });
        })
    };

    let toggle_seconds = Callback::from(move |_: MouseEvent| {
        dispatch_action(ActionRequest::ToggleSeconds);
    });
    let toggle_mute = Callback::from(move |_: MouseEvent| {
        dispatch_action(ActionRequest::ToggleMute);
    });
    let volume_wheel = Callback::from(move |event: WheelEvent| {
        event.prevent_default();
        let delta = if event.delta_y() < 0.0 { 5 } else { -5 };
        dispatch_action(ActionRequest::AdjustVolume { delta });
    });
    let brightness_click = Callback::from(move |_: MouseEvent| {
        dispatch_action(ActionRequest::AdjustBrightness { delta: 5 });
    });
    let brightness_wheel = Callback::from(move |event: WheelEvent| {
        event.prevent_default();
        let delta = if event.delta_y() < 0.0 { 5 } else { -5 };
        dispatch_action(ActionRequest::AdjustBrightness { delta });
    });
    let brightness_right = Callback::from(move |event: MouseEvent| {
        event.prevent_default();
        dispatch_action(ActionRequest::AdjustBrightness { delta: -5 });
    });

    let layout_toggle_class = if layout_open {
        "pill layout-toggle open"
    } else {
        "pill layout-toggle closed"
    };
    let option_class = |symbol: &str| {
        if layout_symbol == symbol {
            "pill layout-option current"
        } else {
            "pill layout-option"
        }
    };

    let cpu_class = format!("pill usage-pill {}", severity(system.cpu_average));
    let memory_class = format!("pill usage-pill {}", severity(system.memory_usage_percent),);
    let memory_title = format!(
        "内存使用: {} / {}",
        format_bytes(system.memory_used),
        format_bytes(system.memory_total),
    );

    let battery_percent = if battery.present {
        battery.percent
    } else {
        None
    };
    let battery_class = format!(
        "pill usage-pill {}",
        match battery_percent {
            None => "usage-warn",
            Some(percent) if percent > 50.0 => "usage-good",
            Some(percent) if percent > 20.0 => "usage-warn",
            Some(_) => "usage-danger",
        },
    );
    let battery_icon = if battery.charging {
        ICON_BAT_CHG
    } else {
        ICON_BAT_FULL
    };
    let battery_title = match battery_percent {
        None => "未检测到电池".to_owned(),
        Some(percent) if battery.charging => format!("电池充电中: {percent:.1}%"),
        Some(percent) => format!("电池电量: {percent:.1}%"),
    };
    let battery_label =
        battery_percent.map_or_else(|| "--".to_owned(), |percent| format!("{percent:.0}%"));

    let volume_pill_class = if audio.as_ref().is_none_or(|device| device.is_muted) {
        "pill volume-pill muted"
    } else {
        "pill volume-pill"
    };
    let volume_icon = volume_icon(audio.as_ref());
    let volume_label = audio
        .as_ref()
        .map_or_else(|| "--".to_owned(), |device| format!("{}%", device.volume));
    let volume_title = audio.as_ref().map_or_else(
        || "左键静音 / 滚轮调节".to_owned(),
        |device| device.name.clone(),
    );
    let brightness_label =
        brightness.map_or_else(|| "--".to_owned(), |percent| format!("{percent:.0}%"));
    let monitor_title = if current.client_name.is_empty() {
        "显示器".to_owned()
    } else {
        current.client_name
    };
    let time_title = if current.show_seconds {
        "点击隐藏秒"
    } else {
        "点击显示秒"
    };
    let scale_text =
        (*scale_factor).map_or_else(|| "s: --".to_owned(), |scale| format!("s: {scale:.2}"));

    html! {
        <div class="button-row">
            <div class="buttons-container">
                {
                    TAG_ICONS.iter().enumerate().map(|(index, icon)| {
                        let tag = tags.get(index).cloned().unwrap_or_default();
                        let base_class = button_class(&tag);
                        let class = if *pressed == Some(index) {
                            format!("{base_class} pressed")
                        } else {
                            base_class.to_owned()
                        };
                        html! {
                            <button
                                key={index}
                                class={class}
                                onmousedown={on_press(index)}
                                onmouseup={on_release(index)}
                                onmouseleave={on_leave.clone()}
                                title={format!("Tag {}", index + 1)}
                            >
                                <span class="nf-icon">{*icon}</span>
                            </button>
                        }
                    }).collect::<Html>()
                }
                <div class="layout-controls">
                    <div class={layout_toggle_class} onclick={toggle_layout} title="切换布局">
                        {layout_symbol.clone()}
                    </div>
                    if layout_open {
                        <div class="layout-selector">
                            <div class={option_class("[]=")} onclick={select_layout(0)}>{"[]="}</div>
                            <div class={option_class("><>")} onclick={select_layout(1)}>{"><>"}</div>
                            <div class={option_class("[M]")} onclick={select_layout(2)}>{"[M]"}</div>
                        </div>
                    }
                </div>
            </div>

            <div class="spacer"></div>

            <div class="right-info-container">
                <div
                    class={classes!(
                        "minimized-dock",
                        (minimized_windows.is_empty() && !minimized_overflow).then_some("is-empty"),
                    )}
                    aria-label="Minimized windows"
                >
                    <span class="minimized-divider" aria-hidden="true"></span>
                    { for minimized_windows.into_iter().map(|window| {
                        let window_id = window.token;
                        let label = minimized_label(&window);
                        let title = format!("{label} — click to restore");
                        let aria_label = format!("Restore {label}");
                        let initial = minimized_initial(&window);
                        let urgent = window.flags & 2 != 0;
                        let preview_available = window.flags & 1 != 0;
                        let class = classes!("minimized-item", urgent.then_some("is-urgent"));
                        let restore = Callback::from(move |event: MouseEvent| {
                            restore_minimized(window_id, event);
                        });
                        let preview_enter = Callback::from(move |event: MouseEvent| {
                            if preview_available {
                                begin_preview(window_id, event);
                            }
                        });
                        let preview_leave = Callback::from(move |event: MouseEvent| {
                            if preview_available {
                                end_preview(window_id, event);
                            }
                        });
                        html! {
                            <button
                                key={window_id}
                                class={class}
                                data-window-id={window_id.to_string()}
                                disabled={!wm_available}
                                title={title}
                                aria-label={aria_label}
                                onclick={restore}
                                onmouseenter={preview_enter}
                                onmouseleave={preview_leave}
                            >
                                <span class="minimized-thumbnail" aria-hidden="true">
                                    <span class="minimized-traffic-lights"></span>
                                    <span class="minimized-initial">{initial}</span>
                                </span>
                                if urgent {
                                    <span class="minimized-urgent-dot"></span>
                                }
                            </button>
                        }
                    }) }
                    if minimized_overflow {
                        <span class="minimized-overflow" title="More minimized windows">{"…"}</span>
                    }
                </div>
                <div class="system-info-container">
                    <div class={cpu_class} title="CPU 平均使用率">
                        <span class="nf-icon">{ICON_CPU}</span>{format!(" {:.0}%", system.cpu_average)}
                    </div>
                    <div class={memory_class} title={memory_title}>
                        <span class="nf-icon">{ICON_MEM}</span>{format!(" {:.0}%", system.memory_usage_percent)}
                    </div>
                    <div class={battery_class} title={battery_title}>
                        <span class="nf-icon">{battery_icon}</span>{format!(" {battery_label}")}
                    </div>
                </div>

                <div
                    class="pill brightness-pill"
                    onclick={brightness_click}
                    onwheel={brightness_wheel}
                    oncontextmenu={brightness_right}
                    title="左键加亮 / 右键减暗 / 滚轮调节"
                >
                    <span class="nf-icon">{ICON_BRIGHT}</span>{format!(" {brightness_label}")}
                </div>

                <div
                    class={volume_pill_class}
                    onclick={toggle_mute}
                    onwheel={volume_wheel}
                    title={volume_title}
                >
                    <span class="nf-icon">{volume_icon}</span>{format!(" {volume_label}")}
                </div>

                // JWM shell entry. Hovering reveals the pages; clicking the
                // pill opens the hub, which is itself the page listing the rest.
                <div class="shell-menu">
                    <div
                        class="pill shell-pill"
                        onclick={Callback::from(|_| dispatch_action(
                            ActionRequest::OpenShellHub { route: ShellRoute::Hub },
                        ))}
                        title="JWM shell"
                    >
                        <span class="nf-icon">{SHELL_ROUTES[0].1}</span>
                    </div>
                    <div class="shell-dropdown">
                        { for SHELL_ROUTES.iter().map(|&(route, icon, label)| html! {
                            <div
                                class="shell-route"
                                onclick={Callback::from(move |_| dispatch_action(
                                    ActionRequest::OpenShellHub { route },
                                ))}
                            >
                                <span class="nf-icon">{icon}</span>
                                <span>{label}</span>
                            </div>
                        }) }
                    </div>
                </div>

                <div
                    class={if *is_taking { "pill screenshot-pill taking" } else { "pill screenshot-pill" }}
                    onclick={take_screenshot}
                    title="截图 (jwm)"
                >
                    <span class="nf-icon">{ICON_SHOT}</span>
                </div>

                <div class="pill time-pill" onclick={toggle_seconds} title={time_title}>
                    <span class="nf-icon">{ICON_TIME}</span>{format!(" {}", current.time)}
                </div>

                <div class="pill monitor-pill" title={monitor_title}>
                    <span class="nf-icon">{ICON_MON}</span>{format!(" {}", monitor_icon(monitor))}
                </div>

                <div class="pill scale-pill" title="Scale Factor">
                    {scale_text}
                </div>
            </div>
        </div>
    }
}

fn main() {
    let document = web_sys::window()
        .expect("browser window is available")
        .document()
        .expect("browser document is available");
    let root = document
        .get_element_by_id("root")
        .expect("root element is available");
    yew::Renderer::<App>::with_root(root).render();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dock_geometry_projects_negative_origin_at_two_x_scale() {
        assert_eq!(
            project_dock_geometry(
                10.0,
                5.0,
                30.0,
                20.0,
                &PhysicalPosition { x: -300, y: 40 },
                2.0,
            ),
            DockGeometry {
                x: -280,
                y: 50,
                width: 60,
                height: 40,
            },
        );
    }

    #[test]
    fn dock_retry_is_bound_to_the_live_session_and_dom() {
        assert!(dock_retry_allowed(41, 41, true));
        assert!(!dock_retry_allowed(41, 42, true));
        assert!(!dock_retry_allowed(41, 41, false));
        assert!(!dock_retry_allowed(0, 0, true));
    }
}
