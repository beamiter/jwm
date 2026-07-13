from pathlib import Path
import re

path = Path("tools/jwm_tool.rs")
text = path.read_text()


def replace_once(old: str, new: str) -> None:
    global text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected exactly one match, found {count}: {old[:120]!r}")
    text = text.replace(old, new, 1)


def sub_once(pattern: str, replacement: str) -> None:
    global text
    text, count = re.subn(pattern, replacement, text, count=1, flags=re.S)
    if count != 1:
        raise SystemExit(f"expected exactly one regex match, found {count}: {pattern}")


replace_once(
    "use std::time::{Duration, Instant};\n",
    "use std::time::{Duration, Instant};\n\n"
    "const DAEMON_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);\n"
    "const RESPONSE_LOCK_TIMEOUT: Duration = Duration::from_secs(12);\n"
    "const RESPONSE_LOCK_STALE_AFTER: Duration = Duration::from_secs(30);\n",
)

response_lock_code = r'''

#[derive(Debug)]
struct ResponseLock {
    path: PathBuf,
}

impl Drop for ResponseLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn response_lock_path(control_pipe: &Path) -> PathBuf {
    response_path(control_pipe).with_extension("lock")
}

fn acquire_response_lock(control_pipe: &Path, timeout: Duration) -> io::Result<ResponseLock> {
    let path = response_lock_path(control_pipe);
    let deadline = Instant::now() + timeout;

    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())?;
                file.flush()?;
                return Ok(ResponseLock { path });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let owner_gone = fs::read_to_string(&path)
                    .ok()
                    .and_then(|value| value.trim().parse::<i32>().ok())
                    .is_some_and(|pid| !process_exists(pid));
                let stale = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age >= RESPONSE_LOCK_STALE_AFTER);
                if owner_gone || stale {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "another jwm-tool command is still waiting for the daemon response",
                    ));
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    }
}
'''
sub_once(
    r'(fn response_path\(control_pipe: &Path\) -> PathBuf \{.*?\n\})\n',
    r'\1' + response_lock_code + "\n",
)

sub_once(
    r'fn home_dir\(\) -> PathBuf \{.*?\n\}',
    '''fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_else(runtime_dir)
}''',
)

sub_once(
    r'fn log_line\(msg: &str\) \{.*?\n\}',
    '''fn log_line(msg: &str) {
    let timestamp = now_ts();
    let line = format!("[{timestamp}] {msg}");
    let directory = log_dir();
    let path = directory.join("jwm_daemon.log");

    if let Err(error) = fs::create_dir_all(&directory) {
        eprintln!(
            "[{timestamp}] 无法创建日志目录 {}: {error}",
            directory.display()
        );
    } else {
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "{line}");
                let _ = file.flush();
            }
            Err(error) => {
                eprintln!(
                    "[{timestamp}] 无法打开日志文件 {}: {error}",
                    path.display()
                );
            }
        }
    }
    println!("{line}");
}''',
)

replace_once(
    '''    fn restart(&mut self) {
        log_line("重启JWM...");
        self.stop();
        // stop() already waited for exit, no extra sleep needed
        let _ = self.start();
    }
''',
    '''    fn restart(&mut self) -> io::Result<()> {
        log_line("重启JWM...");
        self.stop();
        // stop() already waited for exit, no extra sleep needed
        self.start()
    }
''',
)

replace_once(
    '''fn write_response(resp_file: &Path, s: &str) {
    let tmp = resp_file.with_extension("tmp");
    if fs::write(&tmp, s).is_ok() {
        let _ = fs::rename(&tmp, resp_file);
    }
}
''',
    '''fn write_response(resp_file: &Path, s: &str) {
    let tmp = resp_file.with_extension("tmp");
    if fs::write(&tmp, s).is_ok() {
        let _ = fs::rename(&tmp, resp_file);
    }
}

fn daemon_command_response(action: &str, result: io::Result<()>) -> String {
    match result {
        Ok(()) => format!("{action}_done"),
        Err(error) => format!("{action}_error: {error}"),
    }
}

fn validate_daemon_response(response: &str) -> io::Result<()> {
    if response.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon returned an empty response",
        ));
    }
    if response == "unknown_command" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "daemon rejected an unknown command",
        ));
    }
    if let Some((action, detail)) = response.split_once("_error:") {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("{action}: {}", detail.trim()),
        ));
    }
    Ok(())
}
''',
)

