//! The real daemon executable, process lock and private control socket, with a
//! local JWM stand-in. No credential, network or audio capture is used.
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
struct Fixture {
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
    daemon: Option<Child>,
}
impl Fixture {
    fn new() -> Self {
        let base = if std::path::Path::new("/dev/shm").is_dir() {
            std::path::PathBuf::from("/dev/shm")
        } else {
            std::env::temp_dir()
        };
        let dir = base.join(format!(
            "eva-daemon-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let listener = UnixListener::bind(dir.join("jwm-ipc.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let server = thread::spawn(move || {
            while !s.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut client, _)) => {
                        thread::spawn(move || {
                            client
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            let mut line = String::new();
                            if BufReader::new(client.try_clone().unwrap())
                                .read_line(&mut line)
                                .is_err()
                            {
                                return;
                            }
                            let Ok(request) = serde_json::from_str::<Value>(&line) else {
                                return;
                            };
                            if request.get("subscribe").is_some() {
                                let _ = writeln!(client, "{}", json!({"success":true,"data":null}));
                                thread::sleep(Duration::from_millis(500));
                            } else {
                                let _ = writeln!(
                                    client,
                                    "{}",
                                    json!({"success":true,"data":{"active":false,"assistant_permit":null,"state":"idle"}})
                                );
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            dir,
            stop,
            server: Some(server),
            daemon: None,
        }
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_jwm-eva"));
        cmd.env("XDG_RUNTIME_DIR", &self.dir)
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("JWM_SOCKET", self.dir.join("jwm-ipc.sock"))
            .env_remove("OPENAI_API_KEY")
            .env_remove("NOTIFY_SOCKET")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        cmd
    }
    fn status(&self) -> Option<Value> {
        let mut socket = UnixStream::connect(self.dir.join("jwm-eva/control.sock")).ok()?;
        socket.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        writeln!(socket, "{{\"command\":\"status\"}}").ok()?;
        let mut text = String::new();
        BufReader::new(socket).read_line(&mut text).ok()?;
        serde_json::from_str(&text).ok()
    }
    fn start(&mut self) {
        self.daemon = Some(self.command().arg("daemon").spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.status().is_none() {
            assert!(Instant::now() < deadline, "daemon failed to become ready");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = self.daemon.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.server.take() {
            let _ = t.join();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}
#[test]
fn idle_single_instance_and_control_permissions_are_real() {
    let mut f = Fixture::new();
    f.start();
    let status = f.status().unwrap();
    assert_eq!(status["data"]["state"], "idle");
    assert_eq!(status["data"]["microphone_active"], false);
    assert!(status["assistant_permit"].is_null());
    let metadata = fs::symlink_metadata(f.dir.join("jwm-eva/control.sock")).unwrap();
    assert_eq!(metadata.mode() & 0o777, 0o600);
    let rejected = f.command().arg("daemon").output().unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("already running"));
    assert_eq!(f.status().unwrap()["pid"], status["pid"]);
    assert_eq!(status["configuration_id"].as_str().unwrap().len(), 64);
    let selected = f.dir.join("different.toml");
    fs::write(&selected, "[interaction]\nfeedback='text'\n").unwrap();
    let mismatch = f
        .command()
        .args(["--config"])
        .arg(&selected)
        .args(["dry-run", "--text", "切到第三个tag"])
        .output()
        .unwrap();
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("configuration differs"));
    fs::write(&selected, "this is invalid TOML").unwrap();
    for operation in ["status", "cancel"] {
        let control = f
            .command()
            .arg("--config")
            .arg(&selected)
            .arg(operation)
            .output()
            .unwrap();
        assert!(
            control.status.success(),
            "{operation} remains available with malformed config: {}",
            String::from_utf8_lossy(&control.stderr)
        );
    }
    let daemon_pid = status["pid"].as_u64().unwrap();
    let tasks = fs::read_dir(format!("/proc/{daemon_pid}/task")).unwrap();
    for task in tasks.flatten() {
        let name = fs::read_to_string(task.path().join("comm")).unwrap_or_default();
        assert!(
            !name.contains("eva-capture") && !name.contains("eva-playback"),
            "Idle must own no audio worker"
        );
    }
    if let Some(report) = std::env::var_os("JWM_EVA_IDLE_REPORT") {
        fn ticks(pid: u64) -> u64 {
            let text = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let fields = text[text.rfind(')').unwrap() + 2..]
                .split_whitespace()
                .collect::<Vec<_>>();
            fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
        }
        let started = Instant::now();
        let initial = ticks(daemon_pid);
        thread::sleep(Duration::from_secs(2));
        let seconds = started.elapsed().as_secs_f64();
        let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        let cpu = (ticks(daemon_pid) - initial) as f64 / ticks_per_second;
        let proc_status = fs::read_to_string(format!("/proc/{daemon_pid}/status")).unwrap();
        let field = |name: &str| {
            proc_status
                .lines()
                .find(|line| line.starts_with(name))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|number| number.parse::<u64>().ok())
        };
        let sha = Command::new("sha256sum")
            .arg(env!("CARGO_BIN_EXE_jwm-eva"))
            .output()
            .unwrap();
        let value = json!({"environment":"isolated real daemon with local Unix IPC stand-in; no cloud, inference or audio","profile":"debug","binary_sha256":String::from_utf8_lossy(&sha.stdout).split_whitespace().next(),"measurement_seconds":seconds,"cpu_seconds":cpu,"cpu_percent_one_core":100.0*cpu/seconds,"cpu_timer_granularity_seconds":1.0/ticks_per_second,"rss_kib":field("VmRSS:"),"threads":field("Threads:"),"microphone_active":f.status().unwrap()["data"]["microphone_active"],"source_epoch_seconds":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()});
        let path = std::path::PathBuf::from(report);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    }
    let child = f.daemon.as_mut().unwrap();
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !f.dir.join("jwm-eva/control.sock").exists(),
        "graceful shutdown removes only its own endpoint"
    );
}
