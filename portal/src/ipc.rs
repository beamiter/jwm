#![allow(dead_code)]
//! Best-effort bridge to jwm's IPC socket. Used by the picker to resolve
//! `JWM_PORTAL_WINDOW=class:firefox` style queries against the live window
//! list, so we don't depend on the user having set wm_class properly in the
//! Wayland toplevel-list app_id.
//!
//! Failure is non-fatal — the picker keeps the Wayland-bound app_id/title.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct WindowInfo {
    pub id: u64,
    pub name: String,
    pub class: String,
    pub instance: String,
    #[serde(default)]
    pub tags: u32,
}

fn socket_path() -> PathBuf {
    let runtime_dir =
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(runtime_dir).join("jwm.sock")
}

pub fn query_windows() -> std::io::Result<Vec<WindowInfo>> {
    let mut sock = UnixStream::connect(socket_path())?;
    sock.set_read_timeout(Some(Duration::from_millis(500)))?;
    sock.set_write_timeout(Some(Duration::from_millis(500)))?;
    sock.write_all(b"get_windows\n")?;
    let mut buf = String::new();
    sock.read_to_string(&mut buf)?;
    serde_json::from_str::<Vec<WindowInfo>>(buf.trim())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
