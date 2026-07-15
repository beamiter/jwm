#![deny(unsafe_code)]
#![warn(clippy::all, clippy::pedantic)]

use chrono::{SecondsFormat, Utc};
use clap::Parser;
use jwm::application::BackendChoice;
use jwm::doctor::{self, DoctorReport, DoctorStatus};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const IPC_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_IPC_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_REPORTED_VALUE_CHARS: usize = 256;
const SESSION_ENV_KEYS: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_DESKTOP",
    "DESKTOP_SESSION",
];
const OS_RELEASE_KEYS: &[&str] = &[
    "NAME",
    "PRETTY_NAME",
    "ID",
    "ID_LIKE",
    "VERSION",
    "VERSION_ID",
];

#[derive(Debug, Parser)]
#[command(
    name = "jwm-support",
    version,
    about = "Generate a privacy-aware JWM diagnostics bundle",
    long_about = "Collects JWM's read-only startup doctor report, a small allowlist of\n\
                  desktop-session facts, and optional live IPC health/capability snapshots.\n\
                  HOME, PATH, the D-Bus address, command lines, window titles, and arbitrary\n\
                  environment variables are deliberately excluded."
)]
struct Cli {
    /// Backend whose configuration and startup prerequisites should be checked.
    #[arg(
        long,
        env = "JWM_BACKEND",
        default_value = "x11rb",
        value_parser = parse_backend
    )]
    backend: BackendChoice,

    /// Do not connect to a running JWM instance.
    #[arg(long)]
    offline: bool,

    /// Exit with code 2 when doctor reports an error or a requested live probe fails.
    #[arg(long)]
    strict: bool,

    /// Emit compact JSON instead of pretty-printed JSON.
    #[arg(long)]
    compact: bool,

    /// Atomically write the bundle to this path with mode 0600 instead of stdout.
    #[arg(long, value_name = "PATH")]
    output: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct SupportBundleV1 {
    schema_version: u32,
    generated_at: String,
    generator: GeneratorSnapshot,
    requested_backend: String,
    system: SystemSnapshot,
    session_environment: BTreeMap<String, String>,
    doctor: DoctorReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    live: Option<LiveSnapshot>,
    privacy: PrivacySnapshot,
}

#[derive(Debug, Serialize)]
struct GeneratorSnapshot {
    name: &'static str,
    version: &'static str,
}

#[derive(Debug, Serialize)]
struct SystemSnapshot {
    os: &'static str,
    architecture: &'static str,
    family: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    kernel_release: Option<String>,
    distribution: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct LiveSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    socket: Option<String>,
    health: QueryProbe,
    capabilities: QueryProbe,
}

#[derive(Debug, Serialize)]
struct QueryProbe {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl QueryProbe {
    fn failed(error: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(error.into()),
        }
    }
}

#[derive(Debug, Serialize)]
struct PrivacySnapshot {
    environment_policy: &'static str,
    omitted_categories: &'static [&'static str],
}

fn parse_backend(value: &str) -> Result<BackendChoice, String> {
    value.parse()
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(strict_failure) if cli.strict && strict_failure => ExitCode::from(2),
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("jwm-support: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<bool, Box<dyn std::error::Error>> {
    let doctor = doctor::diagnose(cli.backend);
    let live = (!cli.offline).then(collect_live_snapshot);
    let strict_failure = doctor.status == DoctorStatus::Error
        || live
            .as_ref()
            .is_some_and(|snapshot| !snapshot.health.success || !snapshot.capabilities.success);

    let bundle = SupportBundleV1 {
        schema_version: 1,
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        generator: GeneratorSnapshot {
            name: "jwm-support",
            version: env!("CARGO_PKG_VERSION"),
        },
        requested_backend: cli.backend.as_str().to_string(),
        system: collect_system_snapshot(),
        session_environment: collect_session_environment(),
        doctor,
        live,
        privacy: PrivacySnapshot {
            environment_policy: "allowlist-only; values are control-character stripped and length limited",
            omitted_categories: &[
                "HOME and user paths",
                "PATH and executable search paths",
                "D-Bus addresses and authentication material",
                "process command lines",
                "window titles and application content",
                "unrecognized environment variables",
            ],
        },
    };

    let json = if cli.compact {
        serde_json::to_vec(&bundle)?
    } else {
        serde_json::to_vec_pretty(&bundle)?
    };
    write_output(cli.output.as_deref(), &json)?;
    Ok(strict_failure)
}

fn collect_system_snapshot() -> SystemSnapshot {
    SystemSnapshot {
        os: env::consts::OS,
        architecture: env::consts::ARCH,
        family: env::consts::FAMILY,
        kernel_release: fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|value| sanitize_reported_value(value.trim())),
        distribution: fs::read_to_string("/etc/os-release")
            .map_or_else(|_| BTreeMap::new(), |content| parse_os_release(&content)),
    }
}

fn collect_session_environment() -> BTreeMap<String, String> {
    SESSION_ENV_KEYS
        .iter()
        .filter_map(|key| {
            env::var_os(key).map(|value| {
                (
                    (*key).to_string(),
                    sanitize_reported_value(&value.to_string_lossy()),
                )
            })
        })
        .collect()
}

fn parse_os_release(content: &str) -> BTreeMap<String, String> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, raw_value) = line.split_once('=')?;
            if !OS_RELEASE_KEYS.contains(&key) {
                return None;
            }
            Some((
                key.to_string(),
                sanitize_reported_value(&decode_os_release_value(raw_value)),
            ))
        })
        .collect()
}

fn decode_os_release_value(raw_value: &str) -> String {
    let value = raw_value.trim();
    let unquoted = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value);
    unquoted.replace("\\\"", "\"").replace("\\\\", "\\")
}