replace_once(
    '''    let _ = fs::remove_file(resp.with_extension("tmp"));
    let _ = fs::remove_file(pidfile_path());
''',
    '''    let _ = fs::remove_file(resp.with_extension("tmp"));
    let _ = fs::remove_file(response_lock_path(control_pipe));
    let _ = fs::remove_file(pidfile_path());
''',
)

replace_once(
    '''    let mut mgr = JwmManager::new(jwm_binary, backend);
    let _ = mgr.start();

    log_line("开始主循环，监听命令...");
''',
    '''    let mut mgr = JwmManager::new(jwm_binary, backend);
    if let Err(error) = mgr.start() {
        cleanup_resources(&control_pipe);
        return Err(error);
    }

    log_line("开始主循环，监听命令...");
''',
)

replace_once(
    '''                        "restart" => {
                            mgr.restart();
                            write_response(&resp_path, "restart_done");
                        }
                        "stop" => {
                            mgr.stop();
                            write_response(&resp_path, "stop_done");
                        }
                        "start" => {
                            let _ = mgr.start();
                            write_response(&resp_path, "start_done");
                        }
''',
    '''                        "restart" => {
                            let response = daemon_command_response("restart", mgr.restart());
                            write_response(&resp_path, &response);
                        }
                        "stop" => {
                            mgr.stop();
                            write_response(&resp_path, "stop_done");
                        }
                        "start" => {
                            let response = daemon_command_response("start", mgr.start());
                            write_response(&resp_path, &response);
                        }
''',
)

sub_once(
    r'fn send_command\(cmd: &str\) -> io::Result<\(\)> \{.*?\n\}\n\nfn check_daemon',
    '''fn send_command(cmd: &str) -> io::Result<()> {
    let pipe = find_control_pipe().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "未找到JWM守护进程或控制管道；请先启动 jwm-tool daemon",
        )
    })?;
    let _response_lock = acquire_response_lock(&pipe, RESPONSE_LOCK_TIMEOUT)?;
    let resp_path = response_path(&pipe);
    let tmp_resp_path = resp_path.with_extension("tmp");
    let _ = fs::remove_file(&resp_path);
    let _ = fs::remove_file(&tmp_resp_path);

    println!("发送命令: {cmd}");
    let data = format!("{cmd}\\n");
    let mut last_error: Option<io::Error> = None;
    for _ in 0..10 {
        match fs::write(&pipe, &data) {
            Ok(_) => {
                last_error = None;
                break;
            }
            Err(error)
                if error.kind() == io::ErrorKind::BrokenPipe
                    || error.raw_os_error() == Some(32) =>
            {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
    if let Some(error) = last_error {
        return Err(error);
    }

    let deadline = Instant::now() + DAEMON_RESPONSE_TIMEOUT;
    while Instant::now() < deadline {
        if resp_path.exists() {
            let content = fs::read_to_string(&resp_path)?;
            let _ = fs::remove_file(&resp_path);
            let response = content.trim();
            println!("响应: {response}");
            return validate_daemon_response(response);
        }
        thread::sleep(Duration::from_millis(50));
    }

    let _ = fs::remove_file(&resp_path);
    let _ = fs::remove_file(&tmp_resp_path);
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "命令 {cmd:?} 已发送，但在 {} 秒内未收到守护进程响应",
            DAEMON_RESPONSE_TIMEOUT.as_secs()
        ),
    ))
}

fn check_daemon''',
)

replace_once(
    '''    println!("重启JWM...");
    let _ = send_command("restart");
    println!("JWM编译并重启完成！");
''',
    '''    println!("重启JWM...");
    send_command("restart")?;
    println!("JWM编译并重启完成！");
''',
)

