//! Read-only startup health diagnostics.
//!
//! The production entry point captures process environment once and delegates
//! to pure checks. Tests construct the same snapshot directly, so they never
//! mutate process-global environment variables.

#![deny(clippy::all, clippy::pedantic)]

use crate::application::BackendChoice;
use crate::config::{Config, ConfigDiagnostics};
use serde::Serialize;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Severity of one doctor check or of the report as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DoctorStatus {
    Pass,
    Warning,
    Error,
}

impl DoctorStatus {
    const fn rank(self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::Warning => 1,
            Self::Error => 2,
        }
    }
}

/// A single independently actionable startup check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorCheck {
    pub status: DoctorStatus,
    pub id: String,
    pub summary: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_diagnostics: Option<ConfigDiagnostics>,
}

impl DoctorCheck {
    fn new(
        status: DoctorStatus,
        id: impl Into<String>,
        summary: impl Into<String>,
        detail: Option<String>,
        hint: Option<String>,
    ) -> Self {
        Self {
            status,
            id: id.into(),
            summary: summary.into(),
            detail,
            hint,
            config_diagnostics: None,
        }
    }

    fn with_config_diagnostics(mut self, diagnostics: ConfigDiagnostics) -> Self {
        self.config_diagnostics = Some(diagnostics);
        self
    }
}

/// Aggregate counts, convenient for both human and JSON frontends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DoctorSummary {
    pub passed: usize,
    pub warnings: usize,
    pub errors: usize,
}

/// Stable, serializable startup health report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    pub backend: String,
    pub status: DoctorStatus,
    pub summary: DoctorSummary,
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    fn from_checks(choice: BackendChoice, checks: Vec<DoctorCheck>) -> Self {
        let mut summary = DoctorSummary::default();
        let mut status = DoctorStatus::Pass;
        for check in &checks {
            match check.status {
                DoctorStatus::Pass => summary.passed += 1,
                DoctorStatus::Warning => summary.warnings += 1,
                DoctorStatus::Error => summary.errors += 1,
            }
            if check.status.rank() > status.rank() {
                status = check.status;
            }
        }
        Self {
            schema_version: 1,
            backend: choice.as_str().to_string(),
            status,
            summary,
            checks,
        }
    }
}

#[derive(Clone, Debug)]
struct DoctorInputs {
    config_path: PathBuf,
    xdg_runtime_dir: Option<PathBuf>,
    display: Option<OsString>,
    wayland_display: Option<OsString>,
    dbus_session_bus_address: Option<OsString>,
    path: Option<OsString>,
    dri_dir: PathBuf,
    effective_uid: u32,
}

impl DoctorInputs {
    fn capture(choice: BackendChoice) -> Self {
        Self {
            config_path: Config::get_config_path_for(choice.family()),
            xdg_runtime_dir: env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
            display: env::var_os("DISPLAY"),
            wayland_display: env::var_os("WAYLAND_DISPLAY"),
            dbus_session_bus_address: env::var_os("DBUS_SESSION_BUS_ADDRESS"),
            path: env::var_os("PATH"),
            dri_dir: PathBuf::from("/dev/dri"),
            effective_uid: current_effective_uid(),
        }
    }
}

fn current_effective_uid() -> u32 {
    // SAFETY: `geteuid` accepts no arguments and only reads process credentials.
    unsafe { libc::geteuid() }
}

/// Inspect whether the selected backend has the prerequisites needed to start.
///
/// This function only reads environment variables, file metadata and directory
/// entries. It does not construct a display backend, open DRM devices, write a
/// file, or mutate the process environment.
#[must_use]
pub fn diagnose(choice: BackendChoice) -> DoctorReport {
    diagnose_with_inputs(choice, &DoctorInputs::capture(choice))
}

