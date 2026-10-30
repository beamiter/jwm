//! One session process, one independent state connection, three panels/output.
mod action_queue;
use action_queue::LatestActions;
use anyhow::{Context, Result, bail};
use jwm_shell::{
    edge_shell::{Action, EdgeShell, ShellState},
    ipc,
    native_wayland::NativeShell,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::{fs::OpenOptionsExt, process::CommandExt},
    },
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use xbar_core::{
    BarEffect, BarRuntime, BarSnapshot, UserAction, WmCommand,
    config::BarConfig,
    desktop_shell::{SHELL_PROTOCOL_VERSION, ShellSnapshot},
};

enum OutputHost {
    X11(EdgeShell),
    Wayland(NativeShell),
}
impl OutputHost {
    fn update_managed(&mut self, state: ShellState) {
        match self {
            Self::X11(host) => host.update_managed(state),
            Self::Wayland(host) => host.update_managed(state),
        }
    }
    fn acknowledge(&mut self, action: UserAction) {
        match self {
            Self::X11(host) => host.acknowledge(action),
            Self::Wayland(host) => host.acknowledge(action),
        }
    }
    fn panels(&self) -> &[jwm_shell::edge_shell::PanelIdentity] {
        match self {
            Self::X11(host) => host.panels(),
            Self::Wayland(host) => host.panels(),
        }
    }
    fn is_alive(&self) -> bool {
        match self {
            Self::X11(host) => host.is_alive(),
            Self::Wayland(host) => host.is_alive(),
        }
    }
}

type ActionMessage = (i32, u64, Action);

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Release);
}

