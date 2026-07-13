//! Application composition root.
//!
//! This module owns backend construction and the top-level JWM lifecycle. The
//! binary is intentionally kept as a thin process bootstrap layer so startup
//! options can be parsed and tested without starting an X11/Wayland server.

use crate::Jwm;
use crate::backend::api::{Backend, CompositorBenchmark};
use crate::backend::wayland_udev::backend::UdevBackend;
use crate::backend::wayland_winit::backend::WaylandWinitBackend;
use crate::backend::wayland_x11::backend::WaylandX11Backend;
use crate::backend::x11rb::backend::X11rbBackend;
use crate::backend::xcb::backend::XcbBackend;
use crate::config::{BackendFamily, Config, ConfigError, set_backend_family};
use log::{error, info};
use std::env;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;
use std::sync::atomic::Ordering;

pub const BACKEND_ENV: &str = "JWM_BACKEND";
pub const BENCHMARK_ENV: &str = "JWM_BENCHMARK";
pub const BENCHMARK_WARMUP_ENV: &str = "JWM_BENCHMARK_WARMUP";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackendChoice {
    #[default]
    X11rb,
    Xcb,
    WaylandUdev,
    WaylandX11,
    WaylandWinit,
}

impl BackendChoice {
    #[must_use]
    pub const fn family(self) -> BackendFamily {
        match self {
            Self::X11rb | Self::Xcb => BackendFamily::X11,
            Self::WaylandUdev | Self::WaylandX11 | Self::WaylandWinit => BackendFamily::Wayland,
        }
    }

    #[must_use]
    pub const fn config_name(self) -> &'static str {
        match self.family() {
            BackendFamily::X11 => "config_x11.toml",
            BackendFamily::Wayland => "config_wayland.toml",
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X11rb => "x11rb",
            Self::Xcb => "xcb",
            Self::WaylandUdev => "wayland-udev",
            Self::WaylandX11 => "wayland-x11",
            Self::WaylandWinit => "wayland-winit",
        }
    }
}