fn diagnose_with_inputs(choice: BackendChoice, inputs: &DoctorInputs) -> DoctorReport {
    let mut checks = Vec::new();
    checks.extend(check_config(&inputs.config_path, choice));
    checks.push(check_status_bar(
        &inputs.config_path,
        inputs.path.as_deref(),
    ));
    checks.push(check_runtime_dir(
        inputs.xdg_runtime_dir.as_deref(),
        inputs.effective_uid,
        choice.family() == crate::config::BackendFamily::Wayland,
    ));
    checks.push(check_backend_requirements(choice, inputs));
    checks.push(check_dbus(inputs.dbus_session_bus_address.as_deref()));
    checks.push(check_jwm_tool(inputs.path.as_deref()));
    DoctorReport::from_checks(choice, checks)
}

fn check_config(path: &Path, choice: BackendChoice) -> Vec<DoctorCheck> {
    let displayed = path.display().to_string();
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return vec![DoctorCheck::new(
                DoctorStatus::Warning,
                "config.file",
                "The selected backend configuration does not exist yet",
                Some(displayed),
                Some("Run `jwm --gen-config`; first startup can also generate defaults".into()),
            )];
        }
        Err(error) => {
            return vec![DoctorCheck::new(
                DoctorStatus::Error,
                "config.file",
                "The selected backend configuration cannot be inspected",
                Some(format!("{displayed}: {error}")),
                Some("Check the path and its parent-directory permissions".into()),
            )];
        }
    };

    if !metadata.is_file() {
        return vec![DoctorCheck::new(
            DoctorStatus::Error,
            "config.file",
            "The selected configuration path is not a regular file",
            Some(displayed),
            Some("Move the directory or special file aside, then run `jwm --gen-config`".into()),
        )];
    }

    let file_check = DoctorCheck::new(
        DoctorStatus::Pass,
        "config.file",
        "The selected backend configuration exists",
        Some(displayed.clone()),
        None,
    );
    let validation = match Config::validate_config_file(path) {
        Ok(diagnostics) => {
            let status = if diagnostics.has_errors() {
                DoctorStatus::Error
            } else if diagnostics.warning_count() != 0 {
                DoctorStatus::Warning
            } else {
                DoctorStatus::Pass
            };
            let summary = match status {
                DoctorStatus::Pass => "Configuration syntax and semantics are valid".to_string(),
                DoctorStatus::Warning => format!(
                    "Configuration is usable with {} warning(s)",
                    diagnostics.warning_count()
                ),
                DoctorStatus::Error => format!(
                    "Configuration has {} semantic error(s)",
                    diagnostics.error_count()
                ),
            };
            let detail = (!diagnostics.is_empty()).then(|| diagnostics.to_string());
            let hint = (status != DoctorStatus::Pass).then(|| {
                format!(
                    "Fix the reported values, then run `jwm --backend {} --check-config`",
                    choice.as_str()
                )
            });
            DoctorCheck::new(status, "config.validation", summary, detail, hint)
                .with_config_diagnostics(diagnostics)
        }
        Err(error) => DoctorCheck::new(
            DoctorStatus::Error,
            "config.validation",
            "Configuration syntax or structure is invalid",
            Some(error.to_string()),
            Some(format!(
                "Fix the file, then run `jwm --backend {} --check-config`",
                choice.as_str()
            )),
        ),
    };
    vec![file_check, validation]
}