replace_once(
    '''fn install_jwm(jwm_dir: &str) -> io::Result<()> {
''',
    '''fn session_install_targets(jwm_dir: &Path) -> [(PathBuf, &'static str); 3] {
    [
        (
            jwm_dir.join("jwm-x11rb.desktop"),
            "/usr/share/xsessions/",
        ),
        (jwm_dir.join("jwm-xcb.desktop"), "/usr/share/xsessions/"),
        (
            jwm_dir.join("jwm-wayland.desktop"),
            "/usr/share/wayland-sessions/",
        ),
    ]
}

fn install_jwm(jwm_dir: &str) -> io::Result<()> {
''',
)

replace_once(
    '''    sudo_install(&files_to_check[0].1, "/usr/local/bin/")?;
    sudo_install(&files_to_check[1].1, "/usr/local/bin/")?;
    sudo_install(&files_to_check[2].1, "/usr/share/xsessions/")?;
    sudo_install(&files_to_check[3].1, "/usr/share/wayland-sessions/")?;

    println!("安装完成");
''',
    '''    sudo_install(&files_to_check[0].1, "/usr/local/bin/")?;
    sudo_install(&files_to_check[1].1, "/usr/local/bin/")?;
    for (source, destination) in session_install_targets(jwm_dir) {
        sudo_install(&source, destination)?;
    }

    println!("安装完成");
''',
)

sub_once(
    r'fn run_ipc_msg\(name: &str, args_str: &str, subscribe: Option<&str>, raw: bool\) -> io::Result<\(\)> \{.*?\n\}\n\n#\[cfg\(test\)\]',
    '''fn parse_msg_args(args_str: &str) -> io::Result<serde_json::Value> {
    serde_json::from_str(args_str).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("--args 不是有效 JSON: {error}"),
        )
    })
}

fn parse_subscription_topics(topics: &str) -> io::Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut parsed = Vec::new();
    for topic in topics.split(',').map(str::trim).filter(|topic| !topic.is_empty()) {
        if seen.insert(topic.to_string()) {
            parsed.push(topic.to_string());
        }
    }
    if parsed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--subscribe 至少需要一个非空主题",
        ));
    }
    Ok(parsed)
}

fn validate_ipc_response(line: &str) -> io::Result<()> {
    let response: serde_json::Value = serde_json::from_str(line).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("JWM 返回了无效 JSON: {error}"),
        )
    })?;
    if response.get("success").and_then(|value| value.as_bool()) == Some(false) {
        let message = response
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("IPC request failed");
        return Err(io::Error::new(io::ErrorKind::Other, message.to_string()));
    }
    Ok(())
}

fn run_ipc_msg(name: &str, args_str: &str, subscribe: Option<&str>, raw: bool) -> io::Result<()> {
    let sock_path = ipc_socket_path();
    if !sock_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("IPC socket not found at {}; is JWM running?", sock_path.display()),
        ));
    }

    let mut stream = UnixStream::connect(&sock_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    if let Some(topics) = subscribe {
        let topic_list = parse_subscription_topics(topics)?;
        let msg = serde_json::json!({ "subscribe": topic_list });
        let mut line = serde_json::to_string(&msg)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        line.push('\\n');
        stream.write_all(line.as_bytes())?;

        let resp = read_ipc_line(&mut stream)?;
        validate_ipc_response(resp.trim())?;
        if !raw {
            eprintln!("Subscribed: {}", resp.trim());
        }

        stream.set_read_timeout(None)?;
        loop {
            match read_ipc_line(&mut stream) {
                Ok(line) => {
                    if raw {
                        println!("{}", line.trim());
                    } else {
                        match serde_json::from_str::<serde_json::Value>(line.trim()) {
                            Ok(value) => println!(
                                "{}",
                                serde_json::to_string_pretty(&value).unwrap_or(line)
                            ),
                            Err(_) => println!("{}", line.trim()),
                        }
                    }
                }
                Err(error) => {
                    eprintln!("Connection closed: {error}");
                    break;
                }
            }
        }
        return Ok(());
    }

    let args = parse_msg_args(args_str)?;
    let msg = if name.starts_with("get_") {
        serde_json::json!({ "query": name, "args": args })
    } else {
        serde_json::json!({ "command": name, "args": args })
    };

    let mut line = serde_json::to_string(&msg)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    line.push('\\n');
    stream.write_all(line.as_bytes())?;

    let resp = read_ipc_line(&mut stream)?;
    let validation = validate_ipc_response(resp.trim());
    if raw {
        print!("{resp}");
    } else {
        match serde_json::from_str::<serde_json::Value>(resp.trim()) {
            Ok(value) => println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or(resp)
            ),
            Err(_) => print!("{resp}"),
        }
    }
    validation
}

#[cfg(test)]''',
)