impl FromStr for BackendChoice {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "x11rb" => Ok(Self::X11rb),
            "xcb" | "x11-xcb" => Ok(Self::Xcb),
            "wayland-udev" | "udev" | "wayland" => Ok(Self::WaylandUdev),
            "wayland-x11" | "x11-wayland" | "windowed" => Ok(Self::WaylandX11),
            "wayland-winit" | "winit" => Ok(Self::WaylandWinit),
            other => Err(format!(
                "unknown backend {other:?}; expected one of: x11rb, xcb, wayland-udev, wayland-x11, wayland-winit"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BenchmarkRequest {
    pub frames: u32,
    pub warmup: u32,
}

impl BenchmarkRequest {
    pub fn new(frames: u32, warmup: u32) -> Result<Self, String> {
        if frames == 0 {
            return Err("benchmark frame count must be greater than zero".to_string());
        }
        Ok(Self { frames, warmup })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApplicationOptions {
    pub backend: BackendChoice,
    pub benchmark: Option<BenchmarkRequest>,
}

impl ApplicationOptions {
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {
            backend: configured_backend()?,
            benchmark: configured_benchmark()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedConfig {
    pub path: PathBuf,
    pub backup: Option<PathBuf>,
}

/// Resolve the configured backend without constructing it.
///
/// Environment access is kept at this boundary for compatibility. New callers
/// should prefer constructing `ApplicationOptions` explicitly.
pub fn configured_backend() -> Result<BackendChoice, String> {
    env::var(BACKEND_ENV).map_or_else(
        |error| match error {
            env::VarError::NotPresent => Ok(BackendChoice::default()),
            env::VarError::NotUnicode(_) => Err(format!("{BACKEND_ENV} is not valid UTF-8")),
        },
        |value| value.parse(),
    )
}

#[must_use]
pub fn config_path(choice: BackendChoice) -> PathBuf {
    Config::get_config_path_for(choice.family())
}

pub fn generate_config_templates() -> Result<Vec<GeneratedConfig>, ConfigError> {
    let mut generated = Vec::with_capacity(2);
    for family in [BackendFamily::X11, BackendFamily::Wayland] {
        let path = Config::get_config_path_for(family);
        let backup = if path.exists() {
            Some(Config::backup_config(&path)?)
        } else {
            None
        };
        Config::generate_template(&path)?;
        generated.push(GeneratedConfig { path, backup });
    }
    Ok(generated)
}

pub fn validate_config(choice: BackendChoice) -> Result<PathBuf, ConfigError> {
    let path = config_path(choice);
    Config::validate_config_file(&path)?;
    Ok(path)
}

fn create_backend(choice: BackendChoice) -> Result<Box<dyn Backend>, Box<dyn std::error::Error>> {
    // Config is a process-wide singleton, so its family must be established
    // before any backend constructor can access CONFIG.
    set_backend_family(choice.family());
    info!(
        "Initializing {} backend (config: {})",
        choice.as_str(),
        choice.config_name()
    );

    match choice {
        BackendChoice::X11rb => Ok(Box::new(X11rbBackend::new()?)),
        BackendChoice::Xcb => Ok(Box::new(XcbBackend::new()?)),
        BackendChoice::WaylandUdev => Ok(Box::new(UdevBackend::new()?)),
        BackendChoice::WaylandX11 => Ok(Box::new(WaylandX11Backend::new()?)),
        BackendChoice::WaylandWinit => Ok(Box::new(WaylandWinitBackend::new()?)),
    }
}

#[derive(Debug)]
struct RestartCommand {
    executable: OsString,
    arguments: Vec<OsString>,
}

impl RestartCommand {
    fn current() -> Self {
        let mut arguments = env::args_os();
        let invoked_as = arguments.next().unwrap_or_else(|| OsString::from("jwm"));
        let executable = env::current_exe().map_or(invoked_as, |path| path.into_os_string());
        Self {
            executable,
            arguments: arguments.collect(),
        }
    }

    fn exec(&self) -> std::io::Error {
        Command::new(&self.executable)
            .args(&self.arguments)
            .env("JWM_RESTARTING", "1")
            .exec()
    }
}

/// Run JWM using environment-based compatibility options.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    run_with_options(ApplicationOptions::from_env()?)
}

/// Run JWM until it exits or replaces itself during a restart.
pub fn run_with_options(options: ApplicationOptions) -> Result<(), Box<dyn std::error::Error>> {
    let restart_command = RestartCommand::current();

    loop {
        info!(
            "[application] starting JWM instance with backend {}",
            options.backend.as_str()
        );
        let mut backend = create_backend(options.backend)?;

        backend.check_existing_wm()?;

        let mut jwm = Jwm::new(&mut *backend)?;
        jwm.setup(&mut *backend)?;
        jwm.setup_initial_windows(&mut *backend)?;
        configure_benchmark(&mut *backend, options.benchmark);
        jwm.run(&mut *backend)?;
        jwm.cleanup(&mut *backend)?;

        if jwm.is_restarting.load(Ordering::SeqCst) {
            info!("[application] restarting via exec");
            drop(jwm);
            drop(backend);

            let error = restart_command.exec();
            error!("[application] exec failed: {error}; falling back to in-process restart");
            continue;
        }

        if let Err(error) = Command::new("jwm-tool").arg("quit").spawn() {
            error!("[application] failed to quit jwm daemon: {error}");
        }
        return Ok(());
    }
}

fn parse_benchmark(
    frames: Option<&str>,
    warmup: Option<&str>,
) -> Result<Option<BenchmarkRequest>, String> {
    let Some(frames) = frames else {
        if warmup.is_some() {
            return Err(format!("{BENCHMARK_WARMUP_ENV} requires {BENCHMARK_ENV}"));
        }
        return Ok(None);
    };

    let frames = frames
        .parse::<u32>()
        .map_err(|error| format!("invalid {BENCHMARK_ENV} value {frames:?}: {error}"))?;
    let warmup = warmup.map_or(Ok(60), |value| {
        value
            .parse::<u32>()
            .map_err(|error| format!("invalid {BENCHMARK_WARMUP_ENV} value {value:?}: {error}"))
    })?;

    BenchmarkRequest::new(frames, warmup).map(Some)
}

fn configured_benchmark() -> Result<Option<BenchmarkRequest>, String> {
    let frames = env::var(BENCHMARK_ENV).ok();
    let warmup = env::var(BENCHMARK_WARMUP_ENV).ok();
    parse_benchmark(frames.as_deref(), warmup.as_deref())
}

fn configure_benchmark<B: CompositorBenchmark + ?Sized>(
    backend: &mut B,
    request: Option<BenchmarkRequest>,
) {
    let Some(request) = request else {
        return;
    };
    backend.compositor_benchmark_start(request.frames, request.warmup);
    backend.compositor_benchmark_set_auto_exit(true);
    info!(
        "Benchmark mode: collecting {} frames (warmup={})",
        request.frames, request.warmup
    );
}

#[cfg(test)]
mod tests {
    use super::{
        ApplicationOptions, BackendChoice, BenchmarkRequest, config_path, configure_benchmark,
        parse_benchmark,
    };
    use crate::backend::api::CompositorBenchmark;
    use crate::config::BackendFamily;

    #[derive(Default)]
    struct BenchmarkSpy {
        started: Option<(u32, u32)>,
        auto_exit: bool,
    }

    impl CompositorBenchmark for BenchmarkSpy {
        fn compositor_benchmark_start(&mut self, frames: u32, warmup: u32) -> bool {
            self.started = Some((frames, warmup));
            true
        }

        fn compositor_benchmark_set_auto_exit(&mut self, enabled: bool) {
            self.auto_exit = enabled;
        }
    }

    #[test]
    fn backend_aliases_are_parsed() {
        assert_eq!("x11rb".parse(), Ok(BackendChoice::X11rb));
        assert_eq!("X11-XCB".parse(), Ok(BackendChoice::Xcb));
        assert_eq!("wayland".parse(), Ok(BackendChoice::WaylandUdev));
        assert_eq!("windowed".parse(), Ok(BackendChoice::WaylandX11));
        assert_eq!("winit".parse(), Ok(BackendChoice::WaylandWinit));
    }

    #[test]
    fn invalid_backend_reports_supported_choices() {
        let error = "invalid".parse::<BackendChoice>().unwrap_err();
        assert!(error.contains("x11rb"));
        assert!(error.contains("wayland-winit"));
    }

    #[test]
    fn backend_family_matches_configuration_format() {
        assert_eq!(BackendChoice::Xcb.family(), BackendFamily::X11);
        assert_eq!(BackendChoice::WaylandUdev.family(), BackendFamily::Wayland);
        assert_eq!(BackendChoice::X11rb.config_name(), "config_x11.toml");
        assert_eq!(
            BackendChoice::WaylandWinit.config_name(),
            "config_wayland.toml"
        );
        assert_eq!(
            config_path(BackendChoice::X11rb)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("config_x11.toml")
        );
    }

    #[test]
    fn explicit_application_options_have_stable_defaults() {
        assert_eq!(
            ApplicationOptions::default(),
            ApplicationOptions {
                backend: BackendChoice::X11rb,
                benchmark: None,
            }
        );
    }

    #[test]
    fn benchmark_configuration_uses_narrow_capability() {
        let mut backend = BenchmarkSpy::default();
        configure_benchmark(
            &mut backend,
            Some(BenchmarkRequest {
                frames: 120,
                warmup: 30,
            }),
        );

        assert_eq!(backend.started, Some((120, 30)));
        assert!(backend.auto_exit);
    }

    #[test]
    fn benchmark_values_are_validated_without_environment_access() {
        assert_eq!(
            parse_benchmark(Some("120"), None),
            Ok(Some(BenchmarkRequest {
                frames: 120,
                warmup: 60,
            }))
        );
        assert!(parse_benchmark(Some("invalid"), Some("10")).is_err());
        assert!(parse_benchmark(Some("0"), Some("10")).is_err());
        assert!(parse_benchmark(None, Some("10")).is_err());
        assert_eq!(parse_benchmark(None, None), Ok(None));
    }
}
