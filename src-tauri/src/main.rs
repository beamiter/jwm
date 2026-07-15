// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use log::{debug, error, info, warn};
use serde::Serialize;
use std::{
    env,
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{Emitter, Manager, PhysicalPosition, PhysicalSize, WindowEvent};
use xbar_core::{
    BarEffect, BarRuntime, BarSnapshot, LayoutId, ModelConfig, MonitorGeometry, MonitorId,
    RuntimeAdapter, RuntimeIssue, RuntimeUpdate, SharedTransport, TagId, UserAction,
    logging::init as initialize_logging,
};

const TRANSPORT_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const BAR_HEIGHT: f64 = 40.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct TagStatusSnapshot {
    is_selected: bool,
    is_urg: bool,
    is_filled: bool,
    is_occ: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct MonitorInfoSnapshot {
    monitor_num: i32,
    monitor_width: i32,
    monitor_height: i32,
    monitor_x: i32,
    monitor_y: i32,
    tag_status_vec: Vec<TagStatusSnapshot>,
    client_name: String,
    ltsymbol: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct AudioSnapshot {
    volume: i32,
    is_muted: bool,
    device_name: String,
    has_device: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct BrightnessSnapshot {
    percent: Option<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
struct LoadAverageSnapshot {
    one_minute: f64,
    five_minutes: f64,
    fifteen_minutes: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct SystemUpdateSnapshot {
    cpu_usage: Vec<f32>,
    cpu_average: f32,
    memory_total: u64,
    memory_used: u64,
    memory_available: u64,
    memory_usage_percent: f32,
    uptime: u64,
    load_average: LoadAverageSnapshot,
    battery_percent: f32,
    is_charging: bool,
}

struct AppState {
    app_handle: tauri::AppHandle,
    runtime: Arc<Mutex<BarRuntime>>,
    window_baseline: WindowBaseline,
}

#[derive(Clone, Copy)]
struct WindowBaseline {
    initial_position: Option<PhysicalPosition<i32>>,
    logical_width: f64,
    logical_height: f64,
}

#[derive(Default)]
struct EmittedState {
    monitor_initialized: bool,
    monitor: Option<MonitorInfoSnapshot>,
    system: Option<SystemUpdateSnapshot>,
    audio: Option<AudioSnapshot>,
    brightness_percent: Option<u8>,
}

fn monitor_snapshot_from(snapshot: &BarSnapshot, scale_factor: f64) -> Option<MonitorInfoSnapshot> {
    if !snapshot.wm_available {
        return None;
    }
    let geometry = snapshot.geometry.unwrap_or_default();
    Some(MonitorInfoSnapshot {
        monitor_num: snapshot.monitor.0,
        monitor_width: i32::try_from(geometry.width).unwrap_or(i32::MAX),
        monitor_height: i32::try_from(geometry.height).unwrap_or(i32::MAX),
        monitor_x: geometry.x,
        monitor_y: geometry.y,
        tag_status_vec: snapshot
            .tags
            .iter()
            .map(|tag| TagStatusSnapshot {
                is_selected: tag.selected,
                is_urg: tag.urgent,
                is_filled: tag.filled,
                is_occ: tag.occupied,
            })
            .collect(),
        client_name: snapshot.client_name.clone(),
        ltsymbol: format!(
            "{} s: {:.2}, m: {}",
            snapshot.layout_symbol, scale_factor, snapshot.monitor.0
        ),
    })
}

fn audio_snapshot_from(snapshot: &BarSnapshot) -> AudioSnapshot {
    match &snapshot.audio_device {
        Some(device) => AudioSnapshot {
            volume: device.volume.clamp(0, 100),
            is_muted: device.is_muted,
            device_name: device.name.clone(),
            has_device: true,
        },
        None => AudioSnapshot {
            volume: 0,
            is_muted: true,
            device_name: String::new(),
            has_device: false,
        },
    }
}

fn brightness_snapshot_from(snapshot: &BarSnapshot) -> BrightnessSnapshot {
    BrightnessSnapshot {
        percent: snapshot.brightness.percent.map(|value| value.rounded()),
    }
}

fn system_snapshot_from(snapshot: &BarSnapshot) -> SystemUpdateSnapshot {
    SystemUpdateSnapshot {
        cpu_usage: snapshot.system_details.cpu_usage.clone(),
        cpu_average: snapshot.system_details.cpu_average,
        memory_total: snapshot.system_details.memory_total,
        memory_used: snapshot.system_details.memory_used,
        memory_available: snapshot.system_details.memory_available,
        memory_usage_percent: snapshot.system_details.memory_usage_percent,
        uptime: snapshot.system_details.uptime,
        load_average: LoadAverageSnapshot {
            one_minute: snapshot.system_details.load_average.one_minute,
            five_minutes: snapshot.system_details.load_average.five_minutes,
            fifteen_minutes: snapshot.system_details.load_average.fifteen_minutes,
        },
        battery_percent: snapshot
            .battery
            .percent
            .map_or(100.0, |value| value.as_f32()),
        is_charging: snapshot.battery.charging,
    }
}

fn issue_message(issue: &RuntimeIssue) -> String {
    match issue {
        RuntimeIssue::WindowManagerUnavailable { command } => {
            format!("window-manager state is unavailable; command rejected: {command:?}")
        }
        RuntimeIssue::QueueFull { command } => {
            format!("window-manager command queue is full: {command:?}")
        }
        RuntimeIssue::AdapterFailed {
            adapter,
            operation,
            message,
        } => format!("{adapter:?} {operation} failed: {message}"),
        RuntimeIssue::InvalidProviderPercent {
            adapter,
            field,
            error,
        } => format!("{adapter:?} returned invalid {field}: {error}"),
        RuntimeIssue::Model(error) => format!("model rejected action: {error}"),
    }
}

fn launch_and_reap(program: &'static str, args: &[&str]) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .spawn()
        .map_err(|error| format!("failed to launch {program}: {error}"))?;
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(error) = child.wait() {
            error!("failed to reap {program}: {error}");
        }
    });
    Ok(())
}

fn apply_monitor_geometry(
    window: &tauri::WebviewWindow,
    geometry: MonitorGeometry,
    scale_factor: f64,
) -> Result<(), String> {
    let height = (BAR_HEIGHT * scale_factor)
        .round()
        .clamp(1.0, f64::from(u32::MAX)) as u32;
    window
        .set_position(PhysicalPosition::new(geometry.x, geometry.y))
        .map_err(|error| format!("failed to position main window: {error}"))?;
    window
        .set_size(PhysicalSize::new(geometry.width.max(1), height))
        .map_err(|error| format!("failed to resize main window: {error}"))
}

fn apply_window_baseline(
    window: &tauri::WebviewWindow,
    baseline: WindowBaseline,
    scale_factor: f64,
) -> Result<(), String> {
    if let Some(position) = baseline.initial_position {
        window
            .set_position(position)
            .map_err(|error| format!("failed to restore main window position: {error}"))?;
    }
    let width = (baseline.logical_width * scale_factor)
        .round()
        .clamp(1.0, f64::from(u32::MAX)) as u32;
    let height = (baseline.logical_height * scale_factor)
        .round()
        .clamp(1.0, f64::from(u32::MAX)) as u32;
    window
        .set_size(PhysicalSize::new(width, height))
        .map_err(|error| format!("failed to restore main window size: {error}"))
}

fn execute_platform_effect(
    app_handle: &tauri::AppHandle,
    window_baseline: WindowBaseline,
    effect: BarEffect,
) -> Result<(), String> {
    match effect {
        BarEffect::Screenshot => launch_and_reap("flameshot", &["gui"]),
        BarEffect::OpenAudioControl => launch_and_reap("pavucontrol", &[]),
        BarEffect::ApplyMonitorGeometry(geometry) => {
            let window = app_handle
                .get_webview_window("main")
                .ok_or_else(|| "main window is not available".to_owned())?;
            let scale_factor = window
                .scale_factor()
                .map_err(|error| format!("failed to query window scale factor: {error}"))?;
            apply_monitor_geometry(&window, geometry, scale_factor)
        }
        BarEffect::ClearMonitorGeometry => {
            let window = app_handle
                .get_webview_window("main")
                .ok_or_else(|| "main window is not available".to_owned())?;
            let scale_factor = window
                .scale_factor()
                .map_err(|error| format!("failed to query window scale factor: {error}"))?;
            apply_window_baseline(&window, window_baseline, scale_factor)
        }
        BarEffect::WindowManager(command) => Err(format!(
            "no shared transport is available for window-manager command {command:?}"
        )),
        BarEffect::ToggleMute
        | BarEffect::AdjustVolume(_)
        | BarEffect::AdjustBrightness(_)
        | BarEffect::RefreshBattery => Err(format!(
            "enabled xbar_core provider did not consume platform effect {effect:?}"
        )),
    }
}

fn handle_runtime_update(
    app_handle: &tauri::AppHandle,
    window_baseline: WindowBaseline,
    update: RuntimeUpdate,
    fail_on_issue: bool,
) -> Result<(), String> {
    let mut failures = Vec::new();
    for issue in &update.issues {
        let message = issue_message(issue);
        warn!("{message}");
        if fail_on_issue {
            failures.push(message);
        }
    }
    for effect in update.platform_effects {
        if let Err(message) = execute_platform_effect(app_handle, window_baseline, effect) {
            error!("{message}");
            failures.push(message);
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn dispatch_action(state: &AppState, action: UserAction) -> Result<BarSnapshot, String> {
    let (snapshot, update) = {
        let mut runtime = state
            .runtime
            .lock()
            .map_err(|error| format!("bar runtime lock poisoned: {error}"))?;
        let update = runtime.dispatch(action);
        (runtime.snapshot(), update)
    };
    handle_runtime_update(&state.app_handle, state.window_baseline, update, true)?;
    Ok(snapshot)
}

#[tauri::command]
fn send_tag_command(
    tag_index: usize,
    is_view: bool,
    monitor_id: i32,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let tag = TagId::new(tag_index).ok_or_else(|| format!("invalid tag index: {tag_index}"))?;
    let action = if is_view {
        UserAction::ViewTagOn {
            tag,
            monitor: MonitorId(monitor_id),
        }
    } else {
        UserAction::ToggleTagOn {
            tag,
            monitor: MonitorId(monitor_id),
        }
    };
    dispatch_action(&state, action).map(|_| ())
}

#[tauri::command]
fn send_layout_command(
    layout_index: u32,
    monitor_id: i32,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    dispatch_action(
        &state,
        UserAction::SetLayoutOn {
            layout: LayoutId(layout_index),
            monitor: MonitorId(monitor_id),
        },
    )
    .map(|_| ())
}

#[tauri::command]
fn adjust_volume(delta: i32, state: tauri::State<'_, AppState>) -> Result<AudioSnapshot, String> {
    dispatch_action(&state, UserAction::AdjustVolume(delta))
        .map(|snapshot| audio_snapshot_from(&snapshot))
}

#[tauri::command]
fn toggle_mute(state: tauri::State<'_, AppState>) -> Result<AudioSnapshot, String> {
    dispatch_action(&state, UserAction::ToggleMute).map(|snapshot| audio_snapshot_from(&snapshot))
}

#[tauri::command]
fn adjust_brightness(
    delta: i32,
    state: tauri::State<'_, AppState>,
) -> Result<BrightnessSnapshot, String> {
    dispatch_action(&state, UserAction::AdjustBrightness(delta))
        .map(|snapshot| brightness_snapshot_from(&snapshot))
}

#[tauri::command]
fn take_screenshot(state: tauri::State<'_, AppState>) -> Result<(), String> {
    dispatch_action(&state, UserAction::Screenshot).map(|_| ())
}

#[tauri::command]
fn frontend_ready(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let runtime = state
        .runtime
        .lock()
        .map_err(|error| format!("bar runtime lock poisoned: {error}"))?;
    let snapshot = runtime.snapshot();
    let window = state
        .app_handle
        .get_webview_window("main")
        .ok_or_else(|| "main window is not available for frontend replay".to_owned())?;
    let scale_factor = window
        .scale_factor()
        .map_err(|error| format!("failed to query main window scale factor: {error}"))?;

    let mut failures = Vec::new();
    let monitor = monitor_snapshot_from(&snapshot, scale_factor);
    if let Err(error) = state.app_handle.emit("monitor-update", &monitor) {
        failures.push(format!("failed to replay monitor update: {error}"));
    }
    let system = system_snapshot_from(&snapshot);
    if let Err(error) = state.app_handle.emit("system-update", &system) {
        failures.push(format!("failed to replay system update: {error}"));
    }
    let audio = audio_snapshot_from(&snapshot);
    if let Err(error) = state.app_handle.emit("audio-update", &audio) {
        failures.push(format!("failed to replay audio update: {error}"));
    }
    let brightness = brightness_snapshot_from(&snapshot);
    if let Err(error) = state.app_handle.emit("brightness-update", &brightness) {
        failures.push(format!("failed to replay brightness update: {error}"));
    }
    drop(runtime);

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn emit_snapshot(
    app_handle: &tauri::AppHandle,
    snapshot: &BarSnapshot,
    emitted: &mut EmittedState,
) {
    if let Some(window) = app_handle.get_webview_window("main") {
        match window.scale_factor() {
            Ok(scale_factor) => {
                let payload = monitor_snapshot_from(snapshot, scale_factor);
                if !emitted.monitor_initialized || emitted.monitor != payload {
                    if let Err(error) = app_handle.emit("monitor-update", &payload) {
                        error!("failed to emit monitor update: {error}");
                    } else {
                        emitted.monitor = payload;
                        emitted.monitor_initialized = true;
                    }
                }
            }
            Err(error) => error!("failed to query main window scale factor: {error}"),
        }
    }

    let system = system_snapshot_from(snapshot);
    if emitted.system.as_ref() != Some(&system) {
        if let Err(error) = app_handle.emit("system-update", &system) {
            error!("failed to emit system update: {error}");
        } else {
            emitted.system = Some(system);
        }
    }

    let audio = audio_snapshot_from(snapshot);
    if emitted.audio.as_ref() != Some(&audio) {
        if let Err(error) = app_handle.emit("audio-update", &audio) {
            error!("failed to emit audio update: {error}");
        } else {
            emitted.audio = Some(audio);
        }
    }

    let brightness = brightness_snapshot_from(snapshot);
    if emitted.brightness_percent != brightness.percent {
        if let Err(error) = app_handle.emit("brightness-update", &brightness) {
            error!("failed to emit brightness update: {error}");
        } else {
            emitted.brightness_percent = brightness.percent;
        }
    }
}

fn try_open_transport(shared_path: &str) -> Option<SharedTransport> {
    if shared_path.is_empty() {
        return None;
    }
    match SharedTransport::open(shared_path) {
        Ok(transport) => {
            info!("opened existing WM shared transport at {shared_path}");
            Some(transport)
        }
        Err(error) => {
            warn!("WM shared transport is not available at {shared_path}: {error}");
            None
        }
    }
}

fn background_worker(
    app_handle: tauri::AppHandle,
    runtime: Arc<Mutex<BarRuntime>>,
    shared_path: String,
    window_baseline: WindowBaseline,
) {
    info!("starting xbar_core runtime worker");
    let mut emitted = EmittedState::default();
    let mut next_transport_retry = Instant::now();

    loop {
        let now = Instant::now();
        let (snapshot, update) = {
            let mut runtime = match runtime.lock() {
                Ok(runtime) => runtime,
                Err(error) => {
                    error!("bar runtime lock poisoned: {error}");
                    return;
                }
            };

            if runtime.transport().is_none()
                && !shared_path.is_empty()
                && now >= next_transport_retry
            {
                runtime.set_transport(try_open_transport(&shared_path));
                next_transport_retry = now + TRANSPORT_RETRY_INTERVAL;
            }

            let mut update = runtime.poll_transport();
            update.merge(runtime.tick());
            let transport_failed = update.issues.iter().any(|issue| {
                matches!(
                    issue,
                    RuntimeIssue::AdapterFailed {
                        adapter: RuntimeAdapter::Transport,
                        ..
                    }
                )
            });
            if transport_failed {
                runtime.set_transport(None);
                next_transport_retry = now + TRANSPORT_RETRY_INTERVAL;
            }
            (runtime.snapshot(), update)
        };

        if let Err(error) = handle_runtime_update(&app_handle, window_baseline, update, false) {
            error!("background runtime update failed: {error}");
        }
        emit_snapshot(&app_handle, &snapshot, &mut emitted);
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    unsafe {
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
    }

    let args: Vec<String> = env::args().collect();
    let shared_path = args.get(1).cloned().unwrap_or_default();
    if let Err(error) = initialize_logging("tauri_react_bar", &shared_path) {
        eprintln!("failed to initialize logging: {error}");
    }

    tauri::Builder::new()
        .plugin(tauri_plugin_opener::init())
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            let WindowEvent::ScaleFactorChanged { scale_factor, .. } = event else {
                return;
            };
            let Some(state) = window.try_state::<AppState>() else {
                debug!("bar runtime is not managed yet; skipping scale-factor update");
                return;
            };
            let (geometry, window_baseline) = {
                let runtime = match state.runtime.lock() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        error!("bar runtime lock poisoned during scale-factor update: {error}");
                        return;
                    }
                };
                (runtime.snapshot().geometry, state.window_baseline)
            };
            let Some(webview_window) = window.get_webview_window("main") else {
                error!("main webview window is not available for scale-factor update");
                return;
            };
            let result = geometry.map_or_else(
                || apply_window_baseline(&webview_window, window_baseline, *scale_factor),
                |geometry| apply_monitor_geometry(&webview_window, geometry, *scale_factor),
            );
            if let Err(error) = result {
                error!("failed to apply scale-factor geometry update: {error}");
            }
        })
        .setup(move |app| {
            let main_window = app.get_webview_window("main").ok_or_else(|| {
                std::io::Error::other("main window is not available during setup")
            })?;
            let window_baseline = WindowBaseline {
                initial_position: main_window.outer_position().ok(),
                logical_width: 800.0,
                logical_height: BAR_HEIGHT,
            };
            let scale_factor = main_window.scale_factor()?;
            apply_window_baseline(&main_window, window_baseline, scale_factor)
                .map_err(std::io::Error::other)?;
            let transport = try_open_transport(&shared_path);
            let runtime = Arc::new(Mutex::new(BarRuntime::with_transport(
                ModelConfig::default(),
                transport,
            )?));
            let app_handle = app.handle().clone();
            app.manage(AppState {
                app_handle: app_handle.clone(),
                runtime: Arc::clone(&runtime),
                window_baseline,
            });
            tauri::async_runtime::spawn_blocking(move || {
                background_worker(app_handle, runtime, shared_path, window_baseline);
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            send_tag_command,
            send_layout_command,
            take_screenshot,
            adjust_volume,
            toggle_mute,
            adjust_brightness,
            frontend_ready
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
