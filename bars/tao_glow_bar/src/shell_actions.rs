//! Semantic actions shared by the four-edge scene and its native host.
//!
//! Workspace and Dock actions stay on the managed runtime transport. Opening
//! applications and native system pages is asynchronous, so clicking a panel
//! never waits for a child application or a compositor IPC round trip.

use std::env;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc};
use std::thread;

use log::warn;
use xbar_core::{
    BarRuntime, BarSnapshot, DockItemGeometry, RuntimeUpdate, ShellRoute, TagId, UserAction,
    WindowToken,
};
use xbar_linux_actions::{CommandSpec, JwmIpc, ProcessActionHandler};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchTarget {
    Files,
    Terminal,
    Browser,
    Music,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliderKind {
    Volume,
    Brightness,
}

/// Values are captured when the scene is painted, not reconstructed on release.
/// In particular, a stale Dock click must retain its original session and
/// projection generation so the model can reject it after a WM restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellAction {
    User(UserAction),
    Launch(LaunchTarget),
    Slider(SliderKind),
    Restore {
        window: WindowToken,
        wm_session_id: u64,
        minimized_generation: u64,
    },
    CycleLayout,
    NextWorkspace,
    Network,
    Display,
}

/// Dispatch a completed click with a native, desktop-physical Dock target.
pub fn dispatch_with_geometry(
    action: ShellAction,
    geometry: DockItemGeometry,
    runtime: &mut BarRuntime,
) -> RuntimeUpdate {
    if let Some(action) = user_action(action, geometry, &runtime.snapshot()) {
        return runtime.dispatch(action);
    }

    match action {
        ShellAction::Launch(LaunchTarget::Music) => request_page("music_player"),
        ShellAction::Launch(target) => {
            if let Some(command) = launch_command(target) {
                if let Err(error) = ProcessActionHandler::default().launch(&command) {
                    warn!("four-edge launcher failed: {error}");
                }
            } else {
                // An unavailable default application still leaves a useful,
                // native way to choose an installed replacement.
                return runtime.dispatch(UserAction::OpenShellHub(ShellRoute::Applications));
            }
        }
        ShellAction::Network => request_page("wifi_picker"),
        ShellAction::Display => request_page("monitor_layout"),
        // A slider is dispatched by `slider`, with its measured percentage.
        ShellAction::Slider(_)
        | ShellAction::User(_)
        | ShellAction::Restore { .. }
        | ShellAction::CycleLayout
        | ShellAction::NextWorkspace => {}
    }
    RuntimeUpdate::default()
}

/// Resolve the runtime part of an action without performing desktop I/O.
pub fn user_action(
    action: ShellAction,
    geometry: DockItemGeometry,
    snapshot: &BarSnapshot,
) -> Option<UserAction> {
    match action {
        ShellAction::User(action) => Some(action),
        ShellAction::Restore {
            window,
            wm_session_id,
            minimized_generation,
        } => Some(UserAction::RestoreWindow {
            window,
            wm_session_id,
            minimized_generation,
            geometry,
        }),
        ShellAction::CycleLayout if snapshot.wm_available => {
            let count = snapshot
                .layout_count
                .unwrap_or(xbar_core::display::CANONICAL_LAYOUTS.len());
            let available: Vec<_> = xbar_core::display::CANONICAL_LAYOUTS
                .iter()
                .filter(|layout| (layout.id.0 as usize) < count)
                .collect();
            let next = available
                .iter()
                .position(|layout| {
                    snapshot.layout == Some(layout.id)
                        || (snapshot.layout.is_none()
                            && snapshot.layout_symbol.trim() == layout.symbol)
                })
                .map_or(0, |current| (current + 1) % available.len().max(1));
            available.get(next).map(|layout| UserAction::SetLayoutOn {
                layout: layout.id,
                monitor: snapshot.monitor,
            })
        }
        ShellAction::NextWorkspace if snapshot.wm_available && !snapshot.tags.is_empty() => {
            let current = snapshot
                .active_tag
                .map(TagId::index)
                .filter(|index| *index < snapshot.tags.len())
                .or_else(|| snapshot.tags.iter().position(|tag| tag.selected))
                .unwrap_or(0);
            TagId::new((current + 1) % snapshot.tags.len()).map(|tag| UserAction::ViewTagOn {
                tag,
                monitor: snapshot.monitor,
            })
        }
        _ => None,
    }
}

