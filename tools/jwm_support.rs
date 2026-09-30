#![deny(unsafe_code)]
#![deny(clippy::correctness, clippy::suspicious, clippy::perf)]
#![allow(clippy::style, clippy::complexity, clippy::pedantic)]

use chrono::{SecondsFormat, Utc};
use clap::Parser;
use jwm::application::BackendChoice;
use jwm::doctor::{self, DoctorReport, DoctorStatus, DoctorSummary};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const IPC_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_IPC_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_REPORTED_VALUE_CHARS: usize = 256;
const MAX_OUTPUT_CREATE_ATTEMPTS: usize = 128;
const MAX_KERNEL_RELEASE_BYTES: u64 = 4 * 1024;
const MAX_OS_RELEASE_BYTES: u64 = 64 * 1024;
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
    /// Backend whose configuration and startup prerequisites should be checked
    /// (default: wayland-udev when compiled in, otherwise the first compiled
    /// backend).
    // No clap `default_value`: a fixed string would name wayland-udev even in
    // slim `--no-default-features` builds that lack it, and the bundle would
    // then diagnose a backend this build cannot run. An absent flag and env
    // var resolve through `BackendChoice::default()` in `Cli::backend`.
    #[arg(long, env = "JWM_BACKEND", value_parser = parse_backend)]
    backend: Option<BackendChoice>,

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
    doctor: SupportDoctorReport,
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
struct SupportDoctorReport {
    schema_version: u32,
    backend: String,
    compiled_backends: Vec<String>,
    status: DoctorStatus,
    summary: DoctorSummary,
    checks: Vec<SupportDoctorCheck>,
    config_diagnostics_included: bool,
}

#[derive(Debug, Serialize)]
struct SupportDoctorCheck {
    status: DoctorStatus,
    id: String,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

impl From<DoctorReport> for SupportDoctorReport {
    fn from(report: DoctorReport) -> Self {
        let checks = report
            .checks
            .into_iter()
            .map(|check| {
                let id = check.id;
                let summary = sanitize_doctor_summary(&id, &check.summary);
                SupportDoctorCheck {
                    status: check.status,
                    detail: sanitize_doctor_detail(&id, check.detail),
                    id,
                    summary,
                    hint: check.hint.map(|hint| sanitize_reported_value(&hint)),
                }
            })
            .collect();

        Self {
            schema_version: report.schema_version,
            backend: report.backend,
            compiled_backends: report.compiled_backends,
            status: report.status,
            summary: report.summary,
            checks,
            config_diagnostics_included: false,
        }
    }
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

impl Cli {
    /// Backend requested through `--backend`/`JWM_BACKEND`, or the build's
    /// compiled-in default when neither is given.
    fn backend(&self) -> BackendChoice {
        self.backend.unwrap_or_default()
    }
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
    let backend = cli.backend();
    let doctor_report = doctor::diagnose(backend);
    let doctor_failed = doctor_report.status == DoctorStatus::Error;
    let doctor = SupportDoctorReport::from(doctor_report);
    let live = (!cli.offline).then(collect_live_snapshot);
    let strict_failure = doctor_failed
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
        requested_backend: backend.as_str().to_string(),
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
    collect_system_snapshot_from_paths(
        Path::new("/proc/sys/kernel/osrelease"),
        Path::new("/etc/os-release"),
    )
}

fn collect_system_snapshot_from_paths(
    kernel_path: &Path,
    distribution_path: &Path,
) -> SystemSnapshot {
    SystemSnapshot {
        os: env::consts::OS,
        architecture: env::consts::ARCH,
        family: env::consts::FAMILY,
        kernel_release: read_system_text(kernel_path, MAX_KERNEL_RELEASE_BYTES)
            .ok()
            .map(|value| sanitize_reported_value(value.trim())),
        distribution: read_system_text(distribution_path, MAX_OS_RELEASE_BYTES)
            .map_or_else(|_| BTreeMap::new(), |content| parse_os_release(&content)),
    }
}

fn read_system_text(path: &Path, max_bytes: u64) -> io::Result<String> {
    // /etc/os-release commonly links to /usr/lib/os-release. Follow ordinary
    // file symlinks, but inspect the opened descriptor so a FIFO replacement
    // cannot block the diagnostics tool before type/size validation.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "system information source is not a regular file",
        ));
    }
    if metadata.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "system information source exceeds its byte limit",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len().min(4096) as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "system information source exceeds its byte limit",
        ));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
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
    let mut reported_characters = 0;
    for character in value.chars().filter(|character| !character.is_control()) {
        if reported_characters >= MAX_REPORTED_VALUE_CHARS {
            sanitized.push('…');
            break;
        }
        sanitized.push(character);
        reported_characters += 1;
    }
    sanitized
}