fn check_runtime_dir(path: Option<&Path>, effective_uid: u32, required: bool) -> DoctorCheck {
    let failure_status = if required {
        DoctorStatus::Error
    } else {
        DoctorStatus::Warning
    };
    let Some(path) = path.filter(|path| !path.as_os_str().is_empty()) else {
        return DoctorCheck::new(
            failure_status,
            "runtime.xdg_runtime_dir",
            "XDG_RUNTIME_DIR is not set",
            Some(if required {
                "Wayland socket creation requires a private runtime directory".into()
            } else {
                "JWM IPC will use its private /tmp fallback".into()
            }),
            Some("Use the login-session value, normally /run/user/$(id -u)".into()),
        );
    };

    if !path.is_absolute() {
        return DoctorCheck::new(
            failure_status,
            "runtime.xdg_runtime_dir",
            "XDG_RUNTIME_DIR must be an absolute path",
            Some(path.display().to_string()),
            Some("Set it to the absolute runtime directory created by the login session".into()),
        );
    }

    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return DoctorCheck::new(
                failure_status,
                "runtime.xdg_runtime_dir",
                "XDG_RUNTIME_DIR cannot be inspected",
                Some(format!("{}: {error}", path.display())),
                Some(
                    "Ensure the login session created the directory and that it is accessible"
                        .into(),
                ),
            );
        }
    };
    if !metadata.is_dir() {
        return DoctorCheck::new(
            failure_status,
            "runtime.xdg_runtime_dir",
            "XDG_RUNTIME_DIR is not a directory",
            Some(path.display().to_string()),
            Some("Point XDG_RUNTIME_DIR at the login session's private runtime directory".into()),
        );
    }

    let owner = metadata.uid();
    let mode = metadata.permissions().mode() & 0o777;
    let mut problems = Vec::new();
    if owner != effective_uid {
        problems.push(format!(
            "owned by uid {owner}, but the effective uid is {effective_uid}"
        ));
    }
    if mode & 0o077 != 0 {
        problems.push(format!(
            "mode {mode:04o} grants access to group or other users"
        ));
    }
    if mode & 0o700 != 0o700 {
        problems.push(format!("mode {mode:04o} does not grant owner rwx access"));
    }
    if !problems.is_empty() {
        return DoctorCheck::new(
            failure_status,
            "runtime.xdg_runtime_dir",
            "XDG_RUNTIME_DIR ownership or permissions are unsafe",
            Some(format!("{}: {}", path.display(), problems.join("; "))),
            Some("Use a directory owned by the current user with mode 0700".into()),
        );
    }

    DoctorCheck::new(
        DoctorStatus::Pass,
        "runtime.xdg_runtime_dir",
        "XDG_RUNTIME_DIR is private and usable",
        Some(format!("{} (uid {owner}, mode {mode:04o})", path.display())),
        None,
    )
}

fn display_value(name: &str, value: &OsStr) -> String {
    format!("{name}={}", value.to_string_lossy())
}

fn check_backend_requirements(choice: BackendChoice, inputs: &DoctorInputs) -> DoctorCheck {
    match choice {
        BackendChoice::X11rb | BackendChoice::Xcb | BackendChoice::WaylandX11 => {
            match inputs.display.as_deref().filter(|value| !value.is_empty()) {
                Some(display) => DoctorCheck::new(
                    DoctorStatus::Pass,
                    "backend.display",
                    "An X11 host display is configured",
                    Some(display_value("DISPLAY", display)),
                    None,
                ),
                None => DoctorCheck::new(
                    DoctorStatus::Error,
                    "backend.display",
                    "The selected backend requires DISPLAY",
                    Some(format!("backend={}", choice.as_str())),
                    Some("Run JWM from an X11 session or select a different backend".into()),
                ),
            }
        }
        BackendChoice::WaylandWinit => {
            let wayland = inputs
                .wayland_display
                .as_deref()
                .filter(|value| !value.is_empty());
            let x11 = inputs.display.as_deref().filter(|value| !value.is_empty());
            if wayland.is_none() && x11.is_none() {
                DoctorCheck::new(
                    DoctorStatus::Error,
                    "backend.host_display",
                    "The winit backend needs a Wayland or X11 host display",
                    Some("Neither WAYLAND_DISPLAY nor DISPLAY is set".into()),
                    Some("Run it inside an existing graphical session".into()),
                )
            } else {
                let detail = [
                    wayland.map(|value| display_value("WAYLAND_DISPLAY", value)),
                    x11.map(|value| display_value("DISPLAY", value)),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ");
                DoctorCheck::new(
                    DoctorStatus::Pass,
                    "backend.host_display",
                    "A graphical host display is configured for winit",
                    Some(detail),
                    None,
                )
            }
        }
        BackendChoice::WaylandUdev => check_dri(&inputs.dri_dir),
    }
}

fn check_dri(path: &Path) -> DoctorCheck {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return DoctorCheck::new(
                DoctorStatus::Error,
                "backend.dri",
                "The DRM/KMS backend cannot find /dev/dri",
                Some(format!("{}: {error}", path.display())),
                Some("Use a DRM-capable system/session or select a nested backend".into()),
            );
        }
    };
    if !metadata.is_dir() {
        return DoctorCheck::new(
            DoctorStatus::Error,
            "backend.dri",
            "The configured DRM path is not a directory",
            Some(path.display().to_string()),
            Some("Expected a device directory containing card0/card1-style nodes".into()),
        );
    }

    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            return DoctorCheck::new(
                DoctorStatus::Error,
                "backend.dri",
                "The DRM device directory cannot be read",
                Some(error.to_string()),
                Some("Check device permissions and the active seat/session".into()),
            );
        }
    };
    let mut cards = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            name.strip_prefix("card").is_some_and(|suffix| {
                !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit())
            })
        })
        .collect::<Vec<_>>();
    cards.sort();
    if cards.is_empty() {
        return DoctorCheck::new(
            DoctorStatus::Error,
            "backend.dri",
            "No DRM card device is present",
            Some(path.display().to_string()),
            Some("Check kernel DRM drivers, device permissions and seat assignment".into()),
        );
    }

    DoctorCheck::new(
        DoctorStatus::Pass,
        "backend.dri",
        "At least one DRM/KMS card device is present",
        Some(cards.join(", ")),
        None,
    )
}

