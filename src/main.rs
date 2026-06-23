// src/main.rs
use jwm::Jwm;
use log::{error, info, warn};
use std::os::unix::process::CommandExt;
use std::{env, process::Command, sync::atomic::Ordering};
use xbar_core::initialize_logging;

use jwm::config::{set_backend_family, BackendFamily};

// 导入后端
use jwm::backend::wayland_udev::backend::UdevBackend;
use jwm::backend::wayland_winit::backend::WaylandWinitBackend;
use jwm::backend::wayland_x11::backend::WaylandX11Backend;
use jwm::backend::x11::backend::X11Backend;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Suppress verbose third-party crate spam unless the caller already set RUST_LOG.
    // In debug builds default to the `debug` level so the maximum amount of
    // diagnostic output is visible; release builds stay at `info`.
    if env::var("RUST_LOG").is_err() {
        let default_level = if cfg!(debug_assertions) { "debug" } else { "info" };
        unsafe {
            env::set_var(
                "RUST_LOG",
                format!("{default_level},smithay=warn,libseat=warn,drm=warn"),
            )
        };
    }

    // --gen-config: generate backend-specific config templates for both backends, then exit.
    if env::args().any(|a| a == "--gen-config") {
        let jwm_dir = jwm::config::Config::get_config_path_for(BackendFamily::X11)
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(&jwm_dir)?;

        for family in [BackendFamily::X11, BackendFamily::Wayland] {
            set_backend_family(family);
            let path = jwm::config::Config::get_config_path_for(family);
            if path.exists() {
                let backup = jwm::config::Config::backup_config(&path)?;
                println!("备份旧配置 -> {}", backup.display());
            }
            jwm::config::Config::generate_template(&path)?;
            println!("配置已生成: {}", path.display());
        }
        return Ok(());
    }

    // Use a generic shared memory path for logging (not used for bars anymore)
    initialize_logging("jwm", "/dev/shm/jwm_bar_global")?;
    install_panic_hook();
    info!("[main] begin");

    setup_locale();

    let is_restart = env::var("JWM_RESTARTING").is_ok();
    if is_restart {
        info!("[main] Detected exec restart, skipping autostart");
        unsafe { env::remove_var("JWM_RESTARTING") };
    }
    ensure_dbus_session();

    jwm::miscellaneous::init_auto_command();
    jwm::miscellaneous::init_auto_start();

    run_jwm()?;
    Ok(())
}

fn install_panic_hook() {
    // In this workspace the release profile is configured with `panic = "abort"`.
    // That means panics often look like a “crash with no logs”.
    // Installing a hook lets us capture the panic payload + location (and a backtrace)
    // into the regular log output before abort.
    std::panic::set_hook(Box::new(|panic_info| {
        let payload = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };

        let location = panic_info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());

        let backtrace = std::backtrace::Backtrace::force_capture();

        // Best-effort: log to both stderr and our logger.
        eprintln!("[panic] {payload} @ {location}\nBacktrace:\n{backtrace:?}");
        error!("[panic] {payload} @ {location} | backtrace={backtrace:?}");
    }));
}

