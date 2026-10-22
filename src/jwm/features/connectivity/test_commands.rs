//! In-memory connectivity commands for unit tests. Each test thread owns its
//! radios; BackgroundJob explicitly shares them with its workers. No PATH or
//! process-wide environment changes, and no fallback to host network tools.

use std::cell::RefCell;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Output};
use std::sync::{Arc, Mutex};

pub(crate) struct Radios {
    wifi: bool,
    bluetooth: bool,
    calls: Vec<(String, Vec<String>)>,
}

impl Default for Radios {
    fn default() -> Self {
        Self {
            wifi: true,
            bluetooth: true,
            calls: Vec::new(),
        }
    }
}

thread_local! {
    static RADIOS: RefCell<Arc<Mutex<Radios>>> = RefCell::new(Arc::new(Mutex::new(Radios::default())));
}

pub(crate) fn capture() -> Arc<Mutex<Radios>> {
    RADIOS.with(|radios| Arc::clone(&radios.borrow()))
}

pub(crate) fn install(radios: Arc<Mutex<Radios>>) {
    RADIOS.with(|slot| *slot.borrow_mut() = radios);
}

pub(crate) fn calls() -> Vec<(String, Vec<String>)> {
    capture().lock().unwrap().calls.clone()
}

// The locale, stdin and timeout tests deliberately supply shell programs.
// All production connectivity commands, including env-wrapped rfkill, stay
// in the fake executor. Unknown commands fail closed instead of spawning.
pub(super) fn is_shell_fixture(cmd: &str, args: &[&str]) -> bool {
    cmd == "sh" || (cmd == "env" && args.starts_with(&["LC_ALL=C", "sh"]))
}

pub(super) fn output(cmd: &str, args: &[&str]) -> io::Result<Output> {
    let radios = capture();
    let mut radios = radios.lock().unwrap();
    radios.calls.push((
        cmd.to_string(),
        args.iter().map(|arg| (*arg).to_string()).collect(),
    ));
    let stdout = match (cmd, args) {
        ("nmcli", ["-t", "radio", "wifi"]) => {
            if radios.wifi {
                "enabled\n"
            } else {
                "disabled\n"
            }
        }
        ("nmcli", ["radio", "wifi", enabled @ ("on" | "off")]) => {
            radios.wifi = *enabled == "on";
            ""
        }
        ("bluetoothctl", ["show"]) => {
            if radios.bluetooth {
                "Controller 00:00:00:00:00:00 Test\n\tPowered: yes\n"
            } else {
                "Controller 00:00:00:00:00:00 Test\n\tPowered: no\n"
            }
        }
        ("bluetoothctl", ["power", enabled @ ("on" | "off")]) => {
            radios.bluetooth = *enabled == "on";
            ""
        }
        (
            "nmcli",
            [
                "-t",
                "-f",
                "NAME,TYPE,DEVICE",
                "connection",
                "show",
                "--active",
            ],
        )
        | ("nmcli", ["-t", "-f", "NAME" | "NAME,UUID", "connection", "show"])
        | (
            "nmcli",
            [
                "-t",
                "-f",
                "IN-USE,SSID,SIGNAL,SECURITY",
                "dev",
                "wifi",
                "list",
            ],
        )
        | ("bluetoothctl", ["devices"]) => "",
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unmocked connectivity command is disabled in unit tests",
            ));
        }
    };
    Ok(Output {
        status: ExitStatus::from_raw(0),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    })
}