fn check_dbus(address: Option<&OsStr>) -> DoctorCheck {
    let Some(address) = address.filter(|value| !value.is_empty()) else {
        return DoctorCheck::new(
            DoctorStatus::Warning,
            "session.dbus",
            "DBUS_SESSION_BUS_ADDRESS is not set",
            Some("JWM will try the systemd user bus and then dbus-launch".into()),
            Some("Start JWM from a normal login session if application activation fails".into()),
        );
    };
    let Some(address) = address.to_str() else {
        return DoctorCheck::new(
            DoctorStatus::Warning,
            "session.dbus",
            "DBUS_SESSION_BUS_ADDRESS is not valid UTF-8",
            None,
            Some("Use the address exported by the login session".into()),
        );
    };
    let scheme = address
        .split_once(':')
        .map_or("unknown", |(scheme, _)| scheme);
    DoctorCheck::new(
        DoctorStatus::Pass,
        "session.dbus",
        "A D-Bus session address is configured",
        Some(format!("transport={scheme}")),
        None,
    )
}

fn check_jwm_tool(path: Option<&OsStr>) -> DoctorCheck {
    let Some(path) = path.filter(|value| !value.is_empty()) else {
        return DoctorCheck::new(
            DoctorStatus::Warning,
            "command.jwm_tool",
            "PATH is empty, so jwm-tool cannot be discovered",
            None,
            Some("Install jwm-tool and add its directory to PATH".into()),
        );
    };
    match find_executable_in_path(path, "jwm-tool") {
        Some(executable) => DoctorCheck::new(
            DoctorStatus::Pass,
            "command.jwm_tool",
            "jwm-tool is available in PATH",
            Some(executable.display().to_string()),
            None,
        ),
        None => DoctorCheck::new(
            DoctorStatus::Warning,
            "command.jwm_tool",
            "jwm-tool is not executable from PATH",
            None,
            Some("Install the matching jwm-tool binary or add its directory to PATH".into()),
        ),
    }
}

