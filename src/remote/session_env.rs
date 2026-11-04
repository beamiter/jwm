//! Find the X display of the user's running JWM session.
//!
//! A host started over SSH has no `DISPLAY`, and the right value is not
//! guessable: a gdm session on the second VT is `:1` with its cookie under
//! `/run/user/<uid>/gdm/Xauthority`. The running window manager already has
//! both in its environment, and the same user may read it.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Process name of the window manager whose session is shared.
const SESSION_PROCESS: &str = "jwm";

/// Display settings of one running JWM session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDisplay {
    pub display: String,
    pub xauthority: Option<OsString>,
    pub pid: u32,
}

/// The current user's JWM X11 session; the longest-running one when several
/// exist, since a nested test instance is always started later.
pub fn discover() -> io::Result<SessionDisplay> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    discover_in(Path::new("/proc"), uid)
}

fn discover_in(proc_root: &Path, uid: u32) -> io::Result<SessionDisplay> {
    let mut found: Vec<(u64, SessionDisplay)> = Vec::new();
    for entry in fs::read_dir(proc_root)? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let directory = entry.path();
        // Processes come and go during the scan; any unreadable one is skipped.
        let Ok(metadata) = fs::metadata(&directory) else {
            continue;
        };
        if metadata.uid() != uid {
            continue;
        }
        let Ok(comm) = fs::read_to_string(directory.join("comm")) else {
            continue;
        };
        if comm.trim_end() != SESSION_PROCESS {
            continue;
        }
        let Ok(environ) = fs::read(directory.join("environ")) else {
            continue;
        };
        let Some((display, xauthority)) = x11_session_from_environ(&environ) else {
            continue;
        };
        let started = fs::read_to_string(directory.join("stat"))
            .ok()
            .and_then(|stat| start_time(&stat))
            .unwrap_or(u64::MAX);
        found.push((
            started,
            SessionDisplay {
                display,
                xauthority,
                pid,
            },
        ));
    }
    found.sort_by_key(|(started, session)| (*started, session.pid));
    found
        .into_iter()
        .next()
        .map(|(_, session)| session)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no running JWM X11 session of this user was found; pass --display",
            )
        })
}

/// `DISPLAY` and `XAUTHORITY` from a NUL-separated environment block, or
/// `None` for a session that is not an X11 one.
fn x11_session_from_environ(environ: &[u8]) -> Option<(String, Option<OsString>)> {
    let mut display = None;
    let mut xauthority = None;
    for variable in environ.split(|&byte| byte == 0) {
        if let Some(value) = variable.strip_prefix(b"DISPLAY=") {
            display = std::str::from_utf8(value).ok().map(str::to_owned);
        } else if let Some(value) = variable.strip_prefix(b"XAUTHORITY=") {
            xauthority = Some(OsString::from_vec(value.to_vec()));
        } else if let Some(value) = variable.strip_prefix(b"XDG_SESSION_TYPE=")
            && value.eq_ignore_ascii_case(b"wayland")
        {
            // Its DISPLAY, if any, is Xwayland, not the desktop JWM draws.
            return None;
        }
    }
    let display = display.filter(|value| !value.trim().is_empty())?;
    Some((display, xauthority.filter(|value| !value.is_empty())))
}

/// Field 22 of `/proc/<pid>/stat`, read after the parenthesised name, which
/// may itself contain spaces and parentheses.
fn start_time(stat: &str) -> Option<u64> {
    let (_, after_name) = stat.rsplit_once(')')?;
    after_name.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_proc() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "jwm-remote-proc-test-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    fn process(root: &Path, pid: u32, comm: &str, started: u64, environ: &[&str]) {
        let directory = root.join(pid.to_string());
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("comm"), format!("{comm}\n")).unwrap();
        fs::write(
            directory.join("stat"),
            format!("{pid} ({comm}) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 {started} 0"),
        )
        .unwrap();
        let mut block = Vec::new();
        for variable in environ {
            block.extend_from_slice(variable.as_bytes());
            block.push(0);
        }
        fs::write(directory.join("environ"), block).unwrap();
    }

    fn current_uid() -> u32 {
        unsafe { libc::geteuid() }
    }

    #[test]
    fn the_longest_running_jwm_session_is_shared() {
        let root = fake_proc();
        process(&root, 900, "bash", 10, &["DISPLAY=:7"]);
        // A nested test instance started later on another display.
        process(&root, 4400, "jwm", 900, &["DISPLAY=:91"]);
        process(
            &root,
            4353,
            "jwm",
            300,
            &[
                "HOME=/home/u",
                "DISPLAY=:1",
                "XAUTHORITY=/run/user/1000/gdm/Xauthority",
            ],
        );
        fs::write(root.join("self"), "not a pid").unwrap();
        let session = discover_in(&root, current_uid()).unwrap();
        assert_eq!(
            session,
            SessionDisplay {
                display: ":1".into(),
                xauthority: Some("/run/user/1000/gdm/Xauthority".into()),
                pid: 4353,
            }
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn other_users_and_wayland_sessions_are_not_shared() {
        let root = fake_proc();
        process(
            &root,
            10,
            "jwm",
            1,
            &["DISPLAY=:0", "XDG_SESSION_TYPE=wayland"],
        );
        process(&root, 11, "jwm", 2, &["XAUTHORITY=/x"]);
        let error = discover_in(&root, current_uid()).unwrap_err();
        assert!(error.to_string().contains("pass --display"), "{error}");
        // Only the owner's processes count.
        process(&root, 12, "jwm", 3, &["DISPLAY=:2"]);
        assert!(discover_in(&root, current_uid().wrapping_add(1)).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn start_time_skips_a_name_with_spaces_and_parentheses() {
        let stat = "77 (jwm (test) x) S 1 77 77 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 4242 0";
        assert_eq!(start_time(stat), Some(4242));
        assert_eq!(start_time("garbage"), None);
    }
}