fn main() -> Result<()> {
    if std::env::args().any(|arg| arg == "--version") {
        println!(
            "jwm-shell {} protocol {}",
            env!("CARGO_PKG_VERSION"),
            SHELL_PROTOCOL_VERSION
        );
        return Ok(());
    }
    if std::env::args().any(|arg| arg == "--help") {
        println!(
            "jwm-shell — JWM supervised shared desktop sidebars\nUses the current session IPC and [desktop_shell] configuration.\n--version  Print executable and protocol versions\n--inspect-controls  Print current physical sidebar hit regions without creating surfaces\n--inspect-providers  Read current hardware availability without changing controls"
        );
        return Ok(());
    }
    if std::env::args().any(|arg| arg == "--inspect-providers") {
        let mut runtime = BarRuntime::new(BarConfig::load_default()?.model_config())?;
        let update = runtime.tick();
        let sampled = runtime.snapshot();
        let wm = full_snapshot().ok();
        println!(
            "{}",
            serde_json::to_string(&json!({
                "source":"fresh read-only sample using the host provider implementations",
                "audio":{"available":sampled.audio.volume_percent.is_some(),"percent":sampled.audio.volume_percent,"muted":sampled.audio.muted,"device":sampled.audio_device},
                "brightness":{"available":sampled.brightness.percent.is_some(),"percent":sampled.brightness.percent},
                "media_source":"JWM atomic MPRIS snapshot", "media":wm.as_ref().and_then(|snapshot| snapshot.outputs.first()).map(|output| &output.snapshot.media),
                "issues":update.issues.iter().map(ToString::to_string).collect::<Vec<_>>()
            }))?
        );
        return Ok(());
    }
    if std::env::args().any(|arg| arg == "--inspect-controls") {
        let snapshot = full_snapshot()?;
        let mut runtime = BarRuntime::new(BarConfig::load_default()?.model_config())?;
        runtime.tick();
        let sampled = runtime.snapshot();
        let mut rows = Vec::new();
        for output in &snapshot.outputs {
            let view = with_local_providers(output.snapshot.clone(), &sampled);
            let state = ShellState {
                output: output.geometry,
                snapshot: view,
                config: Some(snapshot.config.as_edge_shell()),
                visible: Some(output.visible),
                tasks: Some(
                    output
                        .tasks
                        .iter()
                        .map(|task| ipc::Task {
                            id: task.id,
                            incarnation: task.incarnation,
                            name: task.name.clone(),
                            class: task.class.clone(),
                            monitor: task.monitor,
                            is_focused: task.is_focused,
                            is_minimized: task.is_minimized,
                            is_dock: false,
                            is_desktop: false,
                            is_swallowed: false,
                        })
                        .collect(),
                ),
            };
            for edge in [
                xbar_core::BarEdge::Left,
                xbar_core::BarEdge::Right,
                xbar_core::BarEdge::Bottom,
            ] {
                rows.push(json!({"output":output.id,"output_identity":output.identity,"edge":edge,
                    "panel":xbar_core::edge_shell::panel_geometry(output.geometry,edge,true,snapshot.config.trigger_pixels),
                    "controls":jwm_shell::edge_shell::inspect_controls(&state,edge)?}));
            }
        }
        println!("{}", serde_json::to_string(&rows)?);
        return Ok(());
    }
    let supervisor_pid =
        std::env::var_os("JWM_DESKTOP_SHELL_SESSION").map(|_| unsafe { libc::getppid() });
    #[cfg(target_os = "linux")]
    if let Some(parent) = supervisor_pid {
        // A dead WM must never leave pointer strips behind. A live WM may be
        // temporarily busy in its renderer, so IPC timeout alone is not death.
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if unsafe { libc::getppid() } != parent {
            bail!("supervisor exited during startup");
        }
    }
    unsafe {
        libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
    }
    xbar_core::logging::init("jwm-shell", "")?;
    let mut config = BarConfig::load_default()?;
    config.presentation.edge = xbar_core::BarEdge::Top;
    config.edge_shell.enabled = true;
    let mut providers = BarRuntime::new(config.model_config())?;
    let mut snapshot = full_snapshot()?;
    let _singleton = singleton(snapshot.session)?;
    let expected_session = std::env::var("JWM_DESKTOP_SHELL_SESSION")
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    if expected_session.is_some_and(|session| session != snapshot.session) {
        bail!("supervisor session mismatch");
    }
    let (actions_tx, actions) = mpsc::sync_channel::<ActionMessage>(64);
    let latest_actions = Arc::new(Mutex::new(LatestActions::default()));
    let (invalidated_tx, invalidated) = mpsc::sync_channel(1);
    let subscription_stop = Arc::new(AtomicBool::new(false));
    let subscription = ipc::subscribe(invalidated_tx, subscription_stop.clone());
    let mut outputs: HashMap<i32, OutputHost> = HashMap::new();
    let mut identities: HashMap<i32, (u64, u64, Option<String>)> = HashMap::new();
    let native_wayland = native_wayland_requested();
    let mut last_snapshot = Instant::now() - Duration::from_secs(2);
    let mut last_tick = Instant::now() - Duration::from_secs(2);
    let mut last_heartbeat = Instant::now() - Duration::from_secs(2);
    let mut last_success = Instant::now();
    while !STOP.load(Ordering::Acquire) {
        if supervisor_pid.is_some_and(|parent| unsafe { libc::getppid() } != parent) {
            break;
        }
        let invalid = invalidated.try_recv().is_ok();
        if invalid || last_snapshot.elapsed() >= Duration::from_secs(1) {
            last_snapshot = Instant::now();
            match full_snapshot() {
                Ok(next) => {
                    if next.session != snapshot.session {
                        // Never allow old panel bindings into another WM lifetime.
                        outputs.clear();
                        if expected_session.is_some() {
                            bail!("supervisor session ended");
                        }
                    } else if next.revision < snapshot.revision {
                        continue;
                    }
                    snapshot = next;
                    latest_actions.lock().unwrap().retain_current(&snapshot);
                    last_success = Instant::now();
                }
                Err(error) => {
                    log::warn!("desktop shell resynchronization failed: {error:#}");
                    // Keep valid panels while the original supervisor lives.
                    // Parent-death monitoring handles real WM exits independently
                    // of a renderer pause that temporarily stalls its IPC loop.
                    if supervisor_pid.is_none() && last_success.elapsed() > Duration::from_secs(5) {
                        outputs.clear();
                    }
                }
            }
        }
        if !snapshot.config.enabled {
            break;
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            let update = providers.tick();
            report_issues(&update);
            last_tick = Instant::now();
            let sampled = providers.snapshot();
            let ids: HashSet<_> = snapshot.outputs.iter().map(|output| output.id).collect();
            outputs.retain(|id, _| ids.contains(id));
            identities.retain(|id, _| ids.contains(id));
            for output in &snapshot.outputs {
                let metadata = (
                    output.identity,
                    output.scale.to_bits(),
                    output.connector.clone(),
                );
                if identities
                    .get(&output.id)
                    .is_some_and(|identity| *identity != metadata)
                {
                    outputs.remove(&output.id);
                }
                identities.insert(output.id, metadata);
                let view = with_local_providers(output.snapshot.clone(), &sampled);
                let tasks = output
                    .tasks
                    .iter()
                    .map(|task| ipc::Task {
                        id: task.id,
                        incarnation: task.incarnation,
                        name: task.name.clone(),
                        class: task.class.clone(),
                        monitor: task.monitor,
                        is_focused: task.is_focused,
                        is_minimized: task.is_minimized,
                        is_dock: false,
                        is_desktop: false,
                        is_swallowed: false,
                    })
                    .collect();
                let state = ShellState {
                    output: output.geometry,
                    snapshot: view,
                    config: Some(snapshot.config.as_edge_shell()),
                    visible: Some(output.visible),
                    tasks: Some(tasks),
                };
                if let Some(host) = outputs.get_mut(&output.id) {
                    if !host.is_alive() {
                        bail!("output {} worker stopped", output.id);
                    }
                    host.update_managed(state);
                } else {
                    let sender = actions_tx.clone();
                    let replaceable = latest_actions.clone();
                    let id = output.id;
                    let identity = output.identity;
                    let handler = Arc::new(move |action| {
                        let message = (id, identity, action);
                        if LatestActions::replaceable(&message) {
                            if !replaceable.lock().unwrap().push(message) {
                                log::warn!("desktop shell replaceable action limit reached");
                            }
                        } else if sender.try_send(message).is_err() {
                            log::warn!("desktop shell action queue full; action not accepted");
                        }
                    });
                    let mut native_config = config.clone();
                    native_config.edge_shell = snapshot.config.as_edge_shell();
                    let host = if native_wayland {
                        NativeShell::start_managed(
                            native_config,
                            state,
                            output.scale,
                            output.connector.clone(),
                            handler,
                        )?
                        .map(OutputHost::Wayland)
                    } else {
                        EdgeShell::start_managed(native_config, state, handler)?
                            .map(OutputHost::X11)
                    };
                    if let Some(host) = host {
                        outputs.insert(id, host);
                    }
                    last_heartbeat = Instant::now() - Duration::from_secs(2);
                }
            }
        }
        // Visibility and hot mode edits must not wait for the provider clock.
        if invalid {
            last_tick = Instant::now() - Duration::from_secs(2);
        }
        if last_heartbeat.elapsed() >= Duration::from_secs(1) {
            match register(snapshot.session, &outputs, &identities) {
                Ok(()) => last_heartbeat = Instant::now(),
                Err(error)
                    if supervisor_pid.is_some()
                        || last_success.elapsed() <= Duration::from_secs(5) =>
                {
                    log::warn!("shell registration failed: {error:#}")
                }
                Err(error) => return Err(error),
            }
        }
        let queued = actions
            .try_recv()
            .ok()
            .or_else(|| latest_actions.lock().unwrap().pop());
        let next_action = queued
            .map(Ok)
            .unwrap_or_else(|| actions.recv_timeout(Duration::from_millis(25)));
        match next_action {
            Ok((output, output_identity, action)) => {
                let changes_desktop = !matches!(&action, Action::Bound {action,..} if matches!(action.as_ref(), Action::Model(UserAction::PreviewWindow {..} | UserAction::SetDockGeometry {..})));
                let preview = match &action {
                    Action::Bound { action, .. } => match action.as_ref() {
                        Action::Model(action @ UserAction::PreviewWindow { .. }) => Some(*action),
                        _ => None,
                    },
                    _ => None,
                };
                let retry = matches!(&action, Action::Bound {action,..} if matches!(action.as_ref(), Action::Model(UserAction::SetDockGeometry {..})))
                    .then(|| (output, output_identity, action.clone()));
                match dispatch(output, output_identity, action, &snapshot, &mut providers) {
                    Ok(()) => {
                        if let Some(action) = preview
                            && let Some(host) = outputs.get_mut(&output)
                        {
                            host.acknowledge(action);
                        }
                    }
                    Err(error) => {
                        if error.downcast_ref::<std::io::Error>().is_some()
                            && let Some(message) = retry
                        {
                            latest_actions.lock().unwrap().retry(message);
                        }
                        log::warn!("shell action rejected: {error:#}");
                    }
                }
                if changes_desktop {
                    last_tick = Instant::now() - Duration::from_secs(2);
                    last_snapshot = Instant::now() - Duration::from_secs(2);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    outputs.clear();
    subscription_stop.store(true, Ordering::Release);
    let _ = subscription.join();
    Ok(())
}

fn full_snapshot() -> Result<ShellSnapshot> {
    let snapshot: ShellSnapshot =
        serde_json::from_value(ipc::request("get_desktop_shell", Value::Null)?)?;
    snapshot.validate().map_err(anyhow::Error::msg)?;
    Ok(snapshot)
}

/// Hardware samples are local; desktop appearance and window/media state belong
/// to the supervisor. In particular, XBAR_CONFIG's standalone theme must not
/// overwrite a live theme selection made in the desktop's Theme menu.
fn with_local_providers(mut view: BarSnapshot, sampled: &BarSnapshot) -> BarSnapshot {
    view.audio = sampled.audio;
    view.audio_device = sampled.audio_device.clone();
    view.brightness = sampled.brightness;
    view.battery = sampled.battery;
    view.network = sampled.network.clone();
    view.system = sampled.system;
    view.system_details = sampled.system_details.clone();
    view.time = sampled.time.clone();
    view
}
fn register(
    session: u64,
    outputs: &HashMap<i32, OutputHost>,
    identities: &HashMap<i32, (u64, u64, Option<String>)>,
) -> Result<()> {
    let panels: Vec<_> = outputs.values().flat_map(OutputHost::panels).map(|panel| json!({
        "output":panel.output,"output_identity":identities.get(&panel.output).map(|identity| identity.0),
        "role":panel.role,"window":panel.window,
    })).collect();
    ipc::request(
        "register_desktop_shell",
        json!({"protocol_version":SHELL_PROTOCOL_VERSION,"session":session,"pid":std::process::id(),"panels":panels}),
    )?;
    Ok(())
}
fn singleton(session: u64) -> Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(ipc::runtime_directory().join(format!("jwm-shell-{session}.lock")))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another desktop shell already owns this session");
    }
    Ok(file)
}
fn report_issues(update: &xbar_core::RuntimeUpdate) {
    for issue in &update.issues {
        log::debug!("shell provider: {issue}");
    }
}

fn wm_command(action: UserAction, snapshot: &BarSnapshot) -> Option<WmCommand> {
    let monitor = snapshot.monitor;
    Some(match action {
        UserAction::ViewTag(tag) => WmCommand::ViewTag { tag, monitor },
        UserAction::ViewTagOn { tag, monitor } => WmCommand::ViewTag { tag, monitor },
        UserAction::ToggleTag(tag) => WmCommand::ToggleTag { tag, monitor },
        UserAction::ToggleTagOn { tag, monitor } => WmCommand::ToggleTag { tag, monitor },
        UserAction::SetLayout(layout) => WmCommand::SetLayout { layout, monitor },
        UserAction::SetLayoutOn { layout, monitor } => WmCommand::SetLayout { layout, monitor },
        UserAction::OpenShellHub(route) => WmCommand::OpenShellHub { route, monitor },
        UserAction::RestoreWindow {
            window,
            wm_session_id,
            minimized_generation,
            geometry,
        } => WmCommand::RestoreWindow {
            window,
            wm_session_id,
            minimized_generation,
            geometry,
            monitor,
        },
        UserAction::PreviewWindow {
            window,
            wm_session_id,
            minimized_generation,
            visible,
            renewal,
            geometry,
        } => WmCommand::PreviewWindow {
            window,
            wm_session_id,
            minimized_generation,
            visible,
            renewal,
            geometry,
            monitor,
        },
        UserAction::SetDockGeometry {
            window,
            wm_session_id,
            minimized_generation,
            geometry,
        } => WmCommand::SetDockGeometry {
            window,
            wm_session_id,
            minimized_generation,
            geometry,
            monitor,
        },
        _ => return None,
    })
}
fn dispatch(
    output: i32,
    output_identity: u64,
    action: Action,
    snapshot: &ShellSnapshot,
    providers: &mut BarRuntime,
) -> Result<()> {
    let Action::Bound {
        session,
        generation,
        action,
    } = action
    else {
        bail!("unbound shared shell action");
    };
    if session != snapshot.session {
        bail!("stale shell session");
    }
    let current_output = snapshot
        .outputs
        .iter()
        .find(|item| item.id == output)
        .context("output disconnected")?;
    if current_output.identity != output_identity {
        bail!("stale output identity");
    }
    let view = &current_output.snapshot;
    let projection_bound = matches!(
        action.as_ref(),
        Action::Model(
            UserAction::RestoreWindow { .. }
                | UserAction::PreviewWindow { .. }
                | UserAction::SetDockGeometry { .. }
        )
    );
    if projection_bound && generation != view.wm_sequence.unwrap_or_default() {
        bail!("stale shell window generation");
    }
    let invoke = |name: &str, args: Value| -> Result<()> {
        ipc::request(
            "desktop_shell_action",
            json!({"session":session,"output":output,"output_identity":output_identity,"name":name,"args":args}),
        )?;
        Ok(())
    };
    let launch_or_picker = |program: &str, args: &[std::ffi::OsString]| -> Result<()> {
        if let Err(error) = launch(program, args, invoke) {
            log::warn!("application launcher unavailable: {error:#}");
            let command = WmCommand::OpenShellHub {
                route: xbar_core::ShellRoute::Applications,
                monitor: view.monitor,
            };
            ipc::request(
                "desktop_shell_action",
                json!({"session":session,"output":output,
                "output_identity":output_identity,"command":command}),
            )?;
        }
        Ok(())
    };
    let action = match *action {
        Action::FocusWindow { id, incarnation } => {
            return invoke("focus_window", json!({"id":id,"incarnation":incarnation}));
        }
        Action::MediaPrevious => return invoke("media_previous", Value::Null),
        Action::MediaNext => return invoke("media_next", Value::Null),
        Action::Model(UserAction::MediaPlayPause) => {
            return invoke("media_play_pause", Value::Null);
        }
        Action::Model(action) => action,
        Action::Level { audio, percent } => {
            let current = if audio {
                providers.snapshot().audio.volume_percent
            } else {
                providers.snapshot().brightness.percent
            };
            let Some(current) = current else {
                return Ok(());
            };
            let delta = percent.clamp(0, 100) - i32::from(current.rounded());
            if audio {
                UserAction::AdjustVolume(delta)
            } else {
                UserAction::AdjustBrightness(delta)
            }
        }
        Action::Files => {
            if let Some(home) = std::env::var_os("HOME") {
                launch_or_picker("xdg-open", &[home])?;
            }
            return Ok(());
        }
        Action::Terminal => {
            launch_or_picker("x-terminal-emulator", &[])?;
            return Ok(());
        }
        Action::Browser => {
            launch_or_picker("x-www-browser", &[])?;
            return Ok(());
        }
        Action::CycleTasks | Action::HidePanel => return Ok(()),
        Action::Bound { .. } => bail!("nested action binding"),
    };
    if let Some(command) = wm_command(action, view) {
        ipc::request(
            "desktop_shell_action",
            json!({"session":session,"output":output,"output_identity":output_identity,"command":command}),
        )?;
    } else {
        let update = providers.dispatch(action);
        report_issues(&update);
        for effect in update.platform_effects {
            match effect {
                BarEffect::OpenAudioControl => launch("pavucontrol", &[], invoke)?,
                BarEffect::Screenshot => {
                    xbar_linux_actions::JwmIpc::new().take_screenshot()?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}
fn native_wayland_requested() -> bool {
    std::env::var("JWM_DESKTOP_SHELL_BACKEND").map_or_else(
        |_| std::env::var_os("WAYLAND_DISPLAY").is_some(),
        |backend| backend.starts_with("wayland"),
    )
}

fn launch(
    program: &str,
    args: &[std::ffi::OsString],
    invoke: impl Fn(&str, Value) -> Result<()>,
) -> Result<()> {
    if native_wayland_requested() {
        // The shell can start before XWayland is ready, so its inherited
        // DISPLAY is not authoritative. JWM supplies its current child
        // environment and reaps this independent application process.
        let mut command = vec![program.to_owned()];
        for arg in args {
            command.push(
                arg.to_str()
                    .context("application argument is not UTF-8")?
                    .to_owned(),
            );
        }
        invoke("spawn", json!({"cmd":command}))?;
        return Ok(());
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A detached grandchild is adopted by the session's child reaper. The
    // short intermediate child is reaped synchronously: opening long-lived
    // applications cannot accumulate one waiter thread per application.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            match libc::fork() {
                -1 => Err(std::io::Error::last_os_error()),
                0 => Ok(()),
                _ => libc::_exit(0),
            }
        });
    }
    let mut child = command.spawn()?;
    let status = child.wait()?;
    if !status.success() {
        bail!("application detachment failed: {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_hardware_samples_preserve_live_desktop_theme() {
        for theme in [xbar_core::ThemeMode::Light, xbar_core::ThemeMode::Dark] {
            let mut view = xbar_core::BarModel::default().snapshot();
            view.theme = theme;
            view.client_name = "managed window".into();
            let mut sampled = xbar_core::BarModel::default().snapshot();
            sampled.theme = match theme {
                xbar_core::ThemeMode::Light => xbar_core::ThemeMode::Dark,
                xbar_core::ThemeMode::Dark => xbar_core::ThemeMode::Light,
            };
            sampled.audio.volume_percent = Some(xbar_core::Percent::from_whole(37).unwrap());
            let merged = with_local_providers(view, &sampled);
            assert_eq!(merged.theme, theme);
            assert_eq!(merged.client_name, "managed window");
            assert_eq!(merged.audio.volume_percent, sampled.audio.volume_percent);
        }
    }

    #[test]
    fn dock_commands_preserve_captured_session_generation_and_actual_anchor() {
        let view = BarRuntime::default().snapshot();
        let window = xbar_core::WindowToken(37);
        let geometry = xbar_core::DockItemGeometry::new(-1200, 700, 160, 52);
        let action = UserAction::RestoreWindow {
            window,
            wm_session_id: 19,
            minimized_generation: 27,
            geometry,
        };
        assert!(
            matches!(wm_command(action, &view), Some(WmCommand::RestoreWindow {wm_session_id:19,minimized_generation:27,geometry:anchor,..}) if anchor == geometry)
        );
    }
}