fn check_status_bar(config_path: &Path, path: Option<&OsStr>) -> DoctorCheck {
    let config = if config_path.exists() {
        match Config::load_from_file(config_path) {
            Ok(config) => config,
            Err(error) => {
                return DoctorCheck::new(
                    DoctorStatus::Warning,
                    "command.status_bar",
                    "Status bar discovery was skipped because configuration is invalid",
                    Some(error.to_string()),
                    Some("Fix the configuration errors reported above".into()),
                );
            }
        }
    } else {
        Config::default()
    };

    if !config.show_bar() {
        return DoctorCheck::new(
            DoctorStatus::Pass,
            "command.status_bar",
            "The status bar is disabled by configuration",
            None,
            None,
        );
    }

    let command = config.status_bar_name().trim();
    if command.is_empty() {
        return DoctorCheck::new(
            DoctorStatus::Error,
            "command.status_bar",
            "The status bar is enabled but no executable is configured",
            None,
            Some("Set status_bar.name or disable status_bar.show_bar".into()),
        );
    }
    let executable = if command.contains('/') {
        find_executable_in_path(OsStr::new(""), command)
    } else {
        path.and_then(|path| find_executable_in_path(path, command))
    };
    match executable {
        Some(executable) => DoctorCheck::new(
            DoctorStatus::Pass,
            "command.status_bar",
            format!("Configured status bar {command:?} is executable"),
            Some(executable.display().to_string()),
            None,
        ),
        None => DoctorCheck::new(
            DoctorStatus::Warning,
            "command.status_bar",
            format!("Configured status bar {command:?} is not executable from PATH"),
            None,
            Some("Install the configured bar or update status_bar.name".into()),
        ),
    }
}