fn sanitize_doctor_summary(id: &str, summary: &str) -> String {
    if id == "command.status_bar" && summary.starts_with("Configured status bar ") {
        if summary.ends_with(" is executable") {
            return "Configured status bar is executable".to_string();
        }
        if summary.ends_with(" is not executable from PATH") {
            return "Configured status bar is not executable from PATH".to_string();
        }
        return "Configured status bar check completed".to_string();
    }
    sanitize_reported_value(summary)
}

fn sanitize_doctor_detail(id: &str, detail: Option<String>) -> Option<String> {
    let detail = detail?;
    match id {
        "backend.display" | "backend.host_display" | "backend.dri" | "session.dbus" => {
            Some(sanitize_reported_value(&detail))
        }
        "config.file" => Some("<configuration path redacted>".to_string()),
        "runtime.xdg_runtime_dir" => Some("<runtime directory details redacted>".to_string()),
        "command.jwm_tool" | "command.status_bar" => Some("<executable path redacted>".to_string()),
        _ => None,
    }
}

fn sanitize_live_data(query: &str, data: &mut Value) {
    if query != "get_status" {
        return;
    }

    if let Some(reasons) = data
        .get_mut("health")
        .and_then(Value::as_object_mut)
        .and_then(|health| health.get_mut("reasons"))
        .and_then(Value::as_array_mut)
    {
        for reason in reasons {
            let Some(text) = reason.as_str() else {
                continue;
            };
            *reason = Value::String(if text.starts_with("last configuration reload failed:") {
                "last configuration reload failed (detail redacted)".to_string()
            } else if text.starts_with("last compositor transition failed:") {
                "last compositor transition failed (detail redacted)".to_string()
            } else {
                sanitize_reported_value(text)
            });
        }
    }

    if let Some(transition) = data
        .get_mut("compositor_transition")
        .and_then(Value::as_object_mut)
    {
        let error_present = transition
            .get("last_error")
            .is_some_and(|error| !error.is_null());
        transition.insert("last_error".to_string(), Value::Null);
        transition.insert("last_error_present".to_string(), Value::Bool(error_present));
    }

    let Some(config) = data.get_mut("config").and_then(Value::as_object_mut) else {
        return;
    };

    config.insert(
        "path".to_string(),
        Value::String("<configuration path redacted>".to_string()),
    );
    if let Some(diagnostics) = config.get_mut("diagnostics").and_then(Value::as_object_mut) {
        diagnostics.remove("issues");
        diagnostics.insert("issues_included".to_string(), Value::Bool(false));
    }
    if let Some(reload) = config.get_mut("reload").and_then(Value::as_object_mut) {
        let error_present = reload
            .get("last_error")
            .is_some_and(|error| !error.is_null());
        reload.insert("last_error".to_string(), Value::Null);
        reload.insert("last_error_present".to_string(), Value::Bool(error_present));
    }
}
fn collect_live_snapshot() -> LiveSnapshot {
    let socket = match jwm::ipc_server::validated_socket_path() {
        Ok(path) => path,
        Err(_) => {
            let message = "cannot resolve a safe IPC socket".to_string();
            return LiveSnapshot {
                socket: None,
                health: QueryProbe::failed(message.clone()),
                capabilities: QueryProbe::failed(message),
            };
        }
    };

    LiveSnapshot {
        socket: Some("<validated runtime socket redacted>".to_string()),
        health: query_ipc(&socket, "get_status"),
        capabilities: query_ipc(&socket, "get_capabilities"),
    }
}

fn query_ipc(socket: &Path, query: &str) -> QueryProbe {
    match query_ipc_value(socket, query) {
        Ok(response) => {
            let mut probe = normalize_ipc_response(response);
            if let Some(data) = probe.data.as_mut() {
                sanitize_live_data(query, data);
            }
            probe
        }
        Err(_) => QueryProbe::failed(format!(
            "live IPC query {query:?} failed; inspect locally with jwm-tool"
        )),
    }
}