/// Change a real provider from its current reading to an absolute slider value.
/// Unavailable hardware is intentionally not assigned an invented midpoint.
pub fn slider(kind: SliderKind, percent: u8, runtime: &mut BarRuntime) -> RuntimeUpdate {
    match slider_action(kind, percent, &runtime.snapshot()) {
        Some(action) => runtime.dispatch(action),
        None => RuntimeUpdate::default(),
    }
}

pub fn slider_action(kind: SliderKind, percent: u8, snapshot: &BarSnapshot) -> Option<UserAction> {
    let current = match kind {
        SliderKind::Volume => snapshot.audio.volume_percent,
        SliderKind::Brightness => snapshot.brightness.percent,
    }?;
    let delta = i32::from(percent.min(100)) - i32::from(current.rounded());
    if delta == 0 {
        return None;
    }
    Some(match kind {
        SliderKind::Volume => UserAction::AdjustVolume(delta),
        SliderKind::Brightness => UserAction::AdjustBrightness(delta),
    })
}

/// A vertical slider fills upward: the top is 100%, the bottom is 0%.
/// Captured pointer motion outside the track clamps to those endpoints.
pub fn slider_percent_from_y(y: f32, top: f32, height: f32) -> Option<u8> {
    if !y.is_finite() || !top.is_finite() || !height.is_finite() || height <= 0.0 {
        return None;
    }
    Some(
        (((top + height - y) / height) * 100.0)
            .round()
            .clamp(0.0, 100.0) as u8,
    )
}

/// A single worker with a bounded queue avoids creating one IPC thread per
/// repeated click. The existing client bounds each request to 500 ms.
fn request_page(command: &'static str) {
    static QUEUE: OnceLock<Option<mpsc::SyncSender<&'static str>>> = OnceLock::new();
    let sender = QUEUE.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<&'static str>(8);
        match thread::Builder::new()
            .name("glow-shell-ipc".to_owned())
            .spawn(move || {
                let ipc = JwmIpc::new();
                for command in receiver {
                    if let Err(error) = ipc.command(command, Default::default()) {
                        warn!("four-edge system page failed: {error}");
                    }
                }
            }) {
            Ok(_) => Some(sender),
            Err(error) => {
                warn!("could not start four-edge system-page worker: {error}");
                None
            }
        }
    });
    if let Some(sender) = sender
        && let Err(error) = sender.try_send(command)
    {
        warn!("four-edge system-page queue unavailable: {error}");
    }
}

fn launch_command(target: LaunchTarget) -> Option<CommandSpec> {
    match target {
        LaunchTarget::Files => {
            let home = env::var_os("HOME")?;
            let opener = executable("xdg-open")?;
            Some(CommandSpec::new(opener.into_os_string()).with_arg(home))
        }
        LaunchTarget::Terminal => {
            let program = preferred_executable(
                "TERMINAL",
                &[
                    "x-terminal-emulator",
                    "alacritty",
                    "kitty",
                    "foot",
                    "gnome-terminal",
                    "konsole",
                    "xterm",
                ],
            )?;
            Some(CommandSpec::new(program.into_os_string()))
        }
        LaunchTarget::Browser => {
            let program = preferred_executable(
                "BROWSER",
                &[
                    "x-www-browser",
                    "firefox",
                    "chromium",
                    "chromium-browser",
                    "google-chrome",
                ],
            )?;
            Some(CommandSpec::new(program.into_os_string()).with_arg("about:blank"))
        }
        LaunchTarget::Music => None,
    }
}

fn preferred_executable(variable: &str, fallback: &[&str]) -> Option<PathBuf> {
    // Environment preferences name an executable, never shell source. A value
    // containing arguments cannot become command interpolation here.
    env::var_os(variable)
        .and_then(|program| executable(Path::new(&program)))
        .or_else(|| fallback.iter().find_map(executable))
}