replace_once(
    '''    use super::{
        SmokeTarget, smoke_artifacts_json, smoke_ci_profile_json, smoke_manual_kms_checklist_json,
        smoke_target_json, split_path_list,
    };
''',
    '''    use super::{
        SmokeTarget, acquire_response_lock, daemon_command_response, parse_msg_args,
        parse_subscription_topics, response_lock_path, session_install_targets,
        smoke_artifacts_json, smoke_ci_profile_json, smoke_manual_kms_checklist_json,
        smoke_target_json, split_path_list, validate_daemon_response, validate_ipc_response,
    };
''',
)

tests = r'''

    #[test]
    fn invalid_msg_json_is_rejected_instead_of_becoming_null() {
        let error = parse_msg_args("{not-json").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn subscription_topics_drop_empty_and_duplicate_entries() {
        assert_eq!(
            parse_subscription_topics("window, ,window,tag").unwrap(),
            ["window", "tag"]
        );
        assert!(parse_subscription_topics(" , ").is_err());
    }

    #[test]
    fn ipc_failure_response_becomes_a_cli_error() {
        assert!(validate_ipc_response(r#"{"success":true}"#).is_ok());
        let error = validate_ipc_response(
            r#"{"success":false,"error":"unknown command: nope"}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown command"));
    }

    #[test]
    fn daemon_start_failures_are_not_reported_as_success() {
        let response = daemon_command_response(
            "start",
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "binary missing",
            )),
        );
        assert_eq!(response, "start_error: binary missing");
        assert!(validate_daemon_response(&response).is_err());
        assert!(validate_daemon_response("start_done").is_ok());
    }

    #[test]
    fn response_lock_serializes_control_clients() {
        let pipe = std::env::temp_dir().join(format!(
            "jwm-tool-lock-test-{}",
            std::process::id()
        ));
        let lock_path = response_lock_path(&pipe);
        let _ = std::fs::remove_file(&lock_path);

        let first = acquire_response_lock(&pipe, std::time::Duration::from_millis(50)).unwrap();
        assert!(
            acquire_response_lock(&pipe, std::time::Duration::from_millis(20)).is_err()
        );
        drop(first);
        let second = acquire_response_lock(&pipe, std::time::Duration::from_millis(50)).unwrap();
        drop(second);
        assert!(!lock_path.exists());
    }

    #[test]
    fn session_install_plan_uses_the_correct_display_manager_directories() {
        let plan = session_install_targets(std::path::Path::new("/src/jwm"));
        assert!(plan[0].0.ends_with("jwm-x11rb.desktop"));
        assert_eq!(plan[0].1, "/usr/share/xsessions/");
        assert!(plan[1].0.ends_with("jwm-xcb.desktop"));
        assert_eq!(plan[1].1, "/usr/share/xsessions/");
        assert!(plan[2].0.ends_with("jwm-wayland.desktop"));
        assert_eq!(plan[2].1, "/usr/share/wayland-sessions/");
    }
'''
marker = "\n}\n\nfn send_ipc_query(name: &str)"
index = text.find(marker, text.find("#[cfg(test)]"))
if index < 0:
    raise SystemExit("could not locate jwm-tool test module end")
text = text[:index] + tests + text[index:]

path.write_text(text)