fn query_ipc_value(socket: &Path, query: &str) -> Result<Value, String> {
    let stream = jwm::ipc_connection::connect(socket, IPC_TIMEOUT)
        .map_err(|error| format!("cannot connect to {}: {error}", socket.display()))?;
    let mut request = serde_json::to_vec(&json!({ "query": query, "args": null }))
        .map_err(|error| format!("cannot encode IPC request: {error}"))?;
    request.push(b'\n');
    ipc_exchange(stream, &request, IPC_TIMEOUT)
        .map_err(|error| format!("cannot exchange IPC request: {error}"))
}

fn ipc_remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "IPC I/O deadline exceeded"))
}

// The connection has a separate budget. Once connected, partial request writes
// and response reads share this deadline; a trickling peer cannot renew it.
fn ipc_exchange(mut stream: UnixStream, request: &[u8], timeout: Duration) -> io::Result<Value> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "IPC timeout is too large"))?;
    jwm::ipc_connection::write_all_with_timeout(&mut stream, request, ipc_remaining(deadline)?)?;

    let mut response = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        stream.set_read_timeout(Some(ipc_remaining(deadline)?))?;
        let room = (MAX_IPC_RESPONSE_BYTES + 1 - response.len()).min(buffer.len());
        let count = match stream.read(&mut buffer[..room]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "JWM closed the IPC connection before a complete response frame",
                ));
            }
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let newline = buffer[..count].iter().position(|byte| *byte == b'\n');
        response.extend_from_slice(&buffer[..newline.map_or(count, |index| index + 1)]);
        if response.len() > MAX_IPC_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("IPC response exceeds the {MAX_IPC_RESPONSE_BYTES} byte safety limit"),
            ));
        }
        if newline.is_some() {
            break;
        }
    }
    while matches!(response.last(), Some(b'\n' | b'\r')) {
        response.pop();
    }
    serde_json::from_slice(&response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
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
    write_private_atomic_with_suffixes(path, data, std::iter::repeat_with(unique_suffix))
}