fn executable(program: impl AsRef<Path>) -> Option<PathBuf> {
    let program = program.as_ref();
    let candidates = if program.components().count() > 1 {
        vec![program.to_owned()]
    } else {
        env::split_paths(&env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(program))
            .collect()
    };
    candidates.into_iter().find(|path| {
        path.metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use xbar_core::{BarEvent, BarModel, LayoutId, MonitorId, Percent, WmSnapshot};

    #[test]
    fn vertical_slider_clamps_captured_motion_and_rejects_invalid_tracks() {
        assert_eq!(slider_percent_from_y(20.0, 20.0, 90.0), Some(100));
        assert_eq!(slider_percent_from_y(65.0, 20.0, 90.0), Some(50));
        assert_eq!(slider_percent_from_y(110.0, 20.0, 90.0), Some(0));
        assert_eq!(slider_percent_from_y(-100.0, 20.0, 90.0), Some(100));
        assert_eq!(slider_percent_from_y(500.0, 20.0, 90.0), Some(0));
        assert_eq!(slider_percent_from_y(f32::NAN, 20.0, 90.0), None);
        assert_eq!(slider_percent_from_y(20.0, f32::INFINITY, 90.0), None);
        assert_eq!(slider_percent_from_y(20.0, 20.0, 0.0), None);
    }

    #[test]
    fn slider_uses_real_reading_and_does_not_invent_unavailable_hardware() {
        let mut snapshot = BarModel::default().snapshot();
        assert_eq!(slider_action(SliderKind::Brightness, 60, &snapshot), None);
        snapshot.brightness.percent = Percent::from_whole(40).ok();
        assert_eq!(
            slider_action(SliderKind::Brightness, 60, &snapshot),
            Some(UserAction::AdjustBrightness(20))
        );
        snapshot.audio.volume_percent = Percent::from_whole(80).ok();
        assert_eq!(
            slider_action(SliderKind::Volume, 10, &snapshot),
            Some(UserAction::AdjustVolume(-70))
        );
        assert_eq!(slider_action(SliderKind::Volume, 80, &snapshot), None);
        assert_eq!(
            slider_action(SliderKind::Volume, 255, &snapshot),
            Some(UserAction::AdjustVolume(20))
        );
    }

    #[test]
    fn navigation_uses_canonical_layout_order_and_owning_monitor() {
        let mut snapshot = BarModel::default().snapshot();
        snapshot.wm_available = true;
        snapshot.monitor = MonitorId(4);
        snapshot.layout = Some(LayoutId(0));
        assert_eq!(
            user_action(
                ShellAction::CycleLayout,
                DockItemGeometry::default(),
                &snapshot
            ),
            Some(UserAction::SetLayoutOn {
                layout: LayoutId(3),
                monitor: MonitorId(4)
            })
        );
        snapshot.layout_count = Some(2);
        assert_eq!(
            user_action(
                ShellAction::CycleLayout,
                DockItemGeometry::default(),
                &snapshot
            ),
            Some(UserAction::SetLayoutOn {
                layout: LayoutId(1),
                monitor: MonitorId(4)
            })
        );
        snapshot.active_tag = TagId::new(snapshot.tags.len() - 1);
        assert_eq!(
            user_action(
                ShellAction::NextWorkspace,
                DockItemGeometry::default(),
                &snapshot
            ),
            Some(UserAction::ViewTagOn {
                tag: TagId::new(0).unwrap(),
                monitor: MonitorId(4)
            })
        );
    }

    #[test]
    fn restore_keeps_the_painted_epoch_so_replacement_projection_rejects_it() {
        let mut model = BarModel::default();
        model
            .update(BarEvent::WindowManager(WmSnapshot {
                wm_session_id: 91,
                sequence: Some(74),
                ..WmSnapshot::default()
            }))
            .unwrap();
        let action = user_action(
            ShellAction::Restore {
                window: WindowToken(12),
                wm_session_id: 91,
                minimized_generation: 73,
            },
            DockItemGeometry::new(128, 712, 144, 52),
            &model.snapshot(),
        )
        .unwrap();
        assert!(matches!(
            action,
            UserAction::RestoreWindow {
                minimized_generation: 73,
                ..
            }
        ));
        assert!(model.update(BarEvent::User(action)).is_err());
    }
}