fn find_executable_in_path(path: &OsStr, command: &str) -> Option<PathBuf> {
    if command.contains('/') {
        let candidate = PathBuf::from(command);
        return fs::metadata(&candidate).ok().and_then(|metadata| {
            (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(candidate)
        });
    }
    env::split_paths(path).find_map(|directory| {
        let candidate = directory.join(command);
        fs::metadata(&candidate).ok().and_then(|metadata| {
            (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(candidate)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path =
                env::temp_dir().join(format!("jwm-doctor-test-{}-{sequence}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_inputs(root: &Path) -> DoctorInputs {
        DoctorInputs {
            config_path: root.join("config.toml"),
            xdg_runtime_dir: Some(root.join("runtime")),
            display: Some(OsString::from(":99")),
            wayland_display: None,
            dbus_session_bus_address: Some(OsString::from("unix:path=/tmp/test-bus")),
            path: None,
            dri_dir: root.join("dri"),
            effective_uid: current_effective_uid(),
        }
    }

    #[test]
    fn report_aggregates_status_and_serializes_stable_levels() {
        let report = DoctorReport::from_checks(
            BackendChoice::X11rb,
            vec![
                DoctorCheck::new(DoctorStatus::Pass, "a", "ok", None, None),
                DoctorCheck::new(DoctorStatus::Warning, "b", "warn", None, None),
                DoctorCheck::new(DoctorStatus::Error, "c", "bad", None, None),
            ],
        );
        assert_eq!(report.status, DoctorStatus::Error);
        assert_eq!(
            report.summary,
            DoctorSummary {
                passed: 1,
                warnings: 1,
                errors: 1,
            }
        );
        let json = serde_json::to_value(report).unwrap();
        assert_eq!(json["status"], "error");
        assert_eq!(json["checks"][0]["status"], "pass");
        assert!(json["checks"][0]["detail"].is_null());
        assert!(json["checks"][0]["hint"].is_null());
    }

    #[test]
    fn runtime_dir_check_covers_path_owner_and_mode_without_env_mutation() {
        let root = TestDir::new();
        let runtime = root.path().join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let uid = current_effective_uid();

        assert_eq!(
            check_runtime_dir(Some(&runtime), uid, true).status,
            DoctorStatus::Pass
        );
        assert_eq!(
            check_runtime_dir(Some(Path::new("relative/runtime")), uid, true).status,
            DoctorStatus::Error
        );
        assert_eq!(
            check_runtime_dir(Some(&runtime), uid.wrapping_add(1), true).status,
            DoctorStatus::Error
        );

        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            check_runtime_dir(Some(&runtime), uid, true).status,
            DoctorStatus::Error
        );
        assert_eq!(
            check_runtime_dir(None, uid, false).status,
            DoctorStatus::Warning
        );
        assert_eq!(
            check_runtime_dir(Some(Path::new("relative/runtime")), uid, false).status,
            DoctorStatus::Warning
        );
        assert_eq!(
            check_runtime_dir(Some(&runtime), uid, false).status,
            DoctorStatus::Warning
        );
        assert_eq!(
            check_runtime_dir(None, uid, true).status,
            DoctorStatus::Error
        );
    }

    #[test]
    fn backend_checks_distinguish_x11_winit_and_kms_requirements() {
        let root = TestDir::new();
        let mut inputs = test_inputs(root.path());

        inputs.display = None;
        assert_eq!(
            check_backend_requirements(BackendChoice::X11rb, &inputs).status,
            DoctorStatus::Error
        );
        inputs.display = Some(OsString::from(":1"));
        assert_eq!(
            check_backend_requirements(BackendChoice::WaylandX11, &inputs).status,
            DoctorStatus::Pass
        );
        inputs.display = None;
        inputs.wayland_display = Some(OsString::from("wayland-1"));
        assert_eq!(
            check_backend_requirements(BackendChoice::WaylandWinit, &inputs).status,
            DoctorStatus::Pass
        );

        fs::create_dir(&inputs.dri_dir).unwrap();
        assert_eq!(
            check_backend_requirements(BackendChoice::WaylandUdev, &inputs).status,
            DoctorStatus::Error
        );
        fs::write(inputs.dri_dir.join("card0"), []).unwrap();
        assert_eq!(
            check_backend_requirements(BackendChoice::WaylandUdev, &inputs).status,
            DoctorStatus::Pass
        );
    }

    #[test]
    fn path_check_requires_an_executable_jwm_tool() {
        let root = TestDir::new();
        let binary = root.path().join("jwm-tool");
        fs::write(&binary, b"test").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o600)).unwrap();
        let path = root.path().as_os_str();
        assert_eq!(check_jwm_tool(Some(path)).status, DoctorStatus::Warning);

        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let check = check_jwm_tool(Some(path));
        assert_eq!(check.status, DoctorStatus::Pass);
        let expected = binary.to_string_lossy().into_owned();
        assert_eq!(check.detail.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn config_check_preserves_structured_semantic_diagnostics() {
        let root = TestDir::new();
        let config_path = root.path().join("config.toml");
        Config::default().save_to_file(&config_path).unwrap();
        let valid = check_config(&config_path, BackendChoice::X11rb);
        assert_eq!(valid.len(), 2);
        assert_eq!(valid[1].status, DoctorStatus::Pass);

        let contents = fs::read_to_string(&config_path).unwrap();
        let invalid = contents.replacen("tags_length = 9", "tags_length = 32", 1);
        assert_ne!(contents, invalid);
        fs::write(&config_path, invalid).unwrap();
        let checks = check_config(&config_path, BackendChoice::X11rb);
        assert_eq!(checks[1].status, DoctorStatus::Error);
        assert!(
            checks[1]
                .config_diagnostics
                .as_ref()
                .is_some_and(ConfigDiagnostics::has_errors)
        );
    }

    #[test]
    fn full_diagnosis_uses_injected_snapshot_and_never_process_environment() {
        let root = TestDir::new();
        let mut inputs = test_inputs(root.path());
        Config::default().save_to_file(&inputs.config_path).unwrap();
        let runtime = inputs.xdg_runtime_dir.as_ref().unwrap();
        fs::create_dir(runtime).unwrap();
        fs::set_permissions(runtime, fs::Permissions::from_mode(0o700)).unwrap();

        let tool = root.path().join("jwm-tool");
        fs::write(&tool, b"test").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).unwrap();
        let bar = root.path().join("egui_bar");
        fs::write(&bar, b"test").unwrap();
        fs::set_permissions(&bar, fs::Permissions::from_mode(0o700)).unwrap();
        inputs.path = Some(root.path().as_os_str().to_owned());

        let report = diagnose_with_inputs(BackendChoice::X11rb, &inputs);
        assert_eq!(report.status, DoctorStatus::Pass);
        assert_eq!(report.summary.errors, 0);
        assert_eq!(report.summary.warnings, 0);
        assert_eq!(
            report
                .checks
                .iter()
                .map(|check| check.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "config.file",
                "config.validation",
                "command.status_bar",
                "runtime.xdg_runtime_dir",
                "backend.display",
                "session.dbus",
                "command.jwm_tool",
            ]
        );
    }
}