fn write_private_atomic_with_suffixes(
    path: &Path,
    data: &[u8],
    suffixes: impl IntoIterator<Item = u128>,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

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
    for suffix in suffixes.into_iter().take(MAX_OUTPUT_CREATE_ATTEMPTS) {
        let temporary = parent.join(format!(
            ".{}.{}.{suffix}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
        ));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        // Cleanup is reached only after create_new proved ownership. An
        // occupied name can belong to another writer and must remain intact.
        let result = (|| {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(data)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
            return result;
        }
        // The name was released by rename. A durability error must not
        // unlink a new writer's file that reused the old temporary path.
        return fs::File::open(parent)?.sync_all();
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "support bundle temporary namespace is exhausted",
    ))
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
    use clap::{CommandFactory, FromArgMatches};
    use jwm::doctor::DoctorCheck;
    use std::os::unix::fs::PermissionsExt;

    /// Parse `args` with `JWM_BACKEND` unbound, so the ambient environment
    /// cannot decide what a bare invocation resolves to.
    fn parse_without_backend_env(args: &[&str]) -> Cli {
        let matches = Cli::command()
            .mut_arg("backend", |arg| arg.env(None))
            .try_get_matches_from(args)
            .expect("the support CLI accepts these arguments");
        Cli::from_arg_matches(&matches).expect("matches convert back into Cli")
    }

    fn exchange_response(response: Vec<u8>) -> io::Result<Value> {
        let (client, mut server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let worker = std::thread::spawn(move || -> io::Result<()> {
            let mut request = [0; 8];
            server.read_exact(&mut request)?;
            assert_eq!(&request, b"request\n");
            server.write_all(&response)
        });
        let result = ipc_exchange(client, b"request\n", Duration::from_secs(1));
        let sent = worker.join().unwrap();
        if result.is_ok() {
            sent.unwrap();
        }
        result
    }

    #[test]
    fn ipc_response_requires_a_complete_frame_and_accepts_crlf() {
        for incomplete in [Vec::new(), br#"{"success":true}"#.to_vec()] {
            assert_eq!(
                exchange_response(incomplete).unwrap_err().kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
        for invalid in [b"not JSON\n".to_vec(), vec![0xff, b'\n']] {
            assert_eq!(
                exchange_response(invalid).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        let valid =
            exchange_response(b"{\"success\":true}\r\nignored second frame\n".to_vec()).unwrap();
        assert_eq!(valid, json!({"success": true}));
    }

    #[test]
    fn ipc_response_limit_includes_the_newline() {
        let mut exact = br#"{"success":true}"#.to_vec();
        exact.resize(MAX_IPC_RESPONSE_BYTES - 1, b' ');
        exact.push(b'\n');
        assert_eq!(exchange_response(exact).unwrap(), json!({"success": true}));

        let mut oversized = br#"{"success":true}"#.to_vec();
        oversized.resize(MAX_IPC_RESPONSE_BYTES, b' ');
        oversized.push(b'\n');
        assert_eq!(
            exchange_response(oversized).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn slow_ipc_response_cannot_restart_the_io_deadline() {
        let (client, mut server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let worker = std::thread::spawn(move || -> io::Result<()> {
            let mut request = [0; 8];
            server.read_exact(&mut request)?;
            for _ in 0..100 {
                if server.write_all(b" ").is_err() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            server.write_all(b"{\"success\":true}\n")
        });
        let started = Instant::now();
        let result = ipc_exchange(client, b"request\n", Duration::from_millis(70));
        let elapsed = started.elapsed();
        // Join before asserting so failures cannot leak a live worker.
        let _ = worker.join().unwrap();
        let error = result.unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        assert!(elapsed < Duration::from_secs(1));
    }

    #[test]
    fn cli_backend_has_no_hardcoded_clap_default() {
        // A fixed clap default bypasses `BackendChoice::default()` and names
        // wayland-udev even in builds that do not compile it in, so a slim
        // build's bundle would diagnose a backend it cannot run.
        let command = Cli::command();
        let backend = command
            .get_arguments()
            .find(|arg| arg.get_id() == "backend")
            .expect("the support CLI has a backend argument");
        assert!(backend.get_default_values().is_empty());
    }

    #[test]
    fn bare_cli_resolves_backend_through_compiled_in_default() {
        let cli = parse_without_backend_env(&["jwm-support", "--offline"]);
        assert_eq!(cli.backend, None);
        assert_eq!(cli.backend(), BackendChoice::default());
        assert!(cli.backend().is_compiled());
        #[cfg(feature = "backend-wayland-udev")]
        assert_eq!(cli.backend(), BackendChoice::WaylandUdev);
    }

    #[test]
    fn explicit_backend_flag_overrides_compiled_in_default() {
        let cli = parse_without_backend_env(&["jwm-support", "--backend", "xcb"]);
        assert_eq!(cli.backend(), BackendChoice::Xcb);
    }

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
    fn doctor_report_redacts_paths_and_drops_config_diagnostics() {
        let report = DoctorReport {
            schema_version: 1,
            backend: "x11rb".to_string(),
            compiled_backends: vec!["x11rb".to_string()],
            status: DoctorStatus::Pass,
            summary: DoctorSummary {
                passed: 1,
                warnings: 0,
                errors: 0,
            },
            checks: vec![DoctorCheck {
                status: DoctorStatus::Pass,
                id: "config.file".to_string(),
                summary: "Configuration exists".to_string(),
                detail: Some("/home/alice/.config/jwm/config_x11.toml".to_string()),
                hint: None,
                config_diagnostics: None,
            }],
        };

        let encoded = serde_json::to_string(&SupportDoctorReport::from(report)).unwrap();
        assert!(!encoded.contains("/home/alice"));
        assert!(encoded.contains("configuration path redacted"));
        assert!(encoded.contains("\"config_diagnostics_included\":false"));
    }

    #[test]
    fn doctor_report_redacts_status_bar_commands_from_summaries() {
        let report = DoctorReport {
            schema_version: 1,
            backend: "x11rb".to_string(),
            compiled_backends: vec!["x11rb".to_string()],
            status: DoctorStatus::Pass,
            summary: DoctorSummary {
                passed: 1,
                warnings: 0,
                errors: 0,
            },
            checks: vec![DoctorCheck {
                status: DoctorStatus::Pass,
                id: "command.status_bar".to_string(),
                summary: "Configured status bar \"/home/alice/private-bar\" is executable"
                    .to_string(),
                detail: Some("/home/alice/private-bar".to_string()),
                hint: None,
                config_diagnostics: None,
            }],
        };

        let encoded = serde_json::to_string(&SupportDoctorReport::from(report)).unwrap();
        assert!(!encoded.contains("/home/alice"));
        assert!(encoded.contains("Configured status bar is executable"));
    }

    #[test]
    fn live_status_drops_config_paths_issue_details_and_runtime_errors() {
        let mut status = json!({
            "health": {
                "reasons": [
                    "configuration has 1 error(s)",
                    "last configuration reload failed: /home/alice/private",
                    "last compositor transition failed: /home/alice/gpu-state"
                ]
            },
            "compositor_transition": {
                "attempts": 1,
                "last_error": "failed under /home/alice"
            },
            "config": {
                "path": "/home/alice/.config/jwm/config_x11.toml",
                "diagnostics": {
                    "error_count": 1,
                    "issues": [{"detail": "/home/alice/private"}]
                },
                "reload": {"last_error": "failed under /home/alice"}
            }
        });

        sanitize_live_data("get_status", &mut status);
        let encoded = serde_json::to_string(&status).unwrap();
        assert!(!encoded.contains("/home/alice"));
        assert_eq!(status["config"]["diagnostics"]["issues_included"], false);
        assert!(status["config"]["diagnostics"].get("issues").is_none());
        assert_eq!(status["config"]["reload"]["last_error"], Value::Null);
        assert_eq!(status["config"]["reload"]["last_error_present"], true);
        assert_eq!(status["compositor_transition"]["last_error"], Value::Null);
        assert_eq!(status["compositor_transition"]["last_error_present"], true);
        assert_eq!(
            status["health"]["reasons"][1],
            "last configuration reload failed (detail redacted)"
        );
        assert_eq!(
            status["health"]["reasons"][2],
            "last compositor transition failed (detail redacted)"
        );
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

    #[test]
    fn system_information_reads_are_bounded_and_preserve_regular_symlinks() {
        let directory = env::temp_dir().join(format!(
            "jwm-support-system-info-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir(&directory).unwrap();
        let kernel = directory.join("kernel");
        let release = directory.join("os-release");
        let link = directory.join("release-link");
        fs::write(&kernel, "6.1-test\n").unwrap();
        fs::write(&release, "NAME=JWM Test\nID=jwm-test\nPRIVATE=hidden\n").unwrap();
        std::os::unix::fs::symlink(&release, &link).unwrap();
        let snapshot = collect_system_snapshot_from_paths(&kernel, &link);
        assert_eq!(snapshot.kernel_release.as_deref(), Some("6.1-test"));
        assert_eq!(snapshot.distribution["ID"], "jwm-test");
        assert!(!snapshot.distribution.contains_key("PRIVATE"));

        assert_eq!(read_system_text(&kernel, 9).unwrap(), "6.1-test\n");
        assert_eq!(
            read_system_text(&kernel, 8).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::File::create(&release)
            .unwrap()
            .set_len(MAX_OS_RELEASE_BYTES + 1)
            .unwrap();
        assert!(
            collect_system_snapshot_from_paths(&kernel, &link)
                .distribution
                .is_empty()
        );

        let fifo = directory.join("fifo");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        assert_eq!(
            read_system_text(&fifo, 32).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let unavailable = collect_system_snapshot_from_paths(&fifo, &directory);
        assert!(unavailable.kernel_release.is_none());
        assert!(unavailable.distribution.is_empty());
        fs::write(&kernel, [0xff]).unwrap();
        assert_eq!(
            read_system_text(&kernel, 32).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn support_output_retries_occupied_temporaries_without_deleting_them() {
        let directory = env::temp_dir().join(format!(
            "jwm-support-collision-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("bundle.json");
        let occupied = directory.join(format!(".bundle.json.{}.7.tmp", std::process::id()));
        let available = directory.join(format!(".bundle.json.{}.8.tmp", std::process::id()));
        fs::write(&path, "previous\n").unwrap();
        fs::write(&occupied, "other writer").unwrap();

        write_private_atomic_with_suffixes(&path, b"replacement", [7, 8]).unwrap();

        assert_eq!(fs::read_to_string(&occupied).unwrap(), "other writer");
        assert_eq!(fs::read_to_string(&path).unwrap(), "replacement\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!available.exists());

        let exhausted =
            write_private_atomic_with_suffixes(&path, b"uncommitted", std::iter::repeat(7))
                .unwrap_err();
        assert_eq!(exhausted.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&occupied).unwrap(), "other writer");
        assert_eq!(fs::read_to_string(&path).unwrap(), "replacement\n");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn support_output_failed_commit_removes_only_its_owned_temporary() {
        let directory = env::temp_dir().join(format!(
            "jwm-support-failed-commit-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("occupied-directory");
        fs::create_dir(&path).unwrap();
        let sentinel = path.join("keep");
        fs::write(&sentinel, "preserved").unwrap();
        let temporary = directory.join(format!(".occupied-directory.{}.9.tmp", std::process::id()));

        assert!(write_private_atomic_with_suffixes(&path, b"uncommitted", [9]).is_err());

        assert_eq!(fs::read_to_string(sentinel).unwrap(), "preserved");
        assert!(!temporary.exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