fn sanitize_reported_value(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len().min(MAX_REPORTED_VALUE_CHARS));
    for character in value.chars().filter(|character| !character.is_control()) {
        if sanitized.chars().count() >= MAX_REPORTED_VALUE_CHARS {
            sanitized.push('…');
            break;
        }
        sanitized.push(character);
    }
    sanitized
}

fn collect_live_snapshot() -> LiveSnapshot {
    let socket = match jwm::ipc_server::validated_socket_path() {
        Ok(path) => path,
        Err(error) => {
            let message = format!("cannot resolve a safe IPC socket: {error}");
            return LiveSnapshot {
                socket: None,
                health: QueryProbe::failed(message.clone()),
                capabilities: QueryProbe::failed(message),
            };
        }
    };

    LiveSnapshot {
        socket: Some(socket.display().to_string()),
        health: query_ipc(&socket, "get_status"),
        capabilities: query_ipc(&socket, "get_capabilities"),
    }
}

fn query_ipc(socket: &Path, query: &str) -> QueryProbe {
    match query_ipc_value(socket, query) {
        Ok(response) => normalize_ipc_response(response),
        Err(error) => QueryProbe::failed(error),
    }
}

fn query_ipc_value(socket: &Path, query: &str) -> Result<Value, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|error| format!("cannot connect to {}: {error}", socket.display()))?;
    stream
        .set_read_timeout(Some(IPC_TIMEOUT))
        .map_err(|error| format!("cannot set IPC read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(IPC_TIMEOUT))
        .map_err(|error| format!("cannot set IPC write timeout: {error}"))?;

    let mut request = serde_json::to_vec(&json!({ "query": query, "args": null }))
        .map_err(|error| format!("cannot encode IPC request: {error}"))?;
    request.push(b'\n');
    stream
        .write_all(&request)
        .map_err(|error| format!("cannot write IPC request: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("cannot flush IPC request: {error}"))?;

    let reader = BufReader::new(stream);
    let mut limited = reader.take((MAX_IPC_RESPONSE_BYTES + 1) as u64);
    let mut response = Vec::new();
    let read = limited
        .read_until(b'\n', &mut response)
        .map_err(|error| format!("cannot read IPC response: {error}"))?;
    if read == 0 {
        return Err("JWM closed the IPC connection without a response".to_string());
    }
    if response.len() > MAX_IPC_RESPONSE_BYTES {
        return Err(format!(
            "IPC response exceeds the {} byte safety limit",
            MAX_IPC_RESPONSE_BYTES
        ));
    }
    while matches!(response.last(), Some(b'\n' | b'\r')) {
        response.pop();
    }

    serde_json::from_slice(&response).map_err(|error| format!("JWM returned invalid JSON: {error}"))
}

fn normalize_ipc_response(response: Value) -> QueryProbe {
    let Some(success) = response.get("success").and_then(Value::as_bool) else {
        return QueryProbe::failed("IPC response is missing a boolean `success` field");
    };
    if success {
        QueryProbe {
            success: true,
            data: response.get("data").cloned(),
            error: None,
        }
    } else {
        QueryProbe::failed(
            response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("JWM reported an unspecified IPC failure"),
        )
    }
}

fn write_output(path: Option<&Path>, json: &[u8]) -> io::Result<()> {
    if let Some(path) = path {
        write_private_atomic(path, json)
    } else {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        output.write_all(json)?;
        output.write_all(b"\n")?;
        output.flush()
    }
}

fn write_private_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("output directory does not exist: {}", parent.display()),
        ));
    }
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "output path must name a regular file",
        )
    })?;
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        unique_suffix()
    ));

    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(data)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_parser_keeps_only_the_documented_allowlist() {
        let parsed = parse_os_release(
            r#"
NAME="Example Linux"
PRETTY_NAME="Example Linux 42"
ID=example
SECRET_TOKEN=do-not-copy
# COMMENT=value
"#,
        );

        assert_eq!(
            parsed.get("NAME").map(String::as_str),
            Some("Example Linux")
        );
        assert_eq!(
            parsed.get("PRETTY_NAME").map(String::as_str),
            Some("Example Linux 42")
        );
        assert_eq!(parsed.get("ID").map(String::as_str), Some("example"));
        assert!(!parsed.contains_key("SECRET_TOKEN"));
    }

    #[test]
    fn reported_values_strip_controls_and_are_bounded() {
        let input = format!("hello\nworld{}", "x".repeat(MAX_REPORTED_VALUE_CHARS + 20));
        let sanitized = sanitize_reported_value(&input);

        assert!(!sanitized.contains('\n'));
        assert!(sanitized.ends_with('…'));
        assert!(sanitized.chars().count() <= MAX_REPORTED_VALUE_CHARS + 1);
    }

    #[test]
    fn ipc_envelopes_are_normalized_without_copying_protocol_metadata() {
        let success = normalize_ipc_response(json!({
            "success": true,
            "data": { "schema_version": 1, "status": "healthy" }
        }));
        assert!(success.success);
        assert_eq!(
            success.data.as_ref().and_then(|value| value.get("status")),
            Some(&Value::String("healthy".to_string()))
        );

        let failure = normalize_ipc_response(json!({
            "success": false,
            "error": "not available"
        }));
        assert!(!failure.success);
        assert_eq!(failure.error.as_deref(), Some("not available"));
    }

    #[test]
    fn bundle_files_are_private_and_replaced_atomically() {
        let directory = env::temp_dir().join(format!(
            "jwm-support-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("bundle.json");

        write_private_atomic(&path, br#"{"schema_version":1}"#).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"schema_version\":1}\n"
        );
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        fs::remove_dir_all(directory).unwrap();
    }
}
