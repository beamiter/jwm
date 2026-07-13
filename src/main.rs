use clap::Parser;
use jwm::application::{
    ApplicationOptions, BackendChoice, BenchmarkRequest, config_path, generate_config_templates,
    run_with_options, validate_config,
};
use log::{error, info, warn};
use std::env;
use std::os::unix::fs::FileTypeExt;
use std::process::Command;
use xbar_core::initialize_logging;

#[derive(Debug, Parser)]
#[command(
    name = "jwm",
    version,
    about = "JWM window manager and compositor",
    long_about = "JWM window manager and compositor. Startup options can also be supplied through the existing JWM_* environment variables."
)]
struct Cli {
    /// Select the platform backend.
    #[arg(
        long,
        env = "JWM_BACKEND",
        default_value = "x11rb",
        value_parser = parse_backend,
        value_name = "BACKEND"
    )]
    backend: BackendChoice,

    /// Generate fresh X11 and Wayland configuration templates, backing up existing files.
    #[arg(
        long,
        conflicts_with_all = ["check_config", "print_config_path"]
    )]
    gen_config: bool,

    /// Validate the selected backend's configuration and exit.
    #[arg(
        long,
        conflicts_with_all = ["gen_config", "print_config_path"]
    )]
    check_config: bool,

    /// Print the selected backend's configuration path and exit.
    #[arg(
        long,
        conflicts_with_all = ["gen_config", "check_config"]
    )]
    print_config_path: bool,

    /// Collect a compositor benchmark for this many frames, then exit.
    #[arg(
        long,
        env = "JWM_BENCHMARK",
        value_parser = parse_positive_u32,
        value_name = "FRAMES"
    )]
    benchmark: Option<u32>,

    /// Number of warm-up frames excluded from the compositor benchmark.
    #[arg(
        long,
        env = "JWM_BENCHMARK_WARMUP",
        requires = "benchmark",
        value_name = "FRAMES"
    )]
    benchmark_warmup: Option<u32>,

    /// Override the tracing/log filter (same syntax as RUST_LOG).
    #[arg(long = "log", env = "RUST_LOG", value_name = "FILTER")]
    log_filter: Option<String>,
}

fn parse_backend(value: &str) -> Result<BackendChoice, String> {
    value.parse()
}

fn parse_positive_u32(value: &str) -> Result<u32, String> {
    let parsed = value
        .parse::<u32>()
        .map_err(|error| format!("expected a positive integer: {error}"))?;
    if parsed == 0 {
        return Err("value must be greater than zero".to_string());
    }
    Ok(parsed)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    if cli.print_config_path {
        println!("{}", config_path(cli.backend).display());
        return Ok(());
    }

    if cli.check_config {
        let path = validate_config(cli.backend)?;
        println!(
            "Configuration syntax and structure are valid: {}",
            path.display()
        );
        return Ok(());
    }

    if cli.gen_config {
        for generated in generate_config_templates()? {
            if let Some(backup) = generated.backup {
                println!("Backed up existing configuration: {}", backup.display());
            }
            println!("Generated configuration: {}", generated.path.display());
        }
        return Ok(());
    }

    configure_logging(cli.log_filter.as_deref());
    initialize_logging("jwm", "/dev/shm/jwm_bar_global")?;
    install_panic_hook();
    info!("[main] begin");

    setup_locale();
    ensure_dbus_session();

    let benchmark = cli.benchmark.map(|frames| BenchmarkRequest {
        frames,
        warmup: cli.benchmark_warmup.unwrap_or(60),
    });
    run_with_options(ApplicationOptions {
        backend: cli.backend,
        benchmark,
    })?;
    Ok(())
}

fn configure_logging(log_filter: Option<&str>) {
    let filter = log_filter.map_or_else(
        || {
            if cfg!(debug_assertions) {
                "info,jwm=debug,smithay=warn,libseat=warn,drm=warn".to_string()
            } else {
                "info,jwm=info,smithay=warn,libseat=warn,drm=warn".to_string()
            }
        },
        str::to_owned,
    );
    // Environment mutation happens before logging or worker threads are started.
    unsafe { env::set_var("RUST_LOG", filter) };
}

fn install_panic_hook() {
    // Release builds intentionally keep panic unwinding so worker-thread panics
    // do not abort the entire compositor. The hook still records payload,
    // location and a backtrace before unwinding begins.
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
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_else(|| "<unknown location>".to_string());
        let backtrace = std::backtrace::Backtrace::force_capture();

        eprintln!("[panic] {payload} @ {location}\nBacktrace:\n{backtrace:?}");
        error!("[panic] {payload} @ {location} | backtrace={backtrace:?}");
    }));
}

fn effective_locale() -> String {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| env::var(name).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "C".to_string())
}

fn setup_locale() {
    let locale = effective_locale();
    info!("Using locale: {locale}");
    let is_utf8 = locale.contains("UTF-8") || locale.contains("utf8");
    if !is_utf8 {
        warn!("Non-UTF-8 locale detected ({locale}). Text display may be affected.");
        warn!("Consider setting: export LANG=C.UTF-8");
    }

    if env::var("LC_CTYPE").is_err()
        || env::var_os("LC_CTYPE").is_some_and(|value| value.is_empty())
    {
        let ctype = if is_utf8 { locale.as_str() } else { "C.UTF-8" };
        // Environment mutation happens during single-threaded process bootstrap.
        unsafe { env::set_var("LC_CTYPE", ctype) };
    }
}