fn run_jwm() -> Result<(), Box<dyn std::error::Error>> {
    loop {
        info!("[main] Starting JWM instance");

        let mut backend = select_backend()?;

        backend.check_existing_wm()?;

        let mut jwm = Jwm::new(&mut *backend)?;
        jwm.setup(&mut *backend)?;
        jwm.setup_initial_windows(&mut *backend)?;

        // JWM_BENCHMARK=N: auto-start benchmark collecting N frames then exit with JSON
        if let Ok(val) = env::var("JWM_BENCHMARK") {
            if let Ok(frames) = val.parse::<u32>() {
                let warmup = env::var("JWM_BENCHMARK_WARMUP")
                    .ok()
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(60);
                backend.compositor_benchmark_start(frames, warmup);
                backend.compositor_benchmark_set_auto_exit(true);
                info!("Benchmark mode: collecting {} frames (warmup={})", frames, warmup);
            }
        }

        jwm.run(&mut *backend)?;
        jwm.cleanup(&mut *backend)?;

        if jwm.is_restarting.load(Ordering::SeqCst) {
            info!("[main] Restarting JWM via exec (picks up new binary from disk)...");
            drop(jwm);
            drop(backend);

            // exec() 替换当前进程 image，这样 restart 后一定使用磁盘上的新 binary
            let args: Vec<String> = env::args().collect();
            let err = Command::new(&args[0])
                .args(&args[1..])
                .env("JWM_RESTARTING", "1")
                .exec();
            // exec() 只在失败时返回，回退到进程内 restart
            error!("[main] exec failed: {err}, falling back to in-process restart");
            continue;
        }

        if let Err(_) = Command::new("jwm-tool").arg("quit").spawn() {
            error!("[main] Failed to quit jwm daemon");
        }
        break;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackendChoice {
    X11,
    WaylandUdev,
    WaylandX11,
    WaylandWinit,
}

fn select_backend() -> Result<Box<dyn jwm::backend::api::Backend>, Box<dyn std::error::Error>> {
    // Selection rule:
    // - If JWM_BACKEND is set, honor it.
    // - Otherwise, default to x11.
    let resolved = if let Ok(val) = env::var("JWM_BACKEND") {
        let val = val.to_lowercase();
        match val.as_str() {
            "x11" => BackendChoice::X11,
            "wayland-udev" | "udev" | "wayland" => BackendChoice::WaylandUdev,
            "wayland-x11" | "x11-wayland" | "windowed" => BackendChoice::WaylandX11,
            "wayland-winit" | "winit" => BackendChoice::WaylandWinit,
            other => {
                return Err(format!(
                    "Unknown JWM_BACKEND={other:?}; expected 'x11'|'wayland-udev'|'wayland-x11'|'wayland-winit'"
                )
                .into());
            }
        }
    } else {
        BackendChoice::X11
    };

    // Register backend family with the config system BEFORE CONFIG is first accessed.
    let family = match resolved {
        BackendChoice::X11 => BackendFamily::X11,
        BackendChoice::WaylandUdev | BackendChoice::WaylandX11 | BackendChoice::WaylandWinit => {
            BackendFamily::Wayland
        }
    };
    set_backend_family(family);

    match resolved {
        BackendChoice::X11 => {
            info!("Initializing X11 Backend (config: config_x11.toml)");
            Ok(Box::new(X11Backend::new()?))
        }
        BackendChoice::WaylandUdev => {
            info!("Initializing Wayland/Udev Backend (config: config_wayland.toml)");
            Ok(Box::new(UdevBackend::new()?))
        }
        BackendChoice::WaylandX11 => {
            info!("Initializing Wayland-on-X11 Backend (config: config_wayland.toml)");
            Ok(Box::new(WaylandX11Backend::new()?))
        }
        BackendChoice::WaylandWinit => {
            info!("Initializing Wayland/Winit Backend (config: config_wayland.toml)");
            Ok(Box::new(WaylandWinitBackend::new()?))
        }
    }
}

pub fn setup_locale() {
    let locale = env::var("LANG")
        .or_else(|_| env::var("LC_ALL"))
        .or_else(|_| env::var("LC_CTYPE"))
        .unwrap_or_else(|_| "C".to_string());
    info!("Using locale: {}", locale);
    if !locale.contains("UTF-8") && !locale.contains("utf8") {
        warn!(
            "Non-UTF-8 locale detected ({}). Text display may be affected.",
            locale
        );
        warn!("Consider setting: export LANG=en_US.UTF-8");
    }
    if env::var("LC_CTYPE").is_err() {
        if locale.contains("UTF-8") {
            unsafe {
                env::set_var("LC_CTYPE", &locale);
            }
        } else {
            unsafe {
                env::set_var("LC_CTYPE", "en_US.UTF-8");
            }
        }
    }
}

/// Ensure a D-Bus session bus is available so that apps like gnome-terminal
/// (which delegate window creation to a server process via D-Bus) can work.
///
/// Priority:
/// 1. `DBUS_SESSION_BUS_ADDRESS` already set → nothing to do.
/// 2. systemd user session socket at `$XDG_RUNTIME_DIR/bus` → point to it.
/// 3. Fall back to `dbus-launch --sh-syntax` → parse and export the result.
fn ensure_dbus_session() {
    if env::var("DBUS_SESSION_BUS_ADDRESS").is_ok() {
        info!("[dbus] session bus already configured");
        return;
    }

    // Systemd places the user session socket here; covers most modern distros.
    if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
        let bus_path = std::path::PathBuf::from(&runtime_dir).join("bus");
        if bus_path.exists() {
            let addr = format!("unix:path={}", bus_path.display());
            unsafe { env::set_var("DBUS_SESSION_BUS_ADDRESS", &addr) };
            info!("[dbus] using systemd session bus: {}", addr);
            return;
        }
    }

    // No pre-existing socket — try spawning a private D-Bus daemon.
    info!("[dbus] no session bus found, trying dbus-launch...");
    let output = match Command::new("dbus-launch").arg("--sh-syntax").output() {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            warn!("[dbus] dbus-launch exited {:?}", o.status);
            return;
        }
        Err(e) => {
            warn!("[dbus] dbus-launch not available: {e}");
            return;
        }
    };

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        // Format: DBUS_SESSION_BUS_ADDRESS='unix:...'; export DBUS_SESSION_BUS_ADDRESS;
        let stripped = line.trim().trim_end_matches(';').trim();
        if let Some(eq) = stripped.find('=') {
            let key = &stripped[..eq];
            if key.starts_with("DBUS_") {
                let val = stripped[eq + 1..].trim_matches('\'').trim_matches('"');
                unsafe { env::set_var(key, val) };
                info!("[dbus] set {}={}", key, val);
            }
        }
    }
}
