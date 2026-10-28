//! The task dock uses JWM's backend-neutral window inventory, including native
//! Wayland clients. Requests have bounded frame sizes and a short deadline.
use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Task {
    pub id: u64,
    pub name: String,
    pub monitor: i32,
    pub is_focused: bool,
    pub is_minimized: bool,
    pub is_dock: bool,
    pub is_desktop: bool,
    pub is_swallowed: bool,
}

#[derive(Debug, Deserialize)]
pub struct Monitor {
    pub num: i32,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub show_bar: bool,
    pub has_visible_fullscreen: bool,
    pub locked: bool,
}

pub fn monitor(number: i32) -> Result<Option<Monitor>> {
    let monitors: Vec<Monitor> = serde_json::from_value(request("get_monitors", Value::Null)?)?;
    Ok(monitors.into_iter().find(|monitor| monitor.num == number))
}

pub fn request(command: &str, args: Value) -> Result<Value> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| {
            // geteuid has no preconditions; this is the server's documented
            // fallback directory, never a discovered credential path.
            PathBuf::from(format!("/tmp/jwm-{}", unsafe { libc::geteuid() }))
        });
    let timeout = Duration::from_millis(100);
    let deadline = Instant::now() + timeout;
    let mut stream = jwm_ipc_transport::connect(&runtime.join("jwm-ipc.sock"), timeout)?;
    let message = if command.starts_with("get_") {
        serde_json::json!({"query": command, "args": args})
    } else {
        serde_json::json!({"command": command, "args": args})
    };
    let mut bytes = serde_json::to_vec(&message)?;
    bytes.push(b'\n');
    jwm_ipc_transport::write_all_with_timeout(
        &mut stream,
        &bytes,
        deadline.saturating_duration_since(Instant::now()),
    )?;
    let frame = jwm_ipc_transport::read_frame_until(
        &mut std::io::BufReader::new(stream),
        1024 * 1024,
        deadline,
    )?;
    let response: Value = serde_json::from_slice(&frame)?;
    if response.get("success").and_then(Value::as_bool) != Some(true) {
        bail!("JWM rejected {command}");
    }
    Ok(response.get("data").cloned().unwrap_or(Value::Null))
}

pub fn tasks(monitor: i32) -> Result<Vec<Task>> {
    let value = request("get_windows", Value::Null)?;
    let tasks: Vec<Task> = serde_json::from_value(value)?;
    Ok(tasks
        .into_iter()
        .filter(|task| {
            task.monitor == monitor && !task.is_dock && !task.is_desktop && !task.is_swallowed
        })
        .take(64)
        .collect())
}
