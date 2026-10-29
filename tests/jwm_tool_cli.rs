//! End-to-end checks of `jwm-tool` verbs whose contract is the exit status.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};

/// A private runtime directory, removed on drop.
struct RuntimeDir(PathBuf);

impl RuntimeDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("jwm-tool-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).expect("create runtime directory");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("restrict runtime directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for RuntimeDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Kills and reaps the stand-in daemon even when an assertion fails.
struct StandIn(Child);

impl Drop for StandIn {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn daemon_check(runtime: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_jwm-tool"))
        .arg("daemon-check")
        .env("XDG_RUNTIME_DIR", runtime)
        .output()
        .expect("run jwm-tool daemon-check")
}

/// `starttime` from `/proc/<pid>/stat`: field 22, counted after the
/// parenthesised command name, which may itself contain spaces.
fn process_start_time(pid: u32) -> u64 {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).expect("read process stat");
    let after_name = &stat[stat.rfind(')').expect("stat command name") + 1..];
    after_name
        .split_whitespace()
        .nth(19)
        .and_then(|field| field.parse().ok())
        .expect("stat start time")
}

#[test]
fn daemon_check_fails_when_no_daemon_is_running() {
    let runtime = RuntimeDir::new("absent");

    let absent = daemon_check(runtime.path());
    assert_eq!(
        absent.status.code(),
        Some(1),
        "stdout: {}",
        String::from_utf8_lossy(&absent.stdout)
    );
    assert!(String::from_utf8_lossy(&absent.stdout).contains("未运行"));

    // A live daemon identity and control pipe still report success.
    let stand_in = StandIn(
        Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn stand-in daemon"),
    );
    let pid = stand_in.0.id();
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id").expect("read boot id");
    fs::write(
        runtime.path().join("jwm_daemon.pid"),
        format!("v2 {pid} {} {}\n", process_start_time(pid), boot_id.trim()),
    )
    .expect("write daemon pidfile");
    nix::unistd::mkfifo(
        &runtime.path().join(format!("jwm_control_{pid}")),
        nix::sys::stat::Mode::from_bits_truncate(0o600),
    )
    .expect("create control pipe");

    let running = daemon_check(runtime.path());
    assert!(
        running.status.success(),
        "status {:?}, stdout: {}",
        running.status,
        String::from_utf8_lossy(&running.stdout)
    );
    assert!(String::from_utf8_lossy(&running.stdout).contains("正在运行"));
}

#[test]
fn daemon_retains_private_child_stderr_and_actual_exit_code() {
    let runtime = RuntimeDir::new("child-diagnostics");
    let home = runtime.path().join("home");
    fs::create_dir(&home).unwrap();
    let fixture = runtime.path().join("jwm-fixture");
    fs::write(
        &fixture,
        "#!/bin/sh\nprintf 'private startup failure\\n' >&2\nexit 37\n",
    )
    .unwrap();
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o755)).unwrap();
    let mut daemon = StandIn(
        Command::new(env!("CARGO_BIN_EXE_jwm-tool"))
            .args(["daemon", "--jwm-binary"])
            .arg(&fixture)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env("JWM_BACKEND", "x11rb")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "private daemon failed to finish"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert!(status.success());
    let diagnostic = runtime.path().join("jwm_child_stderr.log");
    assert_eq!(
        fs::read_to_string(&diagnostic).unwrap(),
        "private startup failure\n"
    );
    assert_eq!(
        fs::metadata(diagnostic).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let daemon_log = fs::read_to_string(home.join(".local/share/jwm/jwm_daemon.log")).unwrap();
    assert!(daemon_log.contains("exit_code=37"), "{daemon_log}");
}