fn split_shell_statements(output: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut quote = None;

    for character in output.chars() {
        match (quote, character) {
            (None, '\'' | '"') => {
                quote = Some(character);
                current.push(character);
            }
            (Some(active), value) if value == active => {
                quote = None;
                current.push(character);
            }
            (None, ';' | '\n') => {
                let statement = current.trim();
                if !statement.is_empty() {
                    statements.push(statement.to_string());
                }
                current.clear();
            }
            _ => current.push(character),
        }
    }

    let statement = current.trim();
    if !statement.is_empty() {
        statements.push(statement.to_string());
    }
    statements
}

fn parse_dbus_launch_output(output: &str) -> Vec<(String, String)> {
    split_shell_statements(output)
        .into_iter()
        .filter_map(|statement| {
            if statement.starts_with("export ") {
                return None;
            }
            let (key, raw_value) = statement.split_once('=')?;
            let key = key.trim();
            if !key.starts_with("DBUS_") {
                return None;
            }
            let raw_value = raw_value.trim();
            let value = match (
                raw_value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\'')),
                raw_value
                    .strip_prefix('"')
                    .and_then(|value| value.strip_suffix('"')),
            ) {
                (Some(value), _) | (_, Some(value)) => value,
                _ => raw_value,
            };
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// Ensure a D-Bus session bus is available so applications that delegate
/// window creation to a server process can start correctly.
fn ensure_dbus_session() {
    if env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|value| !value.is_empty()) {
        info!("[dbus] session bus already configured");
        return;
    }

    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR") {
        let bus_path = std::path::PathBuf::from(runtime_dir).join("bus");
        if bus_path
            .metadata()
            .is_ok_and(|metadata| metadata.file_type().is_socket())
        {
            let address = format!("unix:path={}", bus_path.display());
            // Environment mutation happens during single-threaded process bootstrap.
            unsafe { env::set_var("DBUS_SESSION_BUS_ADDRESS", &address) };
            info!("[dbus] using systemd session bus at {}", bus_path.display());
            return;
        }
    }

    info!("[dbus] no session bus found, trying dbus-launch");
    let output = match Command::new("dbus-launch").arg("--sh-syntax").output() {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            warn!("[dbus] dbus-launch exited {:?}", output.status);
            return;
        }
        Err(error) => {
            warn!("[dbus] dbus-launch not available: {error}");
            return;
        }
    };

    let mut configured = false;
    for (key, value) in parse_dbus_launch_output(&String::from_utf8_lossy(&output.stdout)) {
        configured |= key == "DBUS_SESSION_BUS_ADDRESS" && !value.is_empty();
        // Values originate from dbus-launch and are applied before worker threads start.
        unsafe { env::set_var(&key, &value) };
        info!("[dbus] configured {key}");
    }

    if !configured {
        warn!("[dbus] dbus-launch returned no DBUS_SESSION_BUS_ADDRESS");
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, parse_dbus_launch_output};
    use clap::Parser;

    #[test]
    fn cli_accepts_backend_alias_and_benchmark_options() {
        let cli = Cli::try_parse_from([
            "jwm",
            "--backend",
            "wayland",
            "--benchmark",
            "120",
            "--benchmark-warmup",
            "30",
        ])
        .unwrap();

        assert_eq!(cli.backend.as_str(), "wayland-udev");
        assert_eq!(cli.benchmark, Some(120));
        assert_eq!(cli.benchmark_warmup, Some(30));
    }

    #[test]
    fn cli_rejects_zero_length_benchmark() {
        assert!(Cli::try_parse_from(["jwm", "--benchmark", "0"]).is_err());
    }

    #[test]
    fn cli_rejects_conflicting_config_actions() {
        assert!(Cli::try_parse_from(["jwm", "--gen-config", "--check-config"]).is_err());
    }

    #[test]
    fn dbus_shell_output_is_parsed_without_export_suffixes() {
        let variables = parse_dbus_launch_output(
            "DBUS_SESSION_BUS_ADDRESS='unix:path=/tmp/dbus-test;guid=abc'; export DBUS_SESSION_BUS_ADDRESS;\nDBUS_SESSION_BUS_PID=4242; export DBUS_SESSION_BUS_PID;",
        );

        assert_eq!(
            variables,
            vec![
                (
                    "DBUS_SESSION_BUS_ADDRESS".to_string(),
                    "unix:path=/tmp/dbus-test;guid=abc".to_string()
                ),
                ("DBUS_SESSION_BUS_PID".to_string(), "4242".to_string()),
            ]
        );
    }

    #[test]
    fn dbus_shell_parser_ignores_unrelated_statements() {
        let variables = parse_dbus_launch_output(
            "export DBUS_SESSION_BUS_ADDRESS; OTHER=value; DBUS_SESSION_BUS_PID='7';",
        );
        assert_eq!(
            variables,
            vec![("DBUS_SESSION_BUS_PID".to_string(), "7".to_string())]
        );
    }
}
