//! Opt-in controller for the separately installed TSX CLI. Not a sandbox.
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_GRACE: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(20);
const MAX_OUTPUT: usize = 16 * 1024;
const SOURCE: &str = include_str!("../../../scripts/jwm-tsx-preview.mjs");

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp(u64, u64, u64, i64, i64);
impl Stamp {
    fn read(path: &Path) -> Result<Self, String> {
        let m = fs::metadata(path).map_err(|e| e.to_string())?;
        Ok(Self(m.dev(), m.ino(), m.len(), m.ctime(), m.ctime_nsec()))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Checked {
    pub project: PathBuf,
    pub entry: PathBuf,
    pub vite: String,
}

pub(crate) enum Event {
    Checked(Checked),
    Failed(String),
    Stopped,
}

struct Worker {
    cancel: Arc<AtomicBool>,
    events: Receiver<Event>,
    done: Receiver<()>,
    join: thread::JoinHandle<()>,
}

#[derive(Default)]
pub(crate) struct Controller {
    worker: Option<Worker>,
}

impl Controller {
    pub fn is_busy(&self) -> bool {
        self.worker.is_some()
    }

    pub fn check(&mut self, project: &Path, entry: &Path) -> Result<(), String> {
        if self.is_busy() {
            return Err("已有 TSX 操作正在运行。请先取消或停止。".into());
        }
        let project = project.to_owned();
        let entry = entry.to_owned();
        self.worker = Some(worker(move |cancel, events| {
            let result = check(project, entry, cancel);
            if !cancel.load(Ordering::Relaxed) {
                let _ = events.send(match result {
                    Ok(checked) => Event::Checked(checked),
                    Err(error) => Event::Failed(error),
                });
            } else {
                let _ = events.send(Event::Stopped);
            }
        })?);
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(w) = &self.worker {
            w.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn poll(&mut self) -> Option<Event> {
        let w = self.worker.as_ref()?;
        match w.events.try_recv() {
            Err(TryRecvError::Empty) => None,
            Ok(event) => {
                let stopped = w.cancel.load(Ordering::Relaxed);
                self.worker = None;
                if stopped {
                    return Some(Event::Stopped);
                }
                Some(event)
            }
            Err(TryRecvError::Disconnected) => {
                self.worker = None;
                Some(Event::Failed("预检工作进程意外停止。".into()))
            }
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.stop();
        if let Some(w) = self.worker.take()
            && w.done
                .recv_timeout(STOP_GRACE + Duration::from_secs(1))
                .is_ok()
        {
            let _ = w.join.join();
        }
    }
}

fn worker(
    task: impl FnOnce(&AtomicBool, &SyncSender<Event>) + Send + 'static,
) -> Result<Worker, String> {
    let cancel = Arc::new(AtomicBool::new(false));
    let thread_cancel = Arc::clone(&cancel);
    // One ready + one final event, or one check completion.
    let (tx, events) = mpsc::sync_channel(2);
    let (done_tx, done) = mpsc::sync_channel(1);
    let join = thread::Builder::new()
        .name("jwm-tsx-preview".into())
        .spawn(move || {
            task(&thread_cancel, &tx);
            let _ = done_tx.send(());
        })
        .map_err(|e| e.to_string())?;
    Ok(Worker {
        cancel,
        events,
        done,
        join,
    })
}

fn launcher(project: &Path) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("找不到 HOME。")?;
    if !home.is_absolute() {
        return Err("HOME 必须是绝对路径。".into());
    }
    let home = home.canonicalize().map_err(|e| e.to_string())?;
    let bin = home.join(".local/bin/jwm-tsx-preview");
    let own = fs::metadata("/proc/self").map_err(|e| e.to_string())?.uid();
    for path in [
        home.clone(),
        home.join(".local"),
        home.join(".local/bin"),
        bin.clone(),
    ] {
        let m = fs::symlink_metadata(&path)
            .map_err(|_| "未找到可信的 ~/.local/bin/jwm-tsx-preview。请单独安装预览工具。")?;
        if m.file_type().is_symlink() || m.uid() != own || m.mode() & 0o022 != 0 {
            return Err("预览工具必须在当前用户拥有、其他用户不可写的安装路径。".into());
        }
    }
    let m = fs::metadata(&bin).map_err(|e| e.to_string())?;
    if bin.starts_with(project) || !m.is_file() || m.mode() & 0o111 == 0 {
        return Err("不使用项目内或不可执行的预览工具。".into());
    }
    Ok(bin)
}

fn command(project: &Path, entry: &Path) -> Result<Command, String> {
    let node = Path::new("/usr/bin/node");
    let actual = node
        .canonicalize()
        .map_err(|_| "TSX 预检需要系统安装的 /usr/bin/node。")?;
    if !actual.starts_with("/usr/bin") {
        return Err("Node 解释器必须位于受信任的系统目录。".into());
    }
    for path in actual.ancestors() {
        let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err("Node 系统路径不可由普通用户修改。".into());
        }
    }
    Ok(node_command(&actual, project, entry))
}

fn node_command(node: &Path, project: &Path, entry: &Path) -> Command {
    let mut cmd = Command::new(node);
    cmd.args(["--input-type=module", "-", "--check", "--project"])
        .arg(project)
        .arg("--component")
        .arg(entry)
        .current_dir("/")
        .env_clear()
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0);
    cmd
}

fn valid_display(s: &str) -> bool {
    !s.is_empty() && s.len() <= 4096 && !s.chars().any(|c| {
        c.is_control() || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    })
}

fn parse_check(bytes: &[u8], project: &Path, entry: &Path) -> Result<String, String> {
    let result: Value = serde_json::from_slice(bytes).map_err(|_| "TSX 预检结果无效。")?;
    let valid = result["execution"].as_bool() == Some(false)
        && result["mode"].as_str() == Some("component")
        && Path::new(
            result["project"]
                .as_str()
                .filter(|s| valid_display(s))
                .ok_or("项目路径不安全。")?,
        ) == project
        && Path::new(
            result["entry"]
                .as_str()
                .filter(|s| valid_display(s))
                .ok_or("入口路径不安全。")?,
        ) == entry;
    if !valid {
        return Err("预览工具返回的项目或入口与选择不一致。".into());
    }
    result["vite"]
        .as_str()
        .filter(|s| {
            valid_display(s) && s.len() <= 64 && (s.starts_with("7.") || s.starts_with("8."))
        })
        .map(str::to_owned)
        .ok_or_else(|| "预览工具返回的 Vite 版本不受支持。".into())
}

fn check_error(bytes: &[u8]) -> String {
    let parsed: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let message = parsed["error"].as_str().unwrap_or_default();
    for name in ["vite", "react", "react-dom"] {
        if message.starts_with(&format!("Missing local {name}.")) {
            return format!("项目缺少本地 {name}。请自行安装依赖后重试；Notes 不会安装。");
        }
    }
    if message.starts_with("Vite ") && message.ends_with("supports Vite 7/8.") {
        return "预检仅支持 Vite 7 和 8。".into();
    }
    "预检未通过。请检查项目目录、入口、Node 及本地 Vite/React 依赖。".into()
}

fn check(project: PathBuf, entry: PathBuf, cancel: &AtomicBool) -> Result<Checked, String> {
    let project = project.canonicalize().map_err(|e| e.to_string())?;
    let entry = entry.canonicalize().map_err(|e| e.to_string())?;
    if !entry.starts_with(&project)
        || !entry.is_file()
        || !project.is_dir()
        || !entry.to_str().is_some_and(valid_display)
        || !project.to_str().is_some_and(valid_display)
    {
        return Err("选择包含该已保存文件的项目目录。路径必须可安全显示。".into());
    }
    let launcher = launcher(&project)?;
    // Linux O_NONBLOCK makes a concurrent swap to a FIFO fail closed without
    // blocking the worker. Only regular files are read, and never executed.
    const O_NONBLOCK: i32 = 0o4000;
    let installed = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(&launcher)
        .map_err(|e| e.to_string())?;
    if !installed.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("安装的预览工具不是普通文件。".into());
    }
    let mut bytes = Vec::new();
    installed
        .take((SOURCE.len() + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes != SOURCE.as_bytes() {
        return Err("预览工具版本与 Notes 不匹配，请从同一版本重新安装。".into());
    }
    let entry_stamp = Stamp::read(&entry)?;
    let project_stamp = Stamp::read(&project)?;
    if cancel.load(Ordering::Relaxed) {
        return Err("已取消。".into());
    }
    let vite = inspect_metadata(command(&project, &entry)?, &project, &entry, cancel)?;
    if Stamp::read(&entry)? != entry_stamp
        || Stamp::read(&project)? != project_stamp
        || project.canonicalize().map_err(|e| e.to_string())? != project
        || entry.canonicalize().map_err(|e| e.to_string())? != entry
    {
        return Err("检查期间文件发生变化，请重新检查。".into());
    }
    Ok(Checked {
        project,
        entry,
        vite,
    })
}

fn inspect_metadata(
    mut cmd: Command,
    project: &Path,
    entry: &Path,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let mut child = cmd.spawn().map_err(|e| format!("无法运行TSX 预检：{e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    // Execute only this build's embedded source, never bytes from disk.
    let writer = thread::Builder::new()
        .name("jwm-tsx-check-input".into())
        .spawn(move || {
            let _ = stdin.write_all(SOURCE.as_bytes());
            let _ = stdin.write_all(
                b"\nmain(process.argv.slice(2)).catch(error => { console.log(JSON.stringify({error: String(error.message).slice(0, 512)})); process.exitCode = 1; });\n",
            );
        });
    if let Err(error) = writer {
        stop_child(&mut child);
        return Err(error.to_string());
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::sync_channel(1);
    let reader = thread::Builder::new()
        .name("jwm-tsx-check-output".into())
        .spawn(move || {
            let mut output = Vec::new();
            let result = stdout
                .take((MAX_OUTPUT + 1) as u64)
                .read_to_end(&mut output);
            let _ = tx.send(result.map(|_| output));
        });
    if let Err(error) = reader {
        stop_child(&mut child);
        return Err(error.to_string());
    }
    let deadline = Instant::now() + CHECK_TIMEOUT;
    let mut output = None;
    let success = loop {
        if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
            stop_child(&mut child);
            return Err("TSX 预检取消或超时。".into());
        }
        if output.is_none() {
            match rx.try_recv() {
                Ok(Ok(bytes)) if bytes.len() <= MAX_OUTPUT => output = Some(bytes),
                Err(TryRecvError::Empty) => (),
                _ => {
                    stop_child(&mut child);
                    return Err("TSX 预检输出无效或过大。".into());
                }
            }
        }
        match child.try_wait() {
            Ok(Some(status)) if output.is_some() => break status.success(),
            Ok(_) => {}
            Err(error) => {
                stop_child(&mut child);
                return Err(error.to_string());
            }
        }
        thread::sleep(TICK);
    };
    let output = output.unwrap();
    if !success {
        return Err(check_error(&output));
    }
    parse_check(&output, project, entry)
}

// Do not signal a pid after try_wait has reaped it. While unreaped it cannot be
// reused. Signal only this worker's newly created process group, never a service
// discovered by port, executable name, or project.
fn stop_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let group = format!("-{}", child.id());
    let signal = |name: &str| {
        Command::new("/bin/kill")
            .args([name, "--", &group])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if signal("-TERM") {
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(TICK);
        }
    }
    if !signal("-KILL") {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_wrong_check_scope_and_unsafe_display_paths() {
        let expected = serde_json::json!({"project":"/fixture","entry":"/fixture/App.tsx","vite":"8.3.8","mode":"component","execution":false});
        let parse = |v: &Value| {
            parse_check(
                &serde_json::to_vec(v).unwrap(),
                Path::new("/fixture"),
                Path::new("/fixture/App.tsx"),
            )
        };
        assert_eq!(parse(&expected).unwrap(), "8.3.8");
        for (name, value) in [
            ("project", Value::from("/elsewhere")),
            ("entry", Value::from("/fixture/Other.tsx")),
            ("vite", Value::from("6.0.0")),
            ("mode", Value::from("project")),
            ("execution", Value::from(true)),
        ] {
            let mut bad = expected.clone();
            bad[name] = value;
            assert!(parse(&bad).is_err(), "{name}");
        }
        assert!(!valid_display("/foo\nStart"));
        assert!(!valid_display("/foo\u{202e}tsx"));
    }

    #[test]
    fn metadata_command_uses_only_read_only_arguments_and_sanitized_environment() {
        let cmd = node_command(
            Path::new("/usr/bin/node"),
            Path::new("/fixture"),
            Path::new("/fixture/App.tsx"),
        );
        assert_eq!(cmd.get_program(), "/usr/bin/node");
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/")));
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            [
                "--input-type=module",
                "-",
                "--check",
                "--project",
                "/fixture",
                "--component",
                "/fixture/App.tsx"
            ],
        );
        assert_eq!(
            cmd.get_envs().collect::<Vec<_>>(),
            [(
                std::ffi::OsStr::new("LANG"),
                Some(std::ffi::OsStr::new("C.UTF-8"))
            )]
        );
    }

    fn check_stalled_child_is_reaped(cancel_early: bool) {
        let root = std::env::temp_dir().join(format!(
            "jwm-tsx-reap-test-{}-{cancel_early}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let pid_file = root.join("pid");
        let mut cmd = Command::new("/bin/sh");
        // Print only the fixture PID, then replace this same process with sleep.
        // No fixture path is interpolated into shell source.
        cmd.args(["-c", r#"echo "$$" > "$1"; exec /bin/sleep 60"#, "fixture"])
            .arg(&pid_file)
            .env_clear()
            .stdin(Stdio::piped())
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .process_group(0);
        let cancelled = Arc::new(AtomicBool::new(false));
        let notifier = cancel_early.then(|| {
            let flag = Arc::clone(&cancelled);
            let path = pid_file.clone();
            thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(2);
                while !path.exists() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(5));
                }
                flag.store(true, Ordering::Relaxed);
            })
        });
        let began = Instant::now();
        let result = inspect_metadata(
            cmd,
            Path::new("/fixture"),
            Path::new("/fixture/App.tsx"),
            &cancelled,
        );
        let elapsed = began.elapsed();
        if let Some(notifier) = notifier {
            notifier.join().unwrap();
        }
        let pid = fs::read_to_string(&pid_file).expect("fixture must have started");
        fs::remove_dir_all(&root).unwrap();
        let pid = pid.trim().parse::<u32>().unwrap();
        assert!(result.unwrap_err().contains("取消或超时"));
        assert!(
            !Path::new("/proc").join(pid.to_string()).exists(),
            "child was not reaped"
        );
        if cancel_early {
            assert!(
                elapsed < CHECK_TIMEOUT,
                "cancellation was not prompt: {elapsed:?}"
            );
        } else {
            assert!(
                elapsed >= CHECK_TIMEOUT
                    && elapsed < CHECK_TIMEOUT + STOP_GRACE + Duration::from_secs(1),
                "{elapsed:?}"
            );
        }
    }

    #[test]
    fn timed_out_metadata_child_is_reaped() {
        check_stalled_child_is_reaped(false);
    }

    #[test]
    fn cancelled_metadata_child_is_reaped() {
        check_stalled_child_is_reaped(true);
    }

    #[test]
    #[ignore = "run explicitly with JWM_TEST_NODE pointing to an installed Node interpreter"]
    fn embedded_check_real_node_does_not_import_project_code() {
        let node = std::env::var_os("JWM_TEST_NODE").expect("set JWM_TEST_NODE");
        let root = std::env::temp_dir().join(format!("jwm-tsx-check-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let root = root.canonicalize().unwrap();
        let entry = root.join("App.tsx");
        fs::write(
            root.join("package.json"),
            r#"{"scripts":{"dev":"exit 42"}}"#,
        )
        .unwrap();
        fs::write(&entry, "throw new Error('must not import')").unwrap();
        let run = || {
            inspect_metadata(
                node_command(Path::new(&node), &root, &entry),
                &root,
                &entry,
                &AtomicBool::new(false),
            )
        };
        assert!(run().unwrap_err().contains("vite"));
        for name in ["vite", "react", "react-dom"] {
            let dir = root.join("node_modules").join(name);
            fs::create_dir_all(dir.join("dist/node")).unwrap();
            fs::write(dir.join("package.json"), r#"{"version":"8.3.8"}"#).unwrap();
            fs::write(dir.join("dist/node/index.js"), "process.exit(42)").unwrap();
        }
        assert_eq!(run().unwrap(), "8.3.8");
    }

    #[test]
    fn stopped_worker_cannot_publish_stale_result() {
        let mut preview = Controller::default();
        preview.worker = Some(
            worker(|cancel, events| {
                while !cancel.load(Ordering::Relaxed) {
                    thread::sleep(TICK);
                }
                events
                    .send(Event::Checked(Checked {
                        project: "/fixture".into(),
                        entry: "/fixture/App.tsx".into(),
                        vite: "8.3.8".into(),
                    }))
                    .unwrap();
            })
            .unwrap(),
        );
        preview.stop();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match preview.poll() {
                Some(Event::Stopped) => break,
                None if Instant::now() < deadline => thread::sleep(TICK),
                _ => panic!("stale metadata or missing cancellation event"),
            }
        }
        assert!(!preview.is_busy());
    }
}
