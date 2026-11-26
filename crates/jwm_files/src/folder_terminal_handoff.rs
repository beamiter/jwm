//! Launch a terminal from Files with a held native directory, never a shell command string.
use crate::Shared;
use gtk4::{gio, glib, prelude::*};
use jwm_file_core::{FileIdentity, TerminalChildSetup, TerminalHandoff, TerminalLocator};
use std::{
    cell::Cell,
    ffi::{OsStr, OsString},
    fs, io,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    rc::Rc,
    time::Duration,
};

const CHILD_FLAG: &str = "--internal-files-terminal-child";

struct Launcher {
    program: PathBuf,
    needs_execute_flag: bool,
    safe_path: OsString,
}

fn executable(path: &Path) -> bool {
    path.is_absolute()
        && fs::metadata(path)
            .is_ok_and(|stat| stat.is_file() && stat.permissions().mode() & 0o111 != 0)
}

fn pick_launcher(configured_path: &OsStr) -> io::Result<Launcher> {
    let dirs: Vec<_> = std::env::split_paths(configured_path)
        .filter(|path| path.is_absolute())
        .collect();
    let candidates = [("xdg-terminal-exec", false), ("x-terminal-emulator", true)];
    for (name, needs_execute_flag) in candidates {
        let program = dirs.iter().map(|dir| dir.join(name)).find(|p| executable(p));
        if let Some(program) = program {
            return Ok(Launcher {
                program,
                needs_execute_flag,
                safe_path: std::env::join_paths(&dirs).map_err(io::Error::other)?,
            });
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no xdg-terminal-exec or x-terminal-emulator is available",
    ))
}

fn spawn_launcher(launcher: &Launcher, helper: &Path, pin: &TerminalHandoff) -> io::Result<Child> {
    let locator = pin.locator();
    let args = [
        locator.owner.to_string(),
        locator.directory.to_string(),
        locator.reply.to_string(),
        locator.expected.device.to_string(),
        locator.expected.inode.to_string(),
    ];
    let mut command = Command::new(&launcher.program);
    command
        .env("PATH", &launcher.safe_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if launcher.needs_execute_flag {
        command.arg("-e");
    }
    // The desktop terminal only sees a trusted absolute executable and
    // numeric arguments, never a folder pathname it can re-resolve.
    command.arg(helper).arg(CHILD_FLAG).args(args).spawn()
}

fn bad_request() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid terminal child arguments")
}

fn field<T: std::str::FromStr>(value: &OsStr) -> io::Result<T> {
    let text = value.to_str().ok_or_else(bad_request)?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(bad_request());
    }
    text.parse().map_err(|_| bad_request())
}

fn parse_locator(args: &[OsString]) -> io::Result<TerminalLocator> {
    let [owner, directory, reply, device, inode] = args else {
        return Err(bad_request());
    };
    Ok(TerminalLocator {
        owner: field(owner)?,
        directory: field(directory)?,
        reply: field(reply)?,
        expected: FileIdentity {
            device: field(device)?,
            inode: field(inode)?,
        },
    })
}

fn configured_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|path| executable(path))
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// Recognize before GTK/display initialization so this can be a terminal's child.
pub(super) fn run_if_child() -> Option<glib::ExitCode> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next()?.as_os_str() != CHILD_FLAG {
        return None;
    }
    let args: Vec<_> = arguments.take(6).collect();
    let result = (|| -> io::Result<()> {
        let setup = TerminalChildSetup::reopen(parse_locator(&args)?)?;
        // Replace the helper itself: no wrapper keeps handling terminal
        // signals or stands between the interactive shell and job control.
        setup.prepared()?;
        let error = Command::new(configured_shell())
            .current_dir(setup.process_cwd())
            .exec();
        setup.reject_exec();
        Err(error)
    })();
    Some(match result {
        Ok(()) => glib::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("JWM Files could not enter the selected terminal directory: {error}");
            glib::ExitCode::FAILURE
        }
    })
}

pub(super) fn install(window: &gtk4::ApplicationWindow, state: &Shared, status: &gtk4::Label) {
    let action = gio::SimpleAction::new("folder-terminal", None);
    let state = Rc::downgrade(state);
    let status = status.downgrade();
    let pending = Rc::new(Cell::new(false));
    action.connect_activate(move |_, _| {
        let Some((state, status)) = state.upgrade().zip(status.upgrade()) else {
            return;
        };
        let (path, identity, tab) = {
            let browser = state.borrow();
            if browser.pending.is_some() || browser.dialog_open || pending.get() {
                return;
            }
            let Some(identity) = browser.parent.as_ref().map(|parent| parent.identity()) else {
                return;
            };
            (browser.path().clone(), identity, browser.tabs.active_id())
        };
        status.set_text("正在打开此目录的终端…");
        let (send, receive) = futures_channel::oneshot::channel();
        let chosen = path.clone();
        pending.set(true);
        let worker = std::thread::Builder::new()
            .name("jwm-files-terminal-launch".into())
            .spawn(move || {
                let launch = (|| {
                    let search = std::env::var_os("PATH").unwrap_or_default();
                    let launcher = pick_launcher(&search)?;
                    let pin = TerminalHandoff::new(&chosen, identity)?;
                    let child = spawn_launcher(&launcher, &std::env::current_exe()?, &pin)?;
                    Ok::<_, io::Error>((pin, child))
                })();
                match launch {
                    Ok((pin, mut child)) => {
                        let result = pin.wait_for_child(Duration::from_secs(8));
                        let _ = send.send(result);
                        let _ = child.wait();
                    }
                    Err(error) => {
                        let _ = send.send(Err(error));
                    }
                }
            });
        if let Err(error) = worker {
            pending.set(false);
            status.set_text(&format!("无法打开终端：{error}"));
            return;
        }
        let (state, status) = (Rc::downgrade(&state), status.downgrade());
        let pending = pending.clone();
        glib::spawn_future_local(async move {
            let result = receive.await;
            pending.set(false);
            let Some((state, status)) = state.upgrade().zip(status.upgrade()) else {
                return;
            };
            let browser = state.borrow();
            let still_here = browser.tabs.active_id() == tab && browser.path() == &path;
            if browser.pending.is_some() || !still_here {
                return;
            }
            match result {
                Ok(Ok(())) => status.set_text("已在此目录打开终端。"),
                Ok(Err(error)) => status.set_text(&format!("无法打开终端：{error}")),
                Err(_) => status.set_text("终端启动程序未返回结果。"),
            }
        });
    });
    window.add_action(&action);
}
