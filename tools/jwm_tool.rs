use chrono::Local;
use clap::{Parser, Subcommand};
use glob::glob;

mod nested_smoke;
use nix::fcntl::{OFlag, open};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{Signal, kill};
use nix::sys::stat::Mode;
use nix::sys::wait::WaitStatus;
use nix::sys::wait::{WaitPidFlag, waitpid};
use nix::unistd::{Pid, mkfifo, read};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;
use std::collections::{HashSet, VecDeque};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const DAEMON_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_LOCK_TIMEOUT: Duration = Duration::from_secs(12);
const RESPONSE_LOCK_STALE_AFTER: Duration = Duration::from_secs(30);

// --- Runtime directory (XDG_RUNTIME_DIR) ---

fn runtime_dir() -> PathBuf {
    match jwm::ipc_server::validated_socket_path() {
        Ok(socket) => socket
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/dev/null/jwm-runtime-unavailable")),
        Err(error) => {
            eprintln!("Refusing unsafe JWM runtime directory: {error}");
            // A deterministic path below a regular device file makes later
            // filesystem operations fail closed instead of following an
            // attacker-controlled runtime directory.
            PathBuf::from("/dev/null/jwm-runtime-unavailable")
        }
    }
}

fn pidfile_path() -> PathBuf {
    runtime_dir().join("jwm_daemon.pid")
}

fn control_pipe_path(daemon_pid: i32) -> PathBuf {
    runtime_dir().join(format!("jwm_control_{}", daemon_pid))
}

fn response_path(control_pipe: &Path) -> PathBuf {
    let name = control_pipe
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    runtime_dir().join(format!("{}_response", name))
}

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

fn control_pipe_glob_pattern() -> String {
    format!("{}/jwm_control_*", runtime_dir().display())
}

// --- CLI ---

#[derive(Parser)]
#[command(
    name = "jwm-tool",
    version,
    about = "JWM 管理工具（单二进制多子命令）",
    long_about = "JWM 管理工具 — 通过 IPC 控制 JWM 窗口管理器。\n\
                  支持守护进程管理、窗口/标签/布局操作、事件订阅等功能。\n\
                  IPC 套接字位于 $XDG_RUNTIME_DIR/jwm-ipc.sock",
    after_help = "\x1b[1m示例:\x1b[0m\n  \
                  jwm-tool daemon                        # 启动守护进程\n  \
                  jwm-tool status                        # 查看守护进程状态\n  \
                  jwm-tool health --json                 # 查看 JWM 实时健康状态\n  \
                  jwm-tool capabilities                  # 发现 IPC 控制面\n  \
                  jwm-tool msg view --args '{\"tag\":2}'   # 切换到标签 2\n  \
                  jwm-tool msg get_windows               # 查询所有窗口\n  \
                  jwm-tool msg get_windows --raw          # 查询并输出原始 JSON\n  \
                  jwm-tool rebuild                       # 重新编译并重启 JWM"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 启动守护进程（管理 JWM 进程的生命周期）
    Daemon {
        /// 自定义JWM可执行文件路径（默认 /usr/local/bin/jwm，可用 env JWM_BINARY 覆盖）
        #[arg(long, env = "JWM_BINARY")]
        jwm_binary: Option<String>,
        /// 指定运行后端（可用 env JWM_BACKEND 覆盖）
        #[arg(long, env = "JWM_BACKEND")]
        backend: Option<String>,
    },

    /// 重启 JWM 进程
    Restart,
    /// 停止 JWM 进程（守护进程保持运行）
    Stop,
    /// 启动 JWM 进程（需先启动守护进程）
    Start,
    /// 退出守护进程及 JWM
    Quit,
    /// 查看守护进程与 JWM 运行状态
    Status,

    /// 查看 JWM 实时健康状态（不依赖守护进程）
    Health {
        /// 输出版本化 JSON 状态
        #[arg(long)]
        json: bool,
    },

    /// 列出 JWM IPC 支持的命令、查询和订阅主题
    Capabilities {
        /// 输出机器可读 JSON
        #[arg(long)]
        json: bool,
    },

    /// 编译并重启 JWM（cargo build --release）
    Rebuild {
        /// JWM 源码目录（默认 $HOME/jwm，可用 env JWM_DIR 覆盖）
        #[arg(long, env = "JWM_DIR", default_value_t = default_jwm_dir())]
        jwm_dir: String,
    },

    /// 安装 JWM 与桌面入口（参考 install_jwm_scripts.sh）
    Install {
        /// JWM 源码目录（默认 $HOME/jwm，可用 env JWM_DIR 覆盖）
        #[arg(long, env = "JWM_DIR", default_value_t = default_jwm_dir())]
        jwm_dir: String,
    },

    /// 检查守护进程是否存活
    DaemonCheck,
    /// 重启守护进程
    DaemonRestart,

    /// 打印调试信息（PID、套接字、控制管道等）
    Debug,

    /// 对比 niri / Hyprland 打印 wayland-udev 后端竞争力审计
    WaylandAudit {
        /// 输出 Markdown 表格，便于贴进文档或 issue
        #[arg(long)]
        markdown: bool,
    },

    /// 聚合 Wayland/KMS/合成器诊断状态
    WaylandStatus {
        /// 输出完整 JSON 响应集合
        #[arg(long)]
        json: bool,
    },

    /// Wayland smoke matrix 预检（不启动 GUI 客户端）
    WaylandSmoke {
        /// 输出完整 JSON 矩阵
        #[arg(long)]
        json: bool,
        /// 保存 JSON 报告；可选目录，默认 $XDG_RUNTIME_DIR/jwm-smoke
        #[arg(long, value_name = "DIR", num_args = 0..=1)]
        save: Option<Option<PathBuf>>,
    },

    /// 嵌套后端冒烟矩阵：启动 wayland-winit / wayland-x11，验证启动、IPC
    /// 健康、配置重载、窗口生命周期、截图能力与干净退出
    NestedSmoke {
        /// 只测指定后端（winit | x11；默认按宿主会话自动选择）
        #[arg(long, value_name = "BACKEND")]
        backend: Option<String>,
        /// 输出版本化 JSON 报告
        #[arg(long)]
        json: bool,
        /// 保存 JSON 报告；可选目录，默认 $XDG_RUNTIME_DIR/jwm-smoke
        #[arg(long, value_name = "DIR", num_args = 0..=1)]
        save: Option<Option<PathBuf>>,
        /// 被测 jwm 可执行文件（默认与 jwm-tool 同目录，其次 PATH）
        #[arg(long, env = "JWM_BINARY", value_name = "PATH")]
        jwm_binary: Option<PathBuf>,
        /// 窗口生命周期步骤使用的客户端命令（空格分隔；默认自动探测）
        #[arg(long, value_name = "CMD")]
        client: Option<String>,
        /// 通过时也保留私有运行目录与日志
        #[arg(long)]
        keep: bool,
    },

    /// 打印推荐的 Wayland scrolling 触控板手势配置片段
    WaylandGestureConfig {
        /// 只输出 TOML 片段，不带说明文字
        #[arg(long)]
        toml: bool,
    },

    /// 向 JWM IPC 发送消息 (JSON)
    #[command(
        long_about = "通过 Unix 套接字向 JWM 发送 IPC 消息。\n\
                      名称以 get_ 开头或已注册为查询时自动发送 query。\n\n\
                      \x1b[1m可用命令:\x1b[0m\n  \
                      窗口: focusstack, killclient, zoom, togglefloating, togglesticky,\n        \
                      togglepip, togglescratchpad, movestack\n  \
                      布局: setmfact, setcfact, incnmaster, setlayout, cyclelayout, togglebar\n  \
                      标签: view, tag, toggleview, toggletag, loopview\n  \
                      显示器: focusmon, tagmon\n  \
                      其他: spawn, quit, restart, reload_config, set_config, set_config_batch, command_batch\n  \
                      录屏: start_recording, set_recording_region, stop_recording, get_recording_status, toggle_recording, adjust_recording_region\n  \
                      录音: start_audio_recording, stop_audio_recording, get_audio_recording_status, toggle_audio_recording\n\n\
                      \x1b[1m可用查询:\x1b[0m\n  \
                      get_status, get_capabilities, get_windows, get_workspaces, get_monitors, get_tree,\n  \
                      get_config, get_config_status, get_version\n  \
                      完整列表: jwm-tool capabilities\n\n\
                      \x1b[1m可用布局:\x1b[0m\n  \
                      tile, float, monocle, fibonacci, centered_master, bstack,\n  \
                      grid, deck, three_col, tatami, fullscreen\n\n\
                      \x1b[1m事件主题 (--subscribe):\x1b[0m\n  \
                      window (window/new, window/close, window/focus, window/title)\n  \
                      tag (tag/view), layout (layout/set), monitor (monitor/focus)\n  \
                      config (config/reload), * (订阅全部)",
        after_help = "\x1b[1m示例:\x1b[0m\n  \
                      jwm-tool msg view --args '{\"tag\":2}'              # 切换到标签 2\n  \
                      jwm-tool msg focusstack --args '{\"value\":-1}'     # 聚焦上一个窗口\n  \
                      jwm-tool msg setlayout --args '{\"layout\":\"monocle\"}' # 设置布局\n  \
                      jwm-tool msg setmfact --args '0.05'               # 调整主区比例\n  \
                      jwm-tool msg spawn --args '{\"cmd\":[\"alacritty\"]}' # 启动终端\n  \
                      jwm-tool msg killclient                           # 关闭当前窗口\n  \
                      jwm-tool msg get_windows                          # 查询所有窗口\n  \
                      jwm-tool msg get_windows --raw                    # 原始 JSON 输出\n  \
                      jwm-tool msg reload_config                        # 重新加载配置\n  \
                      jwm-tool msg set_config --args '{\"key\":\"appearance.gap_px\",\"value\":8}'\n  \
                      jwm-tool msg set_config_batch --args '{\"values\":{\"appearance.gap_px\":8,\"status_bar.show_bar\":false}}'\n  \
                      jwm-tool msg command_batch --args '{\"commands\":[{\"command\":\"view\",\"args\":{\"tag\":1}},{\"command\":\"focusstack\",\"args\":{\"value\":1}}]}'\n  \
                      jwm-tool msg \"\" --subscribe 'window,tag'          # 订阅事件流\n  \
                      jwm-tool msg \"\" --subscribe '*'                   # 订阅全部事件"
    )]
    Msg {
        /// 命令或查询名称（get_ 前缀自动识别为查询）
        #[arg(help = "命令或查询名称（get_ 前缀自动识别为查询）\n\
                      命令: view, tag, focusstack, killclient, zoom, setlayout, spawn, ...\n\
                      查询: get_status, get_capabilities, get_windows, get_workspaces, ...")]
        name: String,
        /// JSON 参数，格式取决于命令类型
        #[arg(
            long,
            default_value = "null",
            help = "JSON 参数，格式取决于命令类型\n\
                      整数参数: '{\"value\": N}' 或直接 'N'  (focusstack, movestack, ...)\n\
                      浮点参数: '{\"value\": F}' 或直接 'F'  (setmfact, setcfact)\n\
                      标签参数: '{\"tag\": N}'               (view, tag, toggleview, ...)\n\
                      布局参数: '{\"layout\": \"name\"}'       (setlayout)\n\
                      命令参数: '{\"cmd\": [\"prog\", ...]}'   (spawn)"
        )]
        args: String,
        /// 订阅事件流（逗号分隔的主题列表）
        #[arg(
            long,
            help = "订阅事件流（逗号分隔的主题列表）\n\
                      主题: window, tag, layout, monitor, config, * (全部)\n\
                      事件: window/new, window/close, window/focus, window/title,\n\
                            tag/view, layout/set, monitor/focus, config/reload"
        )]
        subscribe: Option<String>,
        /// 输出原始 JSON（不做格式化美化）
        #[arg(long)]
        raw: bool,
    },
}

fn default_jwm_dir() -> String {
    env::var("HOME")
        .map(|h| format!("{}/jwm", h))
        .unwrap_or_else(|_| "./jwm".to_string())
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_else(runtime_dir)
}

fn log_dir() -> PathBuf {
    home_dir().join(".local/share/jwm")
}

fn log_file() -> PathBuf {
    log_dir().join("jwm_daemon.log")
}

/// 单个日志代的大小上限。超过后当前日志重命名为 `<name>.1`（替换旧的
/// 上一代），磁盘占用被限制在大约两代以内。长期运行建议改用 journald，
/// 见 tools/README.md。
const MAX_DAEMON_LOG_BYTES: u64 = 1024 * 1024;

fn rotated_log_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map_or_else(
        || std::ffi::OsString::from("jwm_daemon.log"),
        |n| n.to_os_string(),
    );
    name.push(".1");
    path.with_file_name(name)
}

fn rotate_log_if_needed(path: &Path, max_bytes: u64) -> io::Result<()> {
    let size = match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata.len(),
        _ => return Ok(()),
    };
    if size < max_bytes {
        return Ok(());
    }
    fs::rename(path, rotated_log_path(path))
}

fn append_log_with_rotation(path: &Path, line: &str, max_bytes: u64) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    rotate_log_if_needed(path, max_bytes)?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")?;
    file.flush()
}

fn now_ts() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn log_line(msg: &str) {
    let timestamp = now_ts();
    let line = format!("[{timestamp}] {msg}");
    let path = log_file();

    if let Err(error) = append_log_with_rotation(&path, &line, MAX_DAEMON_LOG_BYTES) {
        eprintln!("[{timestamp}] 无法写入日志文件 {}: {error}", path.display());
    }
    println!("{line}");
}

// --- JwmManager ---

struct JwmManager {
    jwm_binary: PathBuf,
    backend: Option<String>,
    jwm_child: Option<Child>,
    jwm_pid: Option<i32>,
}

impl JwmManager {
    fn new(jwm_binary: PathBuf, backend: Option<String>) -> Self {
        Self {
            jwm_binary,
            backend,
            jwm_child: None,
            jwm_pid: None,
        }
    }

    fn start(&mut self) -> io::Result<()> {
        if self.is_running() {
            if let Some(pid) = self.jwm_pid {
                log_line(&format!("JWM已在运行，PID: {}", pid));
            }
            return Ok(());
        }
        if !self.jwm_binary.is_file() {
            log_line(&format!(
                "错误: JWM二进制文件不存在: {}",
                self.jwm_binary.display()
            ));
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "JWM binary not found",
            ));
        }
        log_line(&format!("启动JWM: {}", self.jwm_binary.display()));
        let mut command = Command::new(&self.jwm_binary);
        if let Some(backend) = self.backend.as_ref() {
            if !backend.trim().is_empty() {
                command.env("JWM_BACKEND", backend);
            }
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let pid = child.id() as i32;
        self.jwm_pid = Some(pid);
        self.jwm_child = Some(child);
        log_line(&format!("JWM已启动，PID: {}", pid));
        Ok(())
    }

    /// Wait for the managed process to exit within `timeout`.
    /// Uses Child::try_wait if we own the handle, otherwise waitpid + kill(0).
    /// Returns true if the process exited.
    fn wait_for_exit(&mut self, pid: i32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        if let Some(child) = self.jwm_child.as_mut() {
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(Some(_)) => return true,
                    Ok(None) => {}
                    Err(e) => {
                        log_line(&format!("try_wait 错误: {e}"));
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(100));
            }
        } else {
            while Instant::now() < deadline {
                match waitpid(Pid::from_raw(pid), Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::StillAlive) => {}
                    Ok(_) => return true,
                    Err(nix::errno::Errno::ECHILD) => {
                        if kill(Pid::from_raw(pid), None).is_err() {
                            return true;
                        }
                    }
                    Err(e) => {
                        log_line(&format!("waitpid 错误: {e}"));
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        false
    }

    fn stop(&mut self) {
        let pid = match self.jwm_pid {
            Some(pid) => pid,
            None => {
                log_line("JWM进程未运行");
                return;
            }
        };

        log_line(&format!("停止JWM进程: {}", pid));

        // Phase 1: graceful SIGTERM
        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        let terminated = self.wait_for_exit(pid, Duration::from_secs(2));

        // Phase 2: force SIGKILL if still alive
        if !terminated {
            log_line(&format!("强制终止JWM进程: {}", pid));
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            self.wait_for_exit(pid, Duration::from_secs(2));
        }

        self.jwm_pid = None;
        self.jwm_child = None;
        log_line("JWM进程已停止");
    }

    fn restart(&mut self) -> io::Result<()> {
        log_line("重启JWM...");
        self.stop();
        // stop() already waited for exit, no extra sleep needed
        self.start()
    }

    fn status_str(&self) -> String {
        if let Some(pid) = self.jwm_pid {
            if self.process_exists(pid) {
                return format!("JWM运行中，PID: {}", pid);
            }
        }
        "JWM未运行".to_string()
    }

    fn is_running(&self) -> bool {
        if let Some(pid) = self.jwm_pid {
            self.process_exists(pid)
        } else {
            false
        }
    }

    fn process_exists(&self, pid: i32) -> bool {
        kill(Pid::from_raw(pid), None).is_ok()
    }
}

// --- Atomic response write ---

fn write_response(resp_file: &Path, s: &str) {
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

// --- IPC helpers ---

fn write_pidfile(pid: i32) -> io::Result<()> {
    let _ = fs::create_dir_all(runtime_dir());
    fs::write(pidfile_path(), pid.to_string())
}

fn read_existing_pid() -> Option<i32> {
    let content = fs::read_to_string(pidfile_path()).ok()?;
    content.trim().parse::<i32>().ok()
}

fn cleanup_resources(control_pipe: &Path) {
    log_line("开始清理资源...");
    let resp = response_path(control_pipe);
    let _ = fs::remove_file(control_pipe);
    let _ = fs::remove_file(&resp);
    let _ = fs::remove_file(resp.with_extension("tmp"));
    let _ = fs::remove_file(response_lock_path(control_pipe));
    let _ = fs::remove_file(pidfile_path());
    log_line("清理完成，守护进程退出");
}

fn mkfifo_safe(p: &Path) -> io::Result<()> {
    let _ = fs::remove_file(p);
    mkfifo(p, Mode::from_bits_truncate(0o600))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("mkfifo error: {e}")))
}

fn open_fifo_rdwr_nonblock(p: &Path) -> io::Result<OwnedFd> {
    open(p, OFlag::O_RDWR | OFlag::O_NONBLOCK, Mode::empty())
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("打开FIFO失败: {e}")))
}

fn read_commands_from_fd<F: AsFd>(fd: F, buf: &mut String) -> io::Result<Vec<String>> {
    let mut tmp = [0u8; 1024];
    let n = match read(fd, &mut tmp) {
        Ok(0) => 0,
        Ok(n) => n,
        Err(nix::errno::Errno::EAGAIN) => 0,
        Err(e) => {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("读取FIFO失败: {e}"),
            ));
        }
    };

    let mut cmds = Vec::new();
    if n > 0 {
        buf.push_str(&String::from_utf8_lossy(&tmp[..n]));
        while let Some(pos) = buf.find('\n') {
            let line: String = buf.drain(..=pos).collect();
            let cmd = line.trim();
            if !cmd.is_empty() {
                cmds.push(cmd.to_string());
            }
        }
    }
    Ok(cmds)
}

// --- Daemon main loop ---

fn run_daemon(jwm_binary: PathBuf, backend: Option<String>) -> io::Result<()> {
    let term_flag = Arc::new(AtomicBool::new(false));
    flag::register(SIGTERM, Arc::clone(&term_flag)).expect("注册SIGTERM失败");
    flag::register(SIGINT, Arc::clone(&term_flag)).expect("注册SIGINT失败");

    // Check for existing daemon
    if let Some(old_pid) = read_existing_pid() {
        if kill(Pid::from_raw(old_pid), None).is_ok() {
            eprintln!("守护进程已在运行，PID: {old_pid}");
            std::process::exit(1);
        } else {
            let _ = fs::remove_file(pidfile_path());
        }
    }

    let daemon_pid = std::process::id() as i32;
    write_pidfile(daemon_pid)?;

    let control_pipe = control_pipe_path(daemon_pid);
    if let Err(e) = mkfifo_safe(&control_pipe) {
        log_line(&format!(
            "错误: 无法创建控制管道 {}: {}",
            control_pipe.display(),
            e
        ));
        std::process::exit(1);
    }

    let fifo_fd = match open_fifo_rdwr_nonblock(&control_pipe) {
        Ok(fd) => fd,
        Err(e) => {
            log_line(&format!("错误: {}", e));
            std::process::exit(1);
        }
    };

    log_line(&format!("JWM守护进程启动，PID: {}", daemon_pid));
    log_line(&format!("控制管道: {}", control_pipe.display()));

    let mut mgr = JwmManager::new(jwm_binary, backend);
    if let Err(error) = mgr.start() {
        cleanup_resources(&control_pipe);
        return Err(error);
    }

    log_line("开始主循环，监听命令...");

    let mut line_buf = String::new();

    loop {
        if term_flag.load(Ordering::Relaxed) {
            if let Some(pid) = mgr.jwm_pid {
                log_line(&format!("终止JWM进程: {}", pid));
            }
            mgr.stop();
            cleanup_resources(&control_pipe);
            break;
        }

        // poll() on FIFO fd — block up to 200ms instead of busy-sleep
        let mut poll_fds = [PollFd::new(fifo_fd.as_fd(), PollFlags::POLLIN)];
        let _ = poll(&mut poll_fds, PollTimeout::from(200u8));

        match read_commands_from_fd(&fifo_fd, &mut line_buf) {
            Ok(cmds) => {
                for cmd in cmds {
                    log_line(&format!("收到命令: {}", cmd));
                    let resp_path = response_path(&control_pipe);
                    match cmd.as_str() {
                        "restart" => {
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
                        "quit" => {
                            log_line("收到退出命令");
                            write_response(&resp_path, "quit_done");
                            mgr.stop();
                            cleanup_resources(&control_pipe);
                            return Ok(());
                        }
                        "status" => {
                            let s = mgr.status_str();
                            write_response(&resp_path, &s);
                        }
                        other => {
                            log_line(&format!("未知命令: {}", other));
                            write_response(&resp_path, "unknown_command");
                        }
                    }
                }
            }
            Err(e) => {
                log_line(&format!("读取命令错误: {}", e));
            }
        }

        // Check JWM process health
        if let Some(pid) = mgr.jwm_pid {
            if kill(Pid::from_raw(pid), None).is_err() {
                log_line(&format!(
                    "检测到JWM意外退出 (PID: {}), 守护进程一并退出",
                    pid
                ));
                mgr.jwm_pid = None;
                mgr.jwm_child = None;
                cleanup_resources(&control_pipe);
                return Ok(());
            } else {
                // Reap zombie
                let _ = waitpid(Pid::from_raw(pid), Some(WaitPidFlag::WNOHANG));
            }
        }
    }

    Ok(())
}

// --- Control client helpers ---

fn read_daemon_pid() -> Option<i32> {
    let content = fs::read_to_string(pidfile_path()).ok()?;
    content.trim().parse::<i32>().ok()
}

fn process_exists(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

fn control_pipe_for(pid: i32) -> PathBuf {
    control_pipe_path(pid)
}

fn is_fifo(p: &Path) -> bool {
    fs::metadata(p)
        .map(|m| m.file_type().is_fifo())
        .unwrap_or(false)
}

fn find_control_pipe() -> Option<PathBuf> {
    let pid = read_daemon_pid()?;
    if !process_exists(pid) {
        return None;
    }
    let pipe = control_pipe_for(pid);
    if is_fifo(&pipe) { Some(pipe) } else { None }
}

fn send_command(cmd: &str) -> io::Result<()> {
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
    let data = format!("{cmd}\n");
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

fn check_daemon() -> bool {
    if let Some(pipe) = find_control_pipe() {
        println!("JWM守护进程正在运行");
        if let Some(pid) = read_daemon_pid() {
            println!("PID: {}", pid);
        }
        println!("控制管道: {}", pipe.display());
        true
    } else {
        println!("JWM守护进程未运行");
        false
    }
}

fn kill_daemon_by_pidfile() {
    if let Some(old_pid) = read_daemon_pid() {
        if process_exists(old_pid) {
            println!("终止旧的守护进程: {}", old_pid);
            let _ = kill(Pid::from_raw(old_pid), Signal::SIGTERM);
            thread::sleep(Duration::from_secs(1));
            if process_exists(old_pid) {
                let _ = kill(Pid::from_raw(old_pid), Signal::SIGKILL);
            }
        }
    }
}

fn cleanup_old_pipes_and_pidfile() {
    if let Ok(entries) = glob(&control_pipe_glob_pattern()) {
        for entry in entries.flatten() {
            let _ = fs::remove_file(entry);
        }
    }
    let _ = fs::remove_file(pidfile_path());
}

fn force_restart_daemon() -> io::Result<()> {
    println!("强制重启守护进程...");
    kill_daemon_by_pidfile();
    cleanup_old_pipes_and_pidfile();

    println!("启动新的守护进程...");
    let exe = env::current_exe()?;
    let child = Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let _ = child.id();

    thread::sleep(Duration::from_secs(1));
    if check_daemon() {
        println!("守护进程重启成功");
        Ok(())
    } else {
        eprintln!("守护进程重启失败");
        Err(io::Error::new(
            io::ErrorKind::Other,
            "daemon restart failed",
        ))
    }
}

// --- Build & Install ---

fn rebuild_and_restart(jwm_dir: &str) -> io::Result<()> {
    if !check_daemon() {
        println!("守护进程未运行，正在强制重启...");
        force_restart_daemon()?;
    }

    println!("开始编译JWM...");
    let status = Command::new("cargo")
        .arg("build")
        .arg("--locked")
        .arg("--release")
        .current_dir(jwm_dir)
        .status()?;
    if !status.success() {
        eprintln!("编译失败！");
        return Err(io::Error::new(io::ErrorKind::Other, "cargo build failed"));
    }

    install_jwm(jwm_dir)?;

    println!("重启JWM...");
    send_command("restart")?;
    println!("JWM编译并重启完成！");
    Ok(())
}

/// Run `sudo install -m <mode> <src> <dest_dir>` and return an error on failure.
fn sudo_install(src: &Path, dest_dir: &str, mode: &str) -> io::Result<()> {
    let status = Command::new("sudo")
        .arg("install")
        .arg("-m")
        .arg(mode)
        .arg(src)
        .arg(dest_dir)
        .status()?;
    if !status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("install {} to {} failed", src.display(), dest_dir),
        ));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct InstallPlanEntry {
    name: &'static str,
    source: PathBuf,
    destination_dir: &'static str,
    mode: &'static str,
}

/// Build the complete install plan before touching system paths.
///
/// Keeping the source and destination together prevents a newly inserted file
/// from silently changing another entry's destination through positional
/// indexing.
fn jwm_install_plan(jwm_dir: &Path) -> Vec<InstallPlanEntry> {
    vec![
        InstallPlanEntry {
            name: "jwm",
            source: jwm_dir.join("target/release/jwm"),
            destination_dir: "/usr/local/bin/",
            mode: "0755",
        },
        InstallPlanEntry {
            name: "jwm-tool",
            source: jwm_dir.join("target/release/jwm-tool"),
            destination_dir: "/usr/local/bin/",
            mode: "0755",
        },
        InstallPlanEntry {
            name: "jwm-support",
            source: jwm_dir.join("target/release/jwm-support"),
            destination_dir: "/usr/local/bin/",
            mode: "0755",
        },
        InstallPlanEntry {
            name: "jwm-x11rb.desktop",
            source: jwm_dir.join("jwm-x11rb.desktop"),
            destination_dir: "/usr/share/xsessions/",
            mode: "0644",
        },
        InstallPlanEntry {
            name: "jwm-xcb.desktop",
            source: jwm_dir.join("jwm-xcb.desktop"),
            destination_dir: "/usr/share/xsessions/",
            mode: "0644",
        },
        InstallPlanEntry {
            name: "jwm-wayland.desktop",
            source: jwm_dir.join("jwm-wayland.desktop"),
            destination_dir: "/usr/share/wayland-sessions/",
            mode: "0644",
        },
    ]
}

#[cfg(test)]
fn session_install_targets(jwm_dir: &Path) -> [(PathBuf, &'static str); 3] {
    [
        (jwm_dir.join("jwm-x11rb.desktop"), "/usr/share/xsessions/"),
        (jwm_dir.join("jwm-xcb.desktop"), "/usr/share/xsessions/"),
        (
            jwm_dir.join("jwm-wayland.desktop"),
            "/usr/share/wayland-sessions/",
        ),
    ]
}

fn install_jwm(jwm_dir: &str) -> io::Result<()> {
    let jwm_dir = Path::new(jwm_dir);
    let install_plan = jwm_install_plan(jwm_dir);

    for entry in &install_plan {
        if !entry.source.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} 未找到: {}", entry.name, entry.source.display()),
            ));
        }
    }

    println!("安装 JWM、jwm-tool 与 jwm-support...");

    let status = Command::new("sudo")
        .args([
            "rm",
            "-f",
            "/usr/local/bin/jwm",
            "/usr/local/bin/jwm-tool",
            "/usr/local/bin/jwm-support",
        ])
        .status()?;
    if !status.success() {
        eprintln!("清理旧二进制失败！");
        return Err(io::Error::new(io::ErrorKind::Other, "sudo rm failed"));
    }

    for entry in &install_plan {
        sudo_install(&entry.source, entry.destination_dir, entry.mode)?;
    }

    println!("安装完成");
    Ok(())
}

// --- Debug ---

/// Run `ps aux | grep <pattern>` and print matching lines.
fn ps_grep(pattern: &str) {
    let _ = Command::new("ps")
        .arg("aux")
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut ps| {
            let mut grep = Command::new("grep")
                .arg(pattern)
                .stdin(ps.stdout.take().expect("ps stdout"))
                .stdout(Stdio::inherit())
                .spawn()?;
            let _ = ps.wait();
            let _ = grep.wait();
            Ok(())
        });
}

fn tail_lines(p: &Path, n: usize) -> io::Result<Vec<String>> {
    use std::io::{BufRead, BufReader};
    let f = fs::File::open(p)?;
    let reader = BufReader::new(f);
    let mut buf: VecDeque<String> = VecDeque::with_capacity(n + 1);
    for line in reader.lines() {
        if let Ok(l) = line {
            buf.push_back(l);
            if buf.len() > n {
                buf.pop_front();
            }
        }
    }
    Ok(buf.into())
}

fn debug_info() {
    println!("=== JWM守护进程调试信息 ===");
    println!("时间: {}", Local::now().format("%Y-%m-%d %H:%M:%S"));
    println!("运行目录: {}", runtime_dir().display());
    println!();

    println!("1. 检查守护进程:");
    ps_grep("jwm-tool");

    println!("\n2. 检查PID文件:");
    if let Ok(pid) = fs::read_to_string(pidfile_path()) {
        println!("PID文件存在: {}", pid.trim());
    } else {
        println!("PID文件不存在");
    }

    println!("\n3. 检查控制管道:");
    let mut found = false;
    if let Ok(entries) = glob(&control_pipe_glob_pattern()) {
        for entry in entries.flatten() {
            found = true;
            if let Ok(meta) = fs::metadata(&entry) {
                println!(
                    "{}  {}",
                    if meta.file_type().is_fifo() {
                        "FIFO"
                    } else {
                        "NOT_FIFO"
                    },
                    entry.display()
                );
            }
        }
    }
    if !found {
        println!("未找到控制管道");
    }

    println!("\n4. 检查JWM进程:");
    ps_grep("-E \"jwm[^_]\"");

    println!("\n5. 检查日志:");
    let lf = log_file();
    if lf.exists() {
        println!("最近的日志:");
        match tail_lines(&lf, 10) {
            Ok(lines) => {
                for l in lines {
                    println!("{}", l);
                }
            }
            Err(_) => println!("读取日志失败"),
        }
    } else {
        println!("日志文件不存在");
    }

    println!("\n6. X11信息:");
    println!("DISPLAY: {}", env::var("DISPLAY").unwrap_or_default());
    ps_grep(" X");
}

// --- Wayland competitiveness audit ---

struct AuditRow {
    area: &'static str,
    competitor_signal: &'static str,
    jwm_now: &'static str,
    next_move: &'static str,
}

const WAYLAND_AUDIT_ROWS: &[AuditRow] = &[
    AuditRow {
        area: "布局体验",
        competitor_signal: "niri 的核心识别度来自 scrollable tiling；Hyprland 覆盖 dwindle/master/scrolling 等多布局。",
        jwm_now: "JWM 已有多布局、tag、overview、Wayland scrolling layout 模块，但 Wayland UX 仍更像 X11 WM 移植。",
        next_move: "把 scrolling layout 升为 Wayland 一等体验：独立每输出状态、自然触控板手势、overview 中可见列/窗口时间线。",
    },
    AuditRow {
        area: "协议与生态",
        competitor_signal: "niri/Hyprland 都依赖 layer-shell、screencopy、gamma、workspace、portal、XWayland 等生态协议形成日用闭环。",
        jwm_now: "udev 后端已覆盖 layer-shell、xdg-output、IME、XWayland、screencopy、image-copy-capture、workspace、gamma、output-power/management、foreign-toplevel、virtual-pointer。",
        next_move: "新增协议自检与默认策略：启动时输出缺失/禁用协议，给 waybar、kanshi、grim、OBS、wlsunset 一组回归脚本。",
    },
    AuditRow {
        area: "显示管线",
        competitor_signal: "Hyprland 强在动效/外观和游戏路径；niri 强在稳定的多显示器、混合 DPI、低干扰重排。",
        jwm_now: "JWM 已有 DRM/KMS、dmabuf feedback、direct scanout、VRR/tearing、HDR metadata、scene-linear、KMS gamma/CTM offload、per-monitor blur 策略。",
        next_move: "优先补三件可感知能力：每输出 presentation telemetry、direct-scanout 拒绝原因统计、混合 DPI/刷新率基准场景。",
    },
    AuditRow {
        area: "控制面",
        competitor_signal: "Hyprland 的 hyprctl 提供 monitors/workspaces/clients/devices/configerrors/rollinglog/JSON 等强控制面。",
        jwm_now: "jwm-tool 已有 daemon/status/msg 和 IPC 查询，但 Wayland 后端的输出、协议、KMS、颜色、延迟诊断还分散。",
        next_move: "把 compositor_get_metrics、VRR、KMS caps、color surfaces、session-lock、tearing hints 汇总成 `jwm-tool wayland-status --json`。",
    },
    AuditRow {
        area: "配置与规则",
        competitor_signal: "niri/Hyprland 的 live reload、窗口规则、工作区规则和动效参数是日用生产力入口。",
        jwm_now: "JWM 已有配置热加载、规则、动画/blur/HDR/VRR 选项；Wayland 协议开关目前偏环境变量。",
        next_move: "把 Wayland optional globals、HDR/VRR/tearing 策略、portal/capture 策略纳入 config_wayland.toml，并在 reload 后输出差异。",
    },
    AuditRow {
        area: "可靠性",
        competitor_signal: "niri 强调日用稳定、属性测试、profiling、输入延迟测量；Hyprland 依靠活跃生态快速修复兼容问题。",
        jwm_now: "JWM 已有 benchmark/metrics 文档和若干单测，但 Wayland 端还缺跨客户端冒烟矩阵。",
        next_move: "建立 `wayland-smoke`：foot/gtk/qt/electron/xwayland/waybar/grim/OBS/wlsunset/kanshi，记录截图、协议、帧统计。",
    },
];

fn print_wayland_audit(markdown: bool) {
    if markdown {
        println!("# JWM wayland-udev 竞争力审计\n");
        println!("| 领域 | niri / Hyprland 信号 | JWM 当前状态 | 下一步 |");
        println!("| --- | --- | --- | --- |");
        for row in WAYLAND_AUDIT_ROWS {
            println!(
                "| {} | {} | {} | {} |",
                row.area, row.competitor_signal, row.jwm_now, row.next_move
            );
        }
        return;
    }

    println!("=== JWM wayland-udev 竞争力审计 ===");
    println!("目标: 对标 niri 的专注体验与 Hyprland 的生态/控制面，并继续进化。");
    println!();
    for row in WAYLAND_AUDIT_ROWS {
        println!("[{}]", row.area);
        println!("  对手信号: {}", row.competitor_signal);
        println!("  JWM现在: {}", row.jwm_now);
        println!("  下一步: {}", row.next_move);
        println!();
    }
}

fn recommended_scrolling_gesture_toml() -> &'static str {
    r#"# 3+ finger swipes are intercepted only when configured.
# Keep this threshold high enough to avoid accidental client gesture capture.
behavior.gesture_swipe_threshold = 80.0

[[behavior.gesture_swipe]]
fingers = 3
direction = "left"
function = "scrolling_focus_column"
argument = 1

[[behavior.gesture_swipe]]
fingers = 3
direction = "right"
function = "scrolling_focus_column"
argument = -1

[[behavior.gesture_swipe]]
fingers = 3
direction = "up"
function = "scrolling_focus_window"
argument = -1

[[behavior.gesture_swipe]]
fingers = 3
direction = "down"
function = "scrolling_focus_window"
argument = 1
"#
}

fn print_wayland_gesture_config(toml_only: bool) {
    if !toml_only {
        println!("=== JWM Wayland Scrolling Gesture Config ===");
        println!("Paste this TOML into your JWM config to enable 3-finger scrolling navigation.");
        println!("The compositor only intercepts configured 3+ finger swipes.");
        println!(
            "If your config already sets behavior.gesture_swipe_threshold, keep only one value."
        );
        println!();
    }
    print!("{}", recommended_scrolling_gesture_toml());
}

struct SmokeTarget {
    area: &'static str,
    name: &'static str,
    commands: &'static [&'static str],
    required_protocols: &'static [&'static str],
    coverage: &'static str,
}

const WAYLAND_SMOKE_TARGETS: &[SmokeTarget] = &[
    SmokeTarget {
        area: "native",
        name: "terminal",
        commands: &["foot", "kitty", "alacritty"],
        required_protocols: &["xdg_wm_base", "wl_shm", "wl_data_device_manager"],
        coverage: "xdg-shell keyboard/pointer focus and resize",
    },
    SmokeTarget {
        area: "native",
        name: "gtk",
        commands: &["gtk4-demo", "gtk3-demo", "gedit", "nautilus"],
        required_protocols: &[
            "xdg_wm_base",
            "xdg_decoration",
            "wl_data_device_manager",
            "fractional_scale",
            "text_input",
        ],
        coverage: "GTK xdg-shell, popups, clipboard, fractional scale",
    },
    SmokeTarget {
        area: "native",
        name: "qt",
        commands: &["qterminal", "qtcreator", "assistant"],
        required_protocols: &[
            "xdg_wm_base",
            "xdg_decoration",
            "wl_data_device_manager",
            "fractional_scale",
        ],
        coverage: "Qt xdg-shell, decorations, mixed DPI",
    },
    SmokeTarget {
        area: "native",
        name: "electron",
        commands: &["code", "chromium", "discord"],
        required_protocols: &[
            "xdg_wm_base",
            "xdg_decoration",
            "wl_data_device_manager",
            "text_input",
            "presentation_time",
        ],
        coverage: "Electron frame pacing, IME, clipboard, popups",
    },
    SmokeTarget {
        area: "native",
        name: "sdl_vulkan",
        commands: &["vkcube", "weston-simple-egl", "glmark2-wayland"],
        required_protocols: &[
            "xdg_wm_base",
            "presentation_time",
            "wp_tearing_control_manager_v1",
        ],
        coverage: "games, dmabuf, presentation timing, VRR/tearing hints",
    },
    SmokeTarget {
        area: "shell",
        name: "bar",
        commands: &["waybar"],
        required_protocols: &["zwlr_layer_shell_v1", "wl_output", "xdg_output"],
        coverage: "layer-shell anchors, exclusive zones, output changes",
    },
    SmokeTarget {
        area: "shell",
        name: "launcher",
        commands: &["wofi", "rofi"],
        required_protocols: &[
            "xdg_wm_base",
            "zwlr_layer_shell_v1",
            "keyboard_shortcuts_inhibit",
        ],
        coverage: "popup/override placement, keyboard grabs",
    },
    SmokeTarget {
        area: "capture",
        name: "screenshot",
        commands: &["grim", "slurp"],
        required_protocols: &["zwlr_screencopy_manager_v1", "zwlr_layer_shell_v1"],
        coverage: "screencopy, selection overlays, capture policy",
    },
    SmokeTarget {
        area: "capture",
        name: "recording",
        commands: &["wf-recorder", "obs"],
        required_protocols: &[
            "zwlr_screencopy_manager_v1",
            "ext_image_copy_capture_manager_v1",
            "ext_output_image_capture_source_manager_v1",
        ],
        coverage: "screencopy/image-copy capture queues and dmabuf formats",
    },
    SmokeTarget {
        area: "color",
        name: "gamma",
        commands: &["wlsunset", "gammastep"],
        required_protocols: &["zwlr_gamma_control_manager_v1", "wp_color_manager_v1"],
        coverage: "gamma-control, color pipeline, output color diagnostics",
    },
    SmokeTarget {
        area: "output",
        name: "output_management",
        commands: &["kanshi", "wlr-randr"],
        required_protocols: &["zwlr_output_manager_v1", "zwlr_output_power_manager_v1"],
        coverage: "wlr-output-management apply/test and rollback diagnostics",
    },
    SmokeTarget {
        area: "xwayland",
        name: "xterm",
        commands: &["xterm", "xeyes"],
        required_protocols: &["xwayland_keyboard_grab", "wl_data_device_manager"],
        coverage: "XWayland map/focus/configure paths",
    },
    SmokeTarget {
        area: "xwayland",
        name: "legacy_window_types",
        commands: &["xmessage", "xclock", "xeyes"],
        required_protocols: &["xwayland_keyboard_grab"],
        coverage: "XWayland dialog/utility/override-redirect style legacy windows",
    },
    SmokeTarget {
        area: "xwayland",
        name: "steam",
        commands: &["steam"],
        required_protocols: &[
            "xwayland_keyboard_grab",
            "wp_tearing_control_manager_v1",
            "presentation_time",
        ],
        coverage: "XWayland fullscreen game and focus-steal behavior",
    },
    SmokeTarget {
        area: "xwayland",
        name: "legacy_tray",
        commands: &["stalonetray", "nm-applet", "blueman-applet"],
        required_protocols: &["xwayland_keyboard_grab"],
        coverage: "legacy X11 tray/status icons and override-redirect helpers",
    },
    SmokeTarget {
        area: "xwayland",
        name: "clipboard_bridge",
        commands: &["xclip", "xsel", "wl-copy"],
        required_protocols: &[
            "wl_data_device_manager",
            "primary_selection",
            "data_control",
        ],
        coverage: "X11/Wayland clipboard bridge smoke",
    },
];

fn split_path_list(path: &str) -> Vec<PathBuf> {
    path.split(':')
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn executable_in_dirs(command: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    if command.contains('/') {
        let path = PathBuf::from(command);
        return is_executable_file(&path).then_some(path);
    }

    dirs.iter()
        .map(|dir| dir.join(command))
        .find(|path| is_executable_file(path))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    meta.is_file() && (meta.permissions().mode() & 0o111) != 0
}

fn smoke_target_json(
    target: &SmokeTarget,
    path_dirs: &[PathBuf],
    published_protocols: Option<&HashSet<String>>,
    xwayland_status: Option<&serde_json::Value>,
) -> serde_json::Value {
    let found = target.commands.iter().find_map(|command| {
        executable_in_dirs(command, path_dirs).map(|path| {
            serde_json::json!({
                "command": command,
                "path": path.display().to_string(),
            })
        })
    });
    let (published, missing, satisfied) = match published_protocols {
        Some(protocols) => {
            let published = target
                .required_protocols
                .iter()
                .copied()
                .filter(|protocol| protocols.contains(*protocol))
                .collect::<Vec<_>>();
            let missing = target
                .required_protocols
                .iter()
                .copied()
                .filter(|protocol| !protocols.contains(*protocol))
                .collect::<Vec<_>>();
            (
                serde_json::json!(published),
                serde_json::json!(missing),
                serde_json::json!(missing.is_empty()),
            )
        }
        None => (
            serde_json::json!([]),
            serde_json::json!([]),
            serde_json::Value::Null,
        ),
    };

    let xwayland = if target.area == "xwayland" {
        let ready = xwayland_status
            .and_then(|status| status.get("wm_ready"))
            .and_then(|value| value.as_bool());
        serde_json::json!({
            "known": xwayland_status.is_some(),
            "wm_ready": ready,
            "display": xwayland_status
                .and_then(|status| status.get("display"))
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            "mapped_window_count": xwayland_status
                .and_then(|status| status.get("mapped_window_count"))
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            "satisfied": ready,
        })
    } else {
        serde_json::Value::Null
    };

    serde_json::json!({
        "area": target.area,
        "name": target.name,
        "commands": target.commands,
        "required_protocols": target.required_protocols,
        "protocols": {
            "known": published_protocols.is_some(),
            "published": published,
            "missing": missing,
            "satisfied": satisfied,
        },
        "xwayland": xwayland,
        "coverage": target.coverage,
        "available": found.is_some(),
        "found": found,
    })
}

fn query_wayland_status_data() -> Result<serde_json::Value, String> {
    match send_ipc_query("get_wayland_status") {
        Ok(resp) if resp.get("success").and_then(|v| v.as_bool()) == Some(true) => {
            Ok(resp.get("data").cloned().unwrap_or_default())
        }
        Ok(resp) => Err(resp
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("get_wayland_status failed")
            .to_string()),
        Err(e) => Err(e.to_string()),
    }
}

fn published_protocols_from_status(data: &serde_json::Value) -> Option<HashSet<String>> {
    let catalog = data
        .get("protocols")
        .and_then(|v| v.get("catalog"))
        .and_then(|v| v.as_array())?;
    Some(
        catalog
            .iter()
            .filter(|entry| entry.get("published").and_then(|v| v.as_bool()) == Some(true))
            .filter_map(|entry| entry.get("name").and_then(|v| v.as_str()))
            .map(ToString::to_string)
            .collect(),
    )
}

fn wayland_status_smoke_json(status: Result<&serde_json::Value, &String>) -> serde_json::Value {
    match status {
        Ok(data) => serde_json::json!({
                "available": true,
                "backend_family": data.get("backend_family").and_then(|v| v.as_str()).unwrap_or("unknown"),
                "outputs": data.get("outputs").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0),
                "windows": data.get("windows").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0),
                "protocol_catalog": data
                    .get("protocols")
                    .and_then(|v| v.get("catalog"))
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0),
                "capture": data.get("capture").is_some_and(|v| !v.is_null()),
                "render_decisions": data.get("render_decisions").is_some_and(|v| !v.is_null()),
                "output_management": data.get("output_management").is_some_and(|v| !v.is_null()),
                "presentation_timing": data.get("presentation_timing").is_some_and(|v| !v.is_null()),
                "xwayland": data.get("xwayland").cloned().unwrap_or(serde_json::Value::Null),
                "metrics_snapshot": data.get("metrics").cloned().unwrap_or(serde_json::Value::Null),
                "render_decisions_snapshot": data.get("render_decisions").cloned().unwrap_or(serde_json::Value::Null),
                "capture_snapshot": data.get("capture").cloned().unwrap_or(serde_json::Value::Null),
                "presentation_timing_snapshot": data.get("presentation_timing").cloned().unwrap_or(serde_json::Value::Null),
        }),
        Err(e) => serde_json::json!({
            "available": false,
            "error": e,
        }),
    }
}

fn smoke_log_summary_json() -> serde_json::Value {
    let path = log_file();
    let tail = tail_lines(&path, 20).unwrap_or_default();
    serde_json::json!({
        "path": path.display().to_string(),
        "exists": path.exists(),
        "tail_line_count": tail.len(),
        "tail": tail,
    })
}

fn smoke_artifacts_json() -> serde_json::Value {
    serde_json::json!({
        "screenshot_hashes": {
            "mode": "schema_reserved",
            "invasive_runner_required": true,
            "hash_algorithm": "sha256",
            "entries": [],
            "entry_schema": {
                "target": "smoke target name",
                "client": "launched command",
                "output": "output name or monitor index",
                "width": "captured width",
                "height": "captured height",
                "sha256": "hex digest of RGBA or PNG bytes",
            },
        },
        "gui_runner": {
            "implemented": false,
            "reason": "current wayland-smoke is non-invasive and does not launch GUI clients",
        },
    })
}

fn smoke_ci_profile_json(
    env_info: &serde_json::Value,
    status: Result<&serde_json::Value, &String>,
) -> serde_json::Value {
    let backend = status
        .ok()
        .and_then(|data| data.get("backend_family"))
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let session_type = env_info
        .get("xdg_session_type")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let nested_wayland = env_info
        .get("wayland_display")
        .and_then(|value| value.as_str())
        .is_some();

    serde_json::json!({
        "nested_wayland_available": nested_wayland,
        "recommended_backend": if nested_wayland { "wayland-winit" } else { "wayland-udev" },
        "current_backend_family": backend,
        "session_type": session_type,
        "kms_required_for_full_coverage": true,
        "notes": [
            "Use wayland-winit for nested CI protocol/client preflight when DRM/KMS is unavailable.",
            "Keep manual KMS runs for modeset, VRR, direct scanout, HDR metadata, and output-management rollback.",
        ],
    })
}

fn smoke_manual_kms_checklist_json() -> serde_json::Value {
    serde_json::json!([
        {
            "id": "kms-output-transaction",
            "area": "output",
            "command": "jwm-tool wayland-status --json",
            "evidence": "output_management.last_transaction, failed_outputs, rollback fields, outputs_before/after",
        },
        {
            "id": "direct-scanout-game",
            "area": "game",
            "command": "jwm-tool wayland-status --json",
            "evidence": "direct_scanout and render_decisions.direct_scanout blockers/active state",
        },
        {
            "id": "vrr-tearing",
            "area": "game",
            "command": "jwm-tool wayland-status --json",
            "evidence": "outputs[].vrr, tearing.active_surface_count, presentation_timing outputs",
        },
        {
            "id": "hdr-color",
            "area": "color",
            "command": "jwm-tool wayland-status --json",
            "evidence": "outputs[].hdr_metadata, color_management.session_policy, render_decisions.color_pipeline",
        },
        {
            "id": "capture-dmabuf",
            "area": "capture",
            "command": "jwm-tool wayland-smoke --save",
            "evidence": "capture snapshot queue counters, dmabuf_advertised, dmabuf_format_count",
        },
        {
            "id": "xwayland-fullscreen",
            "area": "xwayland",
            "command": "jwm-tool wayland-smoke --save",
            "evidence": "xwayland readiness, mapped_window_count, fullscreen/game target availability",
        }
    ])
}

fn default_smoke_report_dir() -> PathBuf {
    runtime_dir().join("jwm-smoke")
}

fn smoke_report_path(dir: &Path) -> PathBuf {
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    dir.join(format!("jwm-smoke-{stamp}.json"))
}

fn nested_smoke_report_path(dir: &Path) -> PathBuf {
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    dir.join(format!("jwm-nested-smoke-{stamp}.json"))
}

/// Resolve the jwm binary under test: explicit flag, then the sibling of this
/// jwm-tool executable, then plain `jwm` from PATH.
fn resolve_nested_smoke_jwm_binary(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }
    if let Ok(current) = env::current_exe() {
        if let Some(dir) = current.parent() {
            let sibling = dir.join("jwm");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("jwm")
}

fn parse_nested_smoke_backends(
    requested: Option<&str>,
) -> io::Result<Vec<nested_smoke::NestedBackendKind>> {
    let display = env::var("DISPLAY").ok();
    let wayland_display = env::var("WAYLAND_DISPLAY").ok();
    let eligible = nested_smoke::eligible_backends(display.as_deref(), wayland_display.as_deref());
    match requested {
        None | Some("all") => Ok(eligible),
        Some("winit") | Some("wayland-winit") => Ok(eligible
            .into_iter()
            .filter(|kind| matches!(kind, nested_smoke::NestedBackendKind::Winit))
            .collect()),
        Some("x11") | Some("wayland-x11") => Ok(eligible
            .into_iter()
            .filter(|kind| matches!(kind, nested_smoke::NestedBackendKind::X11))
            .collect()),
        Some(other) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown nested backend '{other}'; expected winit | x11 | all"),
        )),
    }
}

fn run_nested_smoke_command(
    backend: Option<&str>,
    json_output: bool,
    save: Option<Option<PathBuf>>,
    jwm_binary: Option<PathBuf>,
    client: Option<&str>,
    keep: bool,
) -> io::Result<i32> {
    let backends = parse_nested_smoke_backends(backend)?;
    if backends.is_empty() {
        eprintln!(
            "nested-smoke: no eligible nested backend on this host \
             (need DISPLAY and/or WAYLAND_DISPLAY)"
        );
        return Ok(2);
    }
    let options = nested_smoke::NestedSmokeOptions {
        backends,
        jwm_binary: resolve_nested_smoke_jwm_binary(jwm_binary),
        client: client.map(|command| {
            command
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        }),
        keep_artifacts: keep,
    };
    if !json_output {
        println!(
            "nested-smoke: testing {} with {}",
            options
                .backends
                .iter()
                .map(|kind| kind.jwm_backend_value())
                .collect::<Vec<_>>()
                .join(", "),
            options.jwm_binary.display()
        );
    }

    let report = nested_smoke::run_nested_smoke(&options);

    let saved_path = if let Some(dir) = save {
        let dir = dir.unwrap_or_else(default_smoke_report_dir);
        let path = nested_smoke_report_path(&dir);
        let value = serde_json::to_value(&report)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        save_wayland_smoke_snapshot_at(&value, &path)?;
        Some(path)
    } else {
        None
    };

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        );
    } else {
        for run in &report.runs {
            println!();
            println!(
                "[{}] {}",
                run.backend.jwm_backend_value(),
                match run.result {
                    nested_smoke::RunResult::Pass => "PASS",
                    nested_smoke::RunResult::Fail => "FAIL",
                }
            );
            for step in &run.steps {
                let status = match step.status {
                    nested_smoke::StepStatus::Pass => "pass",
                    nested_smoke::StepStatus::Fail => "FAIL",
                    nested_smoke::StepStatus::Skip => "skip",
                    nested_smoke::StepStatus::NotRun => "not-run",
                };
                println!(
                    "  {:<20} {:<7} {:>6} ms  {}",
                    step.name, status, step.duration_ms, step.detail
                );
                if let Some(action) = &step.action {
                    println!("  {:<20} {:<7} {:>9}  -> {}", "", "", "", action);
                }
            }
            if let Some(artifacts) = &run.artifacts_dir {
                println!("  artifacts: {artifacts}");
            }
        }
        if let Some(path) = &saved_path {
            println!();
            println!("report saved: {}", path.display());
        }
    }
    Ok(nested_smoke::matrix_exit_code(&report))
}

fn save_wayland_smoke_snapshot_at(snapshot: &serde_json::Value, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::File::create(&path)?;
    serde_json::to_writer_pretty(&mut file, snapshot)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    writeln!(file)?;
    Ok(())
}

fn wayland_smoke_snapshot() -> serde_json::Value {
    let path_dirs = env::var("PATH")
        .map(|path| split_path_list(&path))
        .unwrap_or_default();
    let wayland_status = query_wayland_status_data();
    let published_protocols = wayland_status
        .as_ref()
        .ok()
        .and_then(published_protocols_from_status);
    let xwayland_status = wayland_status
        .as_ref()
        .ok()
        .and_then(|status| status.get("xwayland"));
    let targets = WAYLAND_SMOKE_TARGETS
        .iter()
        .map(|target| {
            smoke_target_json(
                target,
                &path_dirs,
                published_protocols.as_ref(),
                xwayland_status,
            )
        })
        .collect::<Vec<_>>();
    let available = targets
        .iter()
        .filter(|target| {
            target
                .get("available")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
        })
        .count();
    let protocols_known = published_protocols.is_some();
    let protocol_satisfied = targets
        .iter()
        .filter(|target| {
            target
                .get("protocols")
                .and_then(|value| value.get("satisfied"))
                .and_then(|value| value.as_bool())
                == Some(true)
        })
        .count();
    let protocol_missing_targets = if protocols_known {
        targets.len().saturating_sub(protocol_satisfied)
    } else {
        0
    };
    let xwayland_target_count = targets
        .iter()
        .filter(|target| target.get("area").and_then(|value| value.as_str()) == Some("xwayland"))
        .count();
    let xwayland_ready_targets = targets
        .iter()
        .filter(|target| target.get("area").and_then(|value| value.as_str()) == Some("xwayland"))
        .filter(|target| {
            target
                .get("xwayland")
                .and_then(|value| value.get("satisfied"))
                .and_then(|value| value.as_bool())
                == Some(true)
        })
        .count();

    let environment = serde_json::json!({
        "wayland_display": env::var("WAYLAND_DISPLAY").ok(),
        "display": env::var("DISPLAY").ok(),
        "xdg_session_type": env::var("XDG_SESSION_TYPE").ok(),
        "xdg_current_desktop": env::var("XDG_CURRENT_DESKTOP").ok(),
        "xdg_runtime_dir": env::var("XDG_RUNTIME_DIR").ok(),
        "ipc_socket": ipc_socket_path(),
        "ipc_socket_exists": ipc_socket_path().exists(),
    });
    let ci_profile = smoke_ci_profile_json(&environment, wayland_status.as_ref());

    serde_json::json!({
        "generated_at": Local::now().to_rfc3339(),
        "environment": environment,
        "summary": {
            "target_count": targets.len(),
            "available_count": available,
            "missing_count": targets.len().saturating_sub(available),
            "protocols_known": protocols_known,
            "protocol_satisfied_count": protocol_satisfied,
            "protocol_missing_target_count": protocol_missing_targets,
            "xwayland_target_count": xwayland_target_count,
            "xwayland_ready_target_count": xwayland_ready_targets,
        },
        "jwm_status": wayland_status_smoke_json(wayland_status.as_ref()),
        "logs": smoke_log_summary_json(),
        "artifacts": smoke_artifacts_json(),
        "ci_profile": ci_profile,
        "manual_kms_checklist": smoke_manual_kms_checklist_json(),
        "targets": targets,
    })
}

fn print_wayland_smoke(json_output: bool, save: Option<Option<PathBuf>>) -> io::Result<()> {
    let mut snapshot = wayland_smoke_snapshot();
    let saved_path = if let Some(dir) = save {
        let dir = dir.unwrap_or_else(default_smoke_report_dir);
        let path = smoke_report_path(&dir);
        if let Some(object) = snapshot.as_object_mut() {
            if let Some(artifacts) = object
                .get_mut("artifacts")
                .and_then(|value| value.as_object_mut())
            {
                artifacts.insert(
                    "report_path".to_string(),
                    serde_json::json!(path.display().to_string()),
                );
            }
        }
        save_wayland_smoke_snapshot_at(&snapshot, &path)?;
        Some(path)
    } else {
        None
    };
    if json_output {
        println!("{}", serde_json::to_string_pretty(&snapshot).unwrap());
        if let Some(path) = saved_path {
            eprintln!("saved: {}", path.display());
        }
        return Ok(());
    }

    println!("=== JWM Wayland Smoke Matrix ===");
    println!(
        "generated_at: {}",
        snapshot["generated_at"].as_str().unwrap_or("")
    );
    let env_info = &snapshot["environment"];
    println!(
        "session: wayland_display={} display={} xdg_session_type={} ipc_socket_exists={}",
        env_info["wayland_display"].as_str().unwrap_or("none"),
        env_info["display"].as_str().unwrap_or("none"),
        env_info["xdg_session_type"].as_str().unwrap_or("none"),
        env_info["ipc_socket_exists"].as_bool().unwrap_or(false)
    );
    let summary = &snapshot["summary"];
    println!(
        "targets: available={}/{} missing={} protocol_satisfied={} protocol_missing_targets={} protocols_known={} xwayland_ready={}/{}",
        summary["available_count"].as_u64().unwrap_or(0),
        summary["target_count"].as_u64().unwrap_or(0),
        summary["missing_count"].as_u64().unwrap_or(0),
        summary["protocol_satisfied_count"].as_u64().unwrap_or(0),
        summary["protocol_missing_target_count"]
            .as_u64()
            .unwrap_or(0),
        summary["protocols_known"].as_bool().unwrap_or(false),
        summary["xwayland_ready_target_count"].as_u64().unwrap_or(0),
        summary["xwayland_target_count"].as_u64().unwrap_or(0)
    );
    let jwm_status = &snapshot["jwm_status"];
    println!(
        "jwm_status: available={} backend={} outputs={} protocols={} render_decisions={} capture={}",
        jwm_status["available"].as_bool().unwrap_or(false),
        jwm_status["backend_family"].as_str().unwrap_or("unknown"),
        jwm_status["outputs"].as_u64().unwrap_or(0),
        jwm_status["protocol_catalog"].as_u64().unwrap_or(0),
        jwm_status["render_decisions"].as_bool().unwrap_or(false),
        jwm_status["capture"].as_bool().unwrap_or(false)
    );
    if let Some(path) = saved_path {
        println!("saved: {}", path.display());
    }
    let logs = &snapshot["logs"];
    println!(
        "logs: exists={} tail_lines={} path={}",
        logs["exists"].as_bool().unwrap_or(false),
        logs["tail_line_count"].as_u64().unwrap_or(0),
        logs["path"].as_str().unwrap_or("unknown")
    );
    let artifacts = &snapshot["artifacts"];
    println!(
        "artifacts: screenshot_hashes={} gui_runner_implemented={}",
        artifacts
            .get("screenshot_hashes")
            .and_then(|value| value.get("mode"))
            .and_then(|value| value.as_str())
            .unwrap_or("unknown"),
        artifacts
            .get("gui_runner")
            .and_then(|value| value.get("implemented"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    );
    let ci_profile = &snapshot["ci_profile"];
    println!(
        "ci_profile: recommended_backend={} nested_wayland={} kms_required={}",
        ci_profile
            .get("recommended_backend")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown"),
        ci_profile
            .get("nested_wayland_available")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        ci_profile
            .get("kms_required_for_full_coverage")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    );
    println!(
        "manual_kms_checklist: items={}",
        snapshot["manual_kms_checklist"]
            .as_array()
            .map(|items| items.len())
            .unwrap_or(0)
    );

    if let Some(targets) = snapshot["targets"].as_array() {
        for target in targets {
            let status = if target["available"].as_bool().unwrap_or(false) {
                "ok"
            } else {
                "missing"
            };
            let found = target
                .get("found")
                .and_then(|found| found.get("command"))
                .and_then(|command| command.as_str())
                .unwrap_or("-");
            let protocols = target.get("protocols").unwrap_or(&serde_json::Value::Null);
            let protocol_status = match protocols.get("satisfied").and_then(|value| value.as_bool())
            {
                Some(true) => "proto=ok".to_string(),
                Some(false) => {
                    let missing = protocols
                        .get("missing")
                        .and_then(|value| value.as_array())
                        .map(|values| {
                            values
                                .iter()
                                .filter_map(|value| value.as_str())
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .unwrap_or_default();
                    format!("proto=missing({missing})")
                }
                None => "proto=unknown".to_string(),
            };
            let xwayland_status = if target["area"].as_str() == Some("xwayland") {
                match target
                    .get("xwayland")
                    .and_then(|value| value.get("satisfied"))
                    .and_then(|value| value.as_bool())
                {
                    Some(true) => " xwayland=ready".to_string(),
                    Some(false) => " xwayland=not-ready".to_string(),
                    None => " xwayland=unknown".to_string(),
                }
            } else {
                String::new()
            };
            println!(
                "{}.{:<18} {:<7} found={} {}{} coverage={}",
                target["area"].as_str().unwrap_or("unknown"),
                target["name"].as_str().unwrap_or("unknown"),
                status,
                found,
                protocol_status,
                xwayland_status,
                target["coverage"].as_str().unwrap_or("")
            );
        }
    }

    Ok(())
}

// --- main ---

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    match cli.cmd {
        Commands::Daemon {
            jwm_binary,
            backend,
        } => {
            let jwm_bin = jwm_binary
                .or_else(|| env::var("JWM_BINARY").ok())
                .unwrap_or_else(|| "/usr/local/bin/jwm".to_string());
            let backend = backend.or_else(|| env::var("JWM_BACKEND").ok());
            run_daemon(PathBuf::from(jwm_bin), backend)?;
        }

        Commands::Restart => send_command("restart")?,
        Commands::Stop => send_command("stop")?,
        Commands::Start => send_command("start")?,
        Commands::Quit => send_command("quit")?,
        Commands::Status => send_command("status")?,
        Commands::Health { json } => run_health(json)?,
        Commands::Capabilities { json } => run_capabilities(json)?,

        Commands::Rebuild { jwm_dir } => {
            rebuild_and_restart(&jwm_dir)?;
        }

        Commands::Install { jwm_dir } => {
            install_jwm(&jwm_dir)?;
        }

        Commands::DaemonCheck => {
            let _ = check_daemon();
        }
        Commands::DaemonRestart => {
            let _ = force_restart_daemon()?;
        }

        Commands::Debug => debug_info(),

        Commands::WaylandAudit { markdown } => print_wayland_audit(markdown),

        Commands::WaylandStatus { json } => run_wayland_status(json)?,

        Commands::WaylandSmoke { json, save } => print_wayland_smoke(json, save)?,

        Commands::NestedSmoke {
            backend,
            json,
            save,
            jwm_binary,
            client,
            keep,
        } => {
            let code = run_nested_smoke_command(
                backend.as_deref(),
                json,
                save,
                jwm_binary,
                client.as_deref(),
                keep,
            )?;
            if code != 0 {
                std::process::exit(code);
            }
        }

        Commands::WaylandGestureConfig { toml } => print_wayland_gesture_config(toml),

        Commands::Msg {
            name,
            args,
            subscribe,
            raw,
        } => {
            run_ipc_msg(&name, &args, subscribe.as_deref(), raw)?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// IPC msg subcommand
// ---------------------------------------------------------------------------

fn ipc_socket_path() -> PathBuf {
    match jwm::ipc_server::validated_socket_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("Refusing unsafe JWM IPC endpoint: {error}");
            PathBuf::from("/dev/null/jwm-runtime-unavailable/jwm-ipc.sock")
        }
    }
}

fn parse_msg_args(args: &str) -> io::Result<serde_json::Value> {
    serde_json::from_str(args).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid JSON passed to --args: {error}"),
        )
    })
}

fn parse_subscription_topics(topics: &str) -> io::Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut parsed = Vec::new();
    for topic in topics
        .split(',')
        .map(str::trim)
        .filter(|topic| !topic.is_empty())
    {
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

fn ipc_request(name: &str, args: serde_json::Value) -> serde_json::Value {
    if name.starts_with("get_") || jwm::ipc::is_supported_query(name) {
        serde_json::json!({ "query": name, "args": args })
    } else {
        serde_json::json!({ "command": name, "args": args })
    }
}

fn ensure_ipc_response_succeeded(response: &str) -> io::Result<()> {
    if serde_json::from_str::<serde_json::Value>(response.trim()).is_err() {
        // Preserve compatibility with legacy/plain-text server responses.
        return Ok(());
    }
    validate_ipc_response(response)
}

fn run_ipc_msg(name: &str, args_str: &str, subscribe: Option<&str>, raw: bool) -> io::Result<()> {
    // Validate --args before touching the socket. Subscription requests do not
    // consume the value, but malformed input is still a CLI usage error.
    let args = parse_msg_args(args_str)?;
    let request = if let Some(topics) = subscribe {
        let topic_list = parse_subscription_topics(topics)?;
        serde_json::json!({ "subscribe": topic_list })
    } else {
        ipc_request(name, args)
    };

    let sock_path = ipc_socket_path();
    if !sock_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "IPC socket not found at {}; is JWM running?",
                sock_path.display()
            ),
        ));
    }

    let mut stream = UnixStream::connect(&sock_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    let mut line = serde_json::to_string(&request)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;

    // Handle subscribe mode
    if subscribe.is_some() {
        // Read subscription confirmation
        let resp = read_ipc_line(&mut stream)?;
        ensure_ipc_response_succeeded(&resp)?;
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
                            Ok(value) => {
                                println!("{}", serde_json::to_string_pretty(&value).unwrap_or(line))
                            }
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

    // Read response
    let resp = read_ipc_line(&mut stream)?;
    if raw {
        print!("{resp}");
    } else {
        match serde_json::from_str::<serde_json::Value>(resp.trim()) {
            Ok(value) => match serde_json::to_string_pretty(&value) {
                Ok(pretty) => println!("{pretty}"),
                Err(_) => print!("{resp}"),
            },
            Err(_) => print!("{resp}"),
        }
    }

    ensure_ipc_response_succeeded(&resp)
}

#[cfg(test)]
// The CLI keeps protocol parsing tests beside the message implementation;
// later functions belong to independent smoke/reporting commands.
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::{
        Cli, Commands, InstallPlanEntry, SmokeTarget, acquire_response_lock,
        append_log_with_rotation, capabilities_output_lines, daemon_command_response,
        ensure_ipc_response_succeeded, health_output_lines, ipc_request, jwm_install_plan,
        parse_msg_args, parse_subscription_topics, response_lock_path, rotated_log_path,
        session_install_targets, smoke_artifacts_json, smoke_ci_profile_json,
        smoke_manual_kms_checklist_json, smoke_target_json, split_path_list, successful_query_data,
        validate_daemon_response, validate_ipc_response,
    };
    use clap::Parser;
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    struct TempLogDir(PathBuf);

    impl TempLogDir {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("jwm-tool-log-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempLogDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn daemon_log_rotates_at_the_bound_and_keeps_at_most_two_generations() {
        let dir = TempLogDir::new("rotate");
        let path = dir.0.join("jwm_daemon.log");
        let rotated = rotated_log_path(&path);
        assert_eq!(rotated.file_name().unwrap(), "jwm_daemon.log.1");

        // 低于上限时只追加，不轮转。
        append_log_with_rotation(&path, "first", 64).unwrap();
        append_log_with_rotation(&path, "second", 64).unwrap();
        assert!(!rotated.exists());

        // 达到上限后当前文件成为上一代，新行进入新的当前文件。
        let filler = "x".repeat(80);
        append_log_with_rotation(&path, &filler, 64).unwrap();
        append_log_with_rotation(&path, "after-rotation", 64).unwrap();
        let previous = fs::read_to_string(&rotated).unwrap();
        assert!(previous.contains("first") && previous.contains(&filler));
        let current = fs::read_to_string(&path).unwrap();
        assert!(current.contains("after-rotation") && !current.contains("first"));

        // 再次轮转会替换上一代：总占用始终限制在两代以内。
        let filler2 = "y".repeat(80);
        append_log_with_rotation(&path, &filler2, 64).unwrap();
        append_log_with_rotation(&path, "third-generation", 64).unwrap();
        let previous = fs::read_to_string(&rotated).unwrap();
        assert!(previous.contains(&filler2) && !previous.contains("first"));
        assert_eq!(
            fs::read_dir(&dir.0).unwrap().count(),
            2,
            "rotation must never accumulate more than two log files"
        );
    }

    #[test]
    fn daemon_log_append_creates_missing_directories() {
        let dir = TempLogDir::new("mkdir");
        let path = dir.0.join("nested").join("jwm_daemon.log");
        append_log_with_rotation(&path, "hello", 1024).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("hello"));
    }

    #[test]
    fn cli_accepts_health_and_capabilities_json_without_changing_status() {
        assert!(matches!(
            Cli::try_parse_from(["jwm-tool", "health", "--json"])
                .unwrap()
                .cmd,
            Commands::Health { json: true }
        ));
        assert!(matches!(
            Cli::try_parse_from(["jwm-tool", "capabilities", "--json"])
                .unwrap()
                .cmd,
            Commands::Capabilities { json: true }
        ));
        assert!(matches!(
            Cli::try_parse_from(["jwm-tool", "status"]).unwrap().cmd,
            Commands::Status
        ));
    }

    #[test]
    fn insight_output_helpers_render_status_and_reject_failed_queries() {
        let status = serde_json::json!({
            "schema_version": 1,
            "version": "0.2.0",
            "backend": "wayland-winit",
            "uptime_ms": 1234,
            "health": {"status": "degraded", "reasons": ["no monitors are available"]},
            "counts": {"windows": 2, "monitors": 0, "workspaces": 0},
            "config": {
                "path": "/tmp/config.toml",
                "diagnostics": {"error_count": 0, "warning_count": 1}
            },
            "features": {"overview": true, "recording": false},
            "compositor_metrics": null
        });
        let lines = health_output_lines(&status);
        assert!(lines.iter().any(|line| line == "backend: wayland-winit"));
        assert!(lines.iter().any(|line| line == "active_features: overview"));
        assert!(
            lines
                .iter()
                .any(|line| line == "reason: no monitors are available")
        );

        let capabilities = serde_json::json!({
            "schema_version": 1,
            "commands": ["focusstack", "reload_config"],
            "queries": ["get_status"],
            "subscription_topics": ["window"]
        });
        let lines = capabilities_output_lines(&capabilities);
        assert!(lines[1].contains("reload_config"));
        assert!(lines[2].contains("get_status"));

        assert!(
            successful_query_data(
                "get_status",
                serde_json::json!({"success": false, "error": "not ready"})
            )
            .is_err()
        );
    }

    #[test]
    fn install_plan_routes_desktops_to_their_session_types() {
        let root = Path::new("/src/jwm");

        assert_eq!(
            jwm_install_plan(root),
            vec![
                InstallPlanEntry {
                    name: "jwm",
                    source: PathBuf::from("/src/jwm/target/release/jwm"),
                    destination_dir: "/usr/local/bin/",
                    mode: "0755",
                },
                InstallPlanEntry {
                    name: "jwm-tool",
                    source: PathBuf::from("/src/jwm/target/release/jwm-tool"),
                    destination_dir: "/usr/local/bin/",
                    mode: "0755",
                },
                InstallPlanEntry {
                    name: "jwm-support",
                    source: PathBuf::from("/src/jwm/target/release/jwm-support"),
                    destination_dir: "/usr/local/bin/",
                    mode: "0755",
                },
                InstallPlanEntry {
                    name: "jwm-x11rb.desktop",
                    source: PathBuf::from("/src/jwm/jwm-x11rb.desktop"),
                    destination_dir: "/usr/share/xsessions/",
                    mode: "0644",
                },
                InstallPlanEntry {
                    name: "jwm-xcb.desktop",
                    source: PathBuf::from("/src/jwm/jwm-xcb.desktop"),
                    destination_dir: "/usr/share/xsessions/",
                    mode: "0644",
                },
                InstallPlanEntry {
                    name: "jwm-wayland.desktop",
                    source: PathBuf::from("/src/jwm/jwm-wayland.desktop"),
                    destination_dir: "/usr/share/wayland-sessions/",
                    mode: "0644",
                },
            ]
        );
    }

    #[test]
    fn msg_args_reject_invalid_json_as_invalid_input() {
        let error = parse_msg_args("{not-json").unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("invalid JSON passed to --args"));
    }

    #[test]
    fn msg_request_preserves_command_and_query_shapes() {
        assert_eq!(
            ipc_request("view", serde_json::json!({ "tag": 2 })),
            serde_json::json!({ "command": "view", "args": { "tag": 2 } })
        );
        assert_eq!(
            ipc_request("get_windows", serde_json::Value::Null),
            serde_json::json!({ "query": "get_windows", "args": null })
        );
    }

    #[test]
    fn msg_response_reports_server_failure() {
        let error = ensure_ipc_response_succeeded(r#"{"success":false,"error":"unknown command"}"#)
            .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(error.to_string().contains("unknown command"));
    }

    #[test]
    fn msg_response_keeps_success_and_legacy_responses_compatible() {
        for response in [
            r#"{"success":true,"data":null}"#,
            r#"{"data":{"windows":[]}}"#,
            "legacy_ok",
        ] {
            ensure_ipc_response_succeeded(response).unwrap();
        }
    }

    #[test]
    fn split_path_list_ignores_empty_entries() {
        let dirs = split_path_list(":/bin::/usr/bin:");

        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].to_string_lossy(), "/bin");
        assert_eq!(dirs[1].to_string_lossy(), "/usr/bin");
    }

    #[test]
    fn smoke_target_json_reports_missing_without_path_dirs() {
        let target = SmokeTarget {
            area: "native",
            name: "demo",
            commands: &["definitely-not-a-jwm-smoke-test-command"],
            required_protocols: &["xdg_wm_base"],
            coverage: "test coverage",
        };

        let value = smoke_target_json(&target, &[], None, None);

        assert_eq!(value["available"], false);
        assert!(value["found"].is_null());
        assert_eq!(value["protocols"]["known"], false);
        assert!(value["protocols"]["satisfied"].is_null());
    }

    #[test]
    fn smoke_target_json_reports_missing_protocols() {
        let target = SmokeTarget {
            area: "capture",
            name: "demo",
            commands: &["definitely-not-a-jwm-smoke-test-command"],
            required_protocols: &["xdg_wm_base", "zwlr_screencopy_manager_v1"],
            coverage: "test coverage",
        };
        let protocols = HashSet::from(["xdg_wm_base".to_string()]);

        let value = smoke_target_json(&target, &[], Some(&protocols), None);

        assert_eq!(value["protocols"]["known"], true);
        assert_eq!(value["protocols"]["satisfied"], false);
        assert_eq!(
            value["protocols"]["missing"],
            serde_json::json!(["zwlr_screencopy_manager_v1"])
        );
    }

    #[test]
    fn smoke_target_json_reports_xwayland_readiness() {
        let target = SmokeTarget {
            area: "xwayland",
            name: "xterm",
            commands: &["definitely-not-a-jwm-smoke-test-command"],
            required_protocols: &["xwayland_keyboard_grab"],
            coverage: "test coverage",
        };
        let protocols = HashSet::from(["xwayland_keyboard_grab".to_string()]);
        let xwayland = serde_json::json!({
            "wm_ready": true,
            "display": ":42",
            "mapped_window_count": 2,
        });

        let value = smoke_target_json(&target, &[], Some(&protocols), Some(&xwayland));

        assert_eq!(value["xwayland"]["known"], true);
        assert_eq!(value["xwayland"]["wm_ready"], true);
        assert_eq!(value["xwayland"]["display"], ":42");
        assert_eq!(value["xwayland"]["satisfied"], true);
    }

    #[test]
    fn smoke_artifacts_reserve_screenshot_hash_schema() {
        let artifacts = smoke_artifacts_json();

        assert_eq!(artifacts["screenshot_hashes"]["mode"], "schema_reserved");
        assert_eq!(artifacts["screenshot_hashes"]["hash_algorithm"], "sha256");
        assert!(
            artifacts["screenshot_hashes"]["entries"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(artifacts["gui_runner"]["implemented"], false);
    }

    #[test]
    fn smoke_ci_profile_prefers_winit_when_nested_wayland_exists() {
        let env_info = serde_json::json!({
            "wayland_display": "wayland-1",
            "xdg_session_type": "wayland",
        });
        let status = serde_json::json!({
            "backend_family": "wayland",
        });

        let profile = smoke_ci_profile_json(&env_info, Ok(&status));

        assert_eq!(profile["nested_wayland_available"], true);
        assert_eq!(profile["recommended_backend"], "wayland-winit");
        assert_eq!(profile["kms_required_for_full_coverage"], true);
    }

    #[test]
    fn smoke_manual_kms_checklist_covers_core_drm_paths() {
        let checklist = smoke_manual_kms_checklist_json();
        let ids = checklist
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item.get("id").and_then(|value| value.as_str()))
            .collect::<HashSet<_>>();

        assert!(ids.contains("kms-output-transaction"));
        assert!(ids.contains("direct-scanout-game"));
        assert!(ids.contains("vrr-tearing"));
        assert!(ids.contains("hdr-color"));
        assert!(ids.contains("capture-dmabuf"));
        assert!(ids.contains("xwayland-fullscreen"));
    }

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
        let error = validate_ipc_response(r#"{"success":false,"error":"unknown command: nope"}"#)
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
        let pipe = std::env::temp_dir().join(format!("jwm-tool-lock-test-{}", std::process::id()));
        let lock_path = response_lock_path(&pipe);
        let _ = std::fs::remove_file(&lock_path);

        let first = acquire_response_lock(&pipe, std::time::Duration::from_millis(50)).unwrap();
        assert!(acquire_response_lock(&pipe, std::time::Duration::from_millis(20)).is_err());
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
}

fn send_ipc_query(name: &str) -> io::Result<serde_json::Value> {
    let sock_path = ipc_socket_path();
    if !sock_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("IPC socket not found at {}", sock_path.display()),
        ));
    }

    let mut stream = UnixStream::connect(&sock_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    let msg = serde_json::json!({ "query": name, "args": serde_json::Value::Null });
    let mut line = serde_json::to_string(&msg).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes())?;

    let resp = read_ipc_line(&mut stream)?;
    serde_json::from_str::<serde_json::Value>(resp.trim())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{name}: {e}")))
}

fn successful_query_data(name: &str, response: serde_json::Value) -> io::Result<serde_json::Value> {
    if response.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
        let error = response
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("IPC query failed");
        return Err(io::Error::other(format!("{name}: {error}")));
    }
    response.get("data").cloned().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{name}: successful response did not contain data"),
        )
    })
}

fn health_output_lines(status: &serde_json::Value) -> Vec<String> {
    let health = status["health"]["status"].as_str().unwrap_or("unknown");
    let backend = status["backend"].as_str().unwrap_or("unknown");
    let version = status["version"].as_str().unwrap_or("unknown");
    let uptime_ms = status["uptime_ms"].as_u64().unwrap_or(0);
    let windows = status["counts"]["windows"].as_u64().unwrap_or(0);
    let monitors = status["counts"]["monitors"].as_u64().unwrap_or(0);
    let workspaces = status["counts"]["workspaces"].as_u64().unwrap_or(0);
    let config = &status["config"];
    let config_path = config["path"].as_str().unwrap_or("unknown");
    let config_errors = config["diagnostics"]["error_count"].as_u64().unwrap_or(0);
    let config_warnings = config["diagnostics"]["warning_count"].as_u64().unwrap_or(0);

    let mut active_features = status["features"]
        .as_object()
        .map(|features| {
            features
                .iter()
                .filter(|(_, enabled)| enabled.as_bool() == Some(true))
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    active_features.sort_unstable();

    let mut lines = vec![
        "=== JWM Health ===".to_string(),
        format!("status: {health}"),
        format!("version: {version}"),
        format!("backend: {backend}"),
        format!("uptime_ms: {uptime_ms}"),
        format!("windows: {windows}  monitors: {monitors}  workspaces: {workspaces}"),
        format!("config: {config_path} (errors={config_errors}, warnings={config_warnings})"),
        format!(
            "active_features: {}",
            if active_features.is_empty() {
                "none".to_string()
            } else {
                active_features.join(",")
            }
        ),
    ];

    if let Some(metrics) = status["compositor_metrics"].as_object() {
        let fps = metrics.get("fps").and_then(serde_json::Value::as_f64);
        let input_p95 = metrics
            .get("input_latency_p95_ms")
            .and_then(serde_json::Value::as_f64);
        if let (Some(fps), Some(input_p95)) = (fps, input_p95) {
            lines.push(format!(
                "compositor: fps={fps:.1} input_p95_ms={input_p95:.2}"
            ));
        }
    }

    if let Some(reasons) = status["health"]["reasons"].as_array() {
        lines.extend(
            reasons
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(|reason| format!("reason: {reason}")),
        );
    }
    lines
}

fn capabilities_output_lines(capabilities: &serde_json::Value) -> Vec<String> {
    let names = |field: &str| {
        capabilities[field]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default()
    };
    vec![
        format!(
            "JWM IPC capabilities (schema {})",
            capabilities["schema_version"].as_u64().unwrap_or(0)
        ),
        format!("commands: {}", names("commands")),
        format!("queries: {}", names("queries")),
        format!("subscription_topics: {}", names("subscription_topics")),
    ]
}

fn run_health(json_output: bool) -> io::Result<()> {
    let response = send_ipc_query("get_status")?;
    let status = successful_query_data("get_status", response)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&status).unwrap());
    } else {
        for line in health_output_lines(&status) {
            println!("{line}");
        }
    }
    Ok(())
}

fn run_capabilities(json_output: bool) -> io::Result<()> {
    let response = send_ipc_query("get_capabilities")?;
    let capabilities = successful_query_data("get_capabilities", response)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&capabilities).unwrap());
    } else {
        for line in capabilities_output_lines(&capabilities) {
            println!("{line}");
        }
    }
    Ok(())
}

fn response_data<'a>(
    responses: &'a serde_json::Value,
    name: &str,
) -> Option<&'a serde_json::Value> {
    responses.get(name)?.get("data")
}

fn response_array_len(responses: &serde_json::Value, name: &str) -> usize {
    response_data(responses, name)
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

fn print_unified_wayland_status(status: &serde_json::Value) {
    let version = status
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let backend = status
        .get("backend_family")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let output_count = status
        .get("outputs")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let workspace_count = status
        .get("workspaces")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let window_count = status
        .get("windows")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    println!("=== JWM Wayland Status ===");
    println!("version: {}", version);
    println!("backend: {}", backend);
    println!("socket: {}", ipc_socket_path().display());
    println!("monitors: {}", output_count);
    println!("workspaces: {}", workspace_count);
    println!("windows: {}", window_count);

    if let Some(config) = status.get("config").filter(|v| !v.is_null()) {
        let path = config
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let exists = config
            .get("exists")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let reload = config.get("reload").unwrap_or(&serde_json::Value::Null);
        let attempts = reload
            .get("attempt_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let last_success = reload
            .get("last_success")
            .and_then(|v| v.as_bool())
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".into());
        let last_error = reload
            .get("last_error")
            .and_then(|v| v.as_str())
            .unwrap_or("none");
        println!(
            "config: path={} exists={} reload_attempts={} last_success={} last_error=\"{}\"",
            path, exists, attempts, last_success, last_error
        );
    }

    if let Some(first_output) = status
        .get("outputs")
        .and_then(|v| v.as_array())
        .and_then(|outputs| outputs.first())
    {
        let name = first_output
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let stable_key = first_output
            .get("identity")
            .and_then(|v| v.get("stable_key"))
            .and_then(|v| v.as_str())
            .unwrap_or(name);
        let monitor_name = first_output
            .get("identity")
            .and_then(|v| v.get("monitor_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        println!(
            "first_output: name={} monitor_name={} stable_key={}",
            name, monitor_name, stable_key
        );
    }

    if let Some(metrics) = status.get("metrics").filter(|v| !v.is_null()) {
        let fps = metrics.get("fps").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let input_p95 = metrics
            .get("input_latency_p95_ms")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let direct_scanout = metrics
            .get("direct_scanout_active")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        println!(
            "metrics: fps={:.1} input_p95_ms={:.2} direct_scanout={}",
            fps, input_p95, direct_scanout
        );
    }

    if let Some(scanout) = status.get("direct_scanout").filter(|v| !v.is_null()) {
        let active = scanout
            .get("active")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let reason = scanout
            .get("compositor_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let first_kms = scanout
            .get("kms_outputs")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| {
                Some(format!(
                    "{}: {}",
                    v.get("output_name")?.as_str()?,
                    v.get("reason")?.as_str()?
                ))
            });
        match first_kms {
            Some(kms) => println!(
                "direct_scanout: active={} compositor_reason=\"{}\" kms=\"{}\"",
                active, reason, kms
            ),
            None => println!(
                "direct_scanout: active={} compositor_reason=\"{}\"",
                active, reason
            ),
        }
    }

    if let Some(decisions) = status.get("render_decisions").filter(|v| !v.is_null()) {
        let scanout = decisions
            .get("direct_scanout")
            .unwrap_or(&serde_json::Value::Null);
        let blur = decisions.get("blur").unwrap_or(&serde_json::Value::Null);
        let hdr = decisions.get("hdr").unwrap_or(&serde_json::Value::Null);
        let tearing = decisions.get("tearing").unwrap_or(&serde_json::Value::Null);
        let color = decisions
            .get("color_pipeline")
            .unwrap_or(&serde_json::Value::Null);
        println!(
            "render_decisions: scanout={}({}) blur={}({}) hdr={}({}) tearing={}({}) shader_fallback_outputs={}",
            scanout
                .get("active")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            scanout
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
            blur.get("active")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            blur.get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
            hdr.get("active").and_then(|v| v.as_bool()).unwrap_or(false),
            hdr.get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
            tearing
                .get("active")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            tearing
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
            color
                .get("shader_fallback_output_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        );
    }

    if let Some(timing) = status.get("presentation_timing").filter(|v| !v.is_null()) {
        let pending = timing
            .get("any_frame_pending")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let first_output = timing
            .get("outputs")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first());
        if let Some(output) = first_output {
            let name = output
                .get("output_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let refresh = output
                .get("refresh_interval_ms")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let last_vblank = output
                .get("last_vblank_ago_ms")
                .and_then(|v| v.as_u64())
                .map(|v| v.to_string())
                .unwrap_or_else(|| "never".into());
            let pending_for = output
                .get("frame_pending_for_ms")
                .and_then(|v| v.as_u64())
                .map(|v| v.to_string())
                .unwrap_or_else(|| "none".into());
            println!(
                "presentation: pending={} first_output={} refresh_ms={:.2} last_vblank_ago_ms={} pending_for_ms={}",
                pending, name, refresh, last_vblank, pending_for
            );
        } else {
            println!("presentation: pending={} outputs=0", pending);
        }
    }

    if let Some(output_mgmt) = status.get("output_management").filter(|v| !v.is_null()) {
        let pending = output_mgmt
            .get("pending_ack_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let soft_disabled = output_mgmt
            .get("soft_disabled_outputs")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let last = output_mgmt.get("last_transaction");
        match last {
            Some(tx) if !tx.is_null() => {
                let id = tx.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                let success = tx.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
                let failures = tx
                    .get("failed_outputs")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                let rollback = tx
                    .get("rollback_attempted")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let first_failure = tx
                    .get("failed_outputs")
                    .and_then(|v| v.as_array())
                    .and_then(|failures| failures.first());
                let failure_detail = first_failure
                    .map(|failure| {
                        let output = failure
                            .get("output_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let field = failure
                            .get("field")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let prop = failure
                            .get("drm_property")
                            .and_then(|v| v.as_str())
                            .unwrap_or("-");
                        let value = failure
                            .get("requested_value")
                            .and_then(|v| v.as_str())
                            .unwrap_or("-");
                        format!("{output}:{field}/{prop}={value}")
                    })
                    .unwrap_or_else(|| "none".into());
                let before_outputs = tx
                    .get("outputs_before")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                let after_outputs = tx
                    .get("outputs_after")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                let before_enabled = tx
                    .get("outputs_before")
                    .and_then(|v| v.as_array())
                    .map(|outputs| {
                        outputs
                            .iter()
                            .filter(|output| {
                                output.get("enabled").and_then(|v| v.as_bool()) == Some(true)
                            })
                            .count()
                    })
                    .unwrap_or(0);
                let after_enabled = tx
                    .get("outputs_after")
                    .and_then(|v| v.as_array())
                    .map(|outputs| {
                        outputs
                            .iter()
                            .filter(|output| {
                                output.get("enabled").and_then(|v| v.as_bool()) == Some(true)
                            })
                            .count()
                    })
                    .unwrap_or(0);
                println!(
                    "output_mgmt: pending_ack={} soft_disabled={} last_tx={} success={} failures={} first_failure={} rollback_attempted={} outputs={}/{} enabled={}/{}",
                    pending,
                    soft_disabled,
                    id,
                    success,
                    failures,
                    failure_detail,
                    rollback,
                    before_outputs,
                    after_outputs,
                    before_enabled,
                    after_enabled
                );
            }
            _ => println!(
                "output_mgmt: pending_ack={} soft_disabled={} last_tx=none",
                pending, soft_disabled
            ),
        }
        if let Some(rejected) = output_mgmt.get("last_rejected").filter(|v| !v.is_null()) {
            let action = rejected
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let output = rejected
                .get("output_name")
                .and_then(|v| v.as_str())
                .unwrap_or("*");
            let field = rejected
                .get("field")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let prop = rejected
                .get("drm_property")
                .and_then(|v| v.as_str())
                .unwrap_or("-");
            let value = rejected
                .get("requested_value")
                .and_then(|v| v.as_str())
                .unwrap_or("-");
            let reason = rejected
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            println!(
                "output_mgmt_rejected: action={} output={} field={} drm_property={} requested={} reason={}",
                action, output, field, prop, value, reason
            );
        }
    }

    if let Some(capture) = status.get("capture").filter(|v| !v.is_null()) {
        let screencopy_pending = capture
            .get("screencopy")
            .and_then(|v| v.get("pending_frames"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let image_pending = capture
            .get("image_copy_capture")
            .and_then(|v| v.get("pending_frames"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let dmabuf = capture
            .get("dmabuf_advertised")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let formats = capture
            .get("dmabuf_format_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let policy = capture
            .get("policy")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let last_failure = capture
            .get("last_failure_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("none");
        let queued_total = capture
            .get("screencopy_queued_total")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            + capture
                .get("image_copy_queued_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        let failed_total = capture
            .get("screencopy_failed_total")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            + capture
                .get("image_copy_failed_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        let fulfilled_total = capture
            .get("screencopy_fulfilled_total")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            + capture
                .get("image_copy_fulfilled_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        let render_failed_total = capture
            .get("screencopy_render_failed_total")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            + capture
                .get("image_copy_render_failed_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        println!(
            "capture: screencopy_pending={} image_pending={} queued_total={} fulfilled_total={} failed_total={} render_failed_total={} dmabuf={} formats={} policy={} last_failure=\"{}\"",
            screencopy_pending,
            image_pending,
            queued_total,
            fulfilled_total,
            failed_total,
            render_failed_total,
            dmabuf,
            formats,
            policy,
            last_failure
        );
    }

    if let Some(scrolling) = status.get("scrolling").filter(|v| !v.is_null()) {
        let active = scrolling
            .get("active_monitor_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let stored = scrolling
            .get("stored_state_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let width_rules = scrolling
            .get("column_width_rule_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let stored_tags = scrolling
            .get("stored_states")
            .and_then(|v| v.as_array())
            .map(|states| {
                states
                    .iter()
                    .filter_map(|state| {
                        let monitor = state
                            .get("monitor")
                            .and_then(|v| v.as_i64())
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "?".into());
                        let tag_mask = state.get("tag_mask").and_then(|v| v.as_u64())?;
                        Some(format!("{}:{:#x}", monitor, tag_mask))
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "none".into());
        let first_active = scrolling
            .get("monitors")
            .and_then(|v| v.as_array())
            .and_then(|monitors| {
                monitors.iter().find(|monitor| {
                    monitor
                        .get("active")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                })
            });
        if let Some(monitor) = first_active {
            let mon_num = monitor.get("monitor").and_then(|v| v.as_u64()).unwrap_or(0);
            let columns = monitor
                .get("column_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let focused_column = monitor
                .get("focused_column")
                .and_then(|v| v.as_u64())
                .map(|v| v.to_string())
                .unwrap_or_else(|| "none".into());
            let viewport = monitor
                .get("viewport_x")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let strip_columns = monitor
                .get("overview_strip")
                .and_then(|v| v.get("column_count"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let strip_focus = monitor
                .get("overview_strip")
                .and_then(|v| v.get("focused_column"))
                .and_then(|v| v.as_u64())
                .map(|v| v.to_string())
                .unwrap_or_else(|| "none".into());
            let overview_order = monitor
                .get("overview_strip")
                .and_then(|v| v.get("overview_order"))
                .and_then(|v| v.as_array())
                .map(|v| v.len())
                .unwrap_or(0);
            println!(
                "scrolling: active_monitors={} stored_states={} width_rules={} stored_tags={} first_monitor={} columns={} focused_column={} viewport_x={:.1} strip_columns={} strip_focus={} overview_order={}",
                active,
                stored,
                width_rules,
                stored_tags,
                mon_num,
                columns,
                focused_column,
                viewport,
                strip_columns,
                strip_focus,
                overview_order
            );
        } else {
            println!(
                "scrolling: active_monitors={} stored_states={} width_rules={} stored_tags={}",
                active, stored, width_rules, stored_tags
            );
        }
    }

    if let Some(gestures) = status.get("gestures").filter(|v| !v.is_null()) {
        let threshold = gestures
            .get("swipe_threshold")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let bindings = gestures
            .get("binding_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let scrolling_bindings = gestures
            .get("scrolling_binding_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let fingers = gestures
            .get("intercepted_fingers")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_u64())
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "none".into());
        let missing_recommended = gestures
            .get("recommended_scrolling_swipes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|entry| {
                        !entry
                            .get("configured")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0);
        println!(
            "gestures: swipe_bindings={} scrolling_bindings={} intercepted_fingers={} threshold={:.1} missing_recommended_scrolling={}",
            bindings, scrolling_bindings, fingers, threshold, missing_recommended
        );
    }

    if let Some(hdr) = status.get("hdr") {
        let enabled = hdr
            .get("config_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let capable = hdr
            .get("capable_output_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        println!(
            "hdr: config_enabled={} capable_outputs={}",
            enabled, capable
        );
    }

    if let Some(protocols) = status.get("protocols") {
        let optional = protocols
            .get("optional")
            .and_then(|v| v.as_array())
            .map(|a| {
                let enabled = a
                    .iter()
                    .filter(|p| p.get("enabled").and_then(|v| v.as_bool()) == Some(true))
                    .count();
                let config_enabled = a
                    .iter()
                    .filter(|p| p.get("config_enabled").and_then(|v| v.as_bool()) == Some(true))
                    .count();
                let env_enabled = a
                    .iter()
                    .filter(|p| p.get("env_enabled").and_then(|v| v.as_bool()) == Some(true))
                    .count();
                (enabled, a.len(), config_enabled, env_enabled)
            })
            .unwrap_or((0, 0, 0, 0));
        println!(
            "optional_protocols: enabled={}/{} config_enabled={} env_enabled={}",
            optional.0, optional.1, optional.2, optional.3
        );

        if let Some(catalog) = protocols.get("catalog").and_then(|v| v.as_array()) {
            let published = catalog
                .iter()
                .filter(|p| p.get("published").and_then(|v| v.as_bool()) == Some(true))
                .count();
            let tracked = catalog
                .iter()
                .filter(|p| p.get("bind_count_tracked").and_then(|v| v.as_bool()) == Some(true))
                .count();
            let bound = catalog
                .iter()
                .filter(|p| p.get("bind_count").and_then(|v| v.as_u64()).unwrap_or(0) > 0)
                .count();
            let total_binds: u64 = catalog
                .iter()
                .filter_map(|p| p.get("bind_count").and_then(|v| v.as_u64()))
                .sum();
            let busiest = catalog
                .iter()
                .filter_map(|p| {
                    Some((
                        p.get("name")?.as_str()?,
                        p.get("bind_count").and_then(|v| v.as_u64()).unwrap_or(0),
                    ))
                })
                .max_by_key(|(_, count)| *count)
                .filter(|(_, count)| *count > 0)
                .map(|(name, count)| format!("{}={}", name, count))
                .unwrap_or_else(|| "none".into());
            let latest = catalog
                .iter()
                .filter_map(|p| {
                    Some((
                        p.get("name")?.as_str()?,
                        p.get("last_bound_unix_ms")?.as_u64()?,
                    ))
                })
                .max_by_key(|(_, ts)| *ts)
                .map(|(name, ts)| format!("{}@{}", name, ts))
                .unwrap_or_else(|| "none".into());
            println!(
                "protocol_binds: published={} tracked={} bound={} total_binds={} busiest={} latest={}",
                published, tracked, bound, total_binds, busiest, latest
            );
        }
    }

    if let Some(tearing) = status.get("tearing") {
        let count = tearing
            .get("active_surface_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        println!("tearing_hints: active_surface_count={}", count);
    }

    if let Some(lock) = status.get("session_lock") {
        let locked = lock
            .get("locked")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let surfaces = lock
            .get("lock_surface_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        println!("session_lock: locked={} surfaces={}", locked, surfaces);
    }

    if let Some(xwayland) = status.get("xwayland").filter(|v| !v.is_null()) {
        let available = xwayland
            .get("available")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let wm_ready = xwayland
            .get("wm_ready")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let display = xwayland
            .get("display")
            .and_then(|v| v.as_str())
            .unwrap_or("none");
        let mapped = xwayland
            .get("mapped_window_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let pending = xwayland
            .get("pending_association_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        println!(
            "xwayland: available={} wm_ready={} display={} mapped_windows={} pending_assoc={}",
            available, wm_ready, display, mapped, pending
        );
    }

    if let Some(color) = status.get("color_management") {
        let count = color
            .get("surface_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let advanced = color
            .get("advanced_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let render_path = color
            .get("render_path_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let output_count = color
            .get("output_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let hdr_surfaces = color
            .get("hdr_surface_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let transfer_functions = color
            .get("transfer_functions")
            .and_then(|v| v.as_object())
            .map(|counts| {
                counts
                    .iter()
                    .map(|(name, count)| format!("{}:{}", name, count.as_u64().unwrap_or(0)))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "none".into());
        let primaries = color
            .get("primaries")
            .and_then(|v| v.as_object())
            .map(|counts| {
                counts
                    .iter()
                    .map(|(name, count)| format!("{}:{}", name, count.as_u64().unwrap_or(0)))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "none".into());
        let max_lum = color
            .get("max_luminance_peak")
            .and_then(|v| v.as_u64())
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".into());
        let policy = color
            .get("session_policy")
            .filter(|v| !v.is_null())
            .map(|policy| {
                let sdr_on_hdr = policy
                    .get("sdr_on_hdr_policy")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let mixed = policy
                    .get("mixed_hdr_policy")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let hdr_outputs = policy
                    .get("hdr_output_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let sdr_outputs = policy
                    .get("sdr_output_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                format!(
                    "sdr_on_hdr={} mixed={} hdr_outputs={} sdr_outputs={}",
                    sdr_on_hdr, mixed, hdr_outputs, sdr_outputs
                )
            })
            .unwrap_or_else(|| "unknown".into());
        let first_surface = color
            .get("surface_samples")
            .and_then(|v| v.as_array())
            .and_then(|surfaces| surfaces.first())
            .map(|surface| {
                let id = surface
                    .get("surface_object_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let tf = surface
                    .get("transfer_function")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let prim = surface
                    .get("primaries")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let hdr = surface
                    .get("hdr")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                format!("{id}:{tf}/{prim}/hdr={hdr}")
            })
            .unwrap_or_else(|| "none".into());
        let shader_fallback_outputs = status
            .get("outputs")
            .and_then(|v| v.as_array())
            .map(|outputs| {
                outputs
                    .iter()
                    .filter(|output| {
                        output
                            .get("color_management")
                            .and_then(|v| v.get("shader_fallback_required"))
                            .and_then(|v| v.as_bool())
                            == Some(true)
                    })
                    .count()
            })
            .unwrap_or(0);
        println!(
            "color_management: surfaces={} hdr_surfaces={} advanced={} render_path={} outputs={} shader_fallback_outputs={} tf=[{}] primaries=[{}] max_lum={} policy=[{}] first_surface={}",
            count,
            hdr_surfaces,
            advanced,
            render_path,
            output_count,
            shader_fallback_outputs,
            transfer_functions,
            primaries,
            max_lum,
            policy,
            first_surface
        );
    }

    match status.get("blur").filter(|v| !v.is_null()) {
        Some(blur) => {
            let strength = blur
                .get("current_strength")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let reuse = blur
                .get("temporal_reuse_rate_pct")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            println!("blur: strength={} temporal_reuse={:.1}%", strength, reuse);
        }
        None => println!("blur: unavailable"),
    }
}

fn run_wayland_status(json_output: bool) -> io::Result<()> {
    match send_ipc_query("get_wayland_status") {
        Ok(resp) if resp.get("success").and_then(|v| v.as_bool()) == Some(true) => {
            let status = resp.get("data").cloned().unwrap_or(serde_json::Value::Null);
            let snapshot = serde_json::json!({
                "generated_at": Local::now().to_rfc3339(),
                "socket": ipc_socket_path(),
                "wayland_status": status,
            });

            if json_output {
                println!("{}", serde_json::to_string_pretty(&snapshot).unwrap());
            } else {
                print_unified_wayland_status(&snapshot["wayland_status"]);
            }
            return Ok(());
        }
        Ok(_) => {
            // Connected to an older compositor that does not know
            // get_wayland_status yet; fall through to legacy query fan-out.
        }
        Err(e) => {
            let snapshot = serde_json::json!({
                "generated_at": Local::now().to_rfc3339(),
                "socket": ipc_socket_path(),
                "success": false,
                "error": e.to_string(),
            });
            if json_output {
                println!("{}", serde_json::to_string_pretty(&snapshot).unwrap());
            } else {
                println!("=== JWM Wayland Status ===");
                println!("version: unknown");
                println!("socket: {}", ipc_socket_path().display());
                println!("ipc: unavailable ({})", e);
            }
            return Ok(());
        }
    }

    let queries = [
        "get_version",
        "get_monitors",
        "get_workspaces",
        "get_windows",
        "get_scrolling_status",
        "get_gesture_status",
        "get_config_status",
        "get_metrics",
        "get_hdr_status",
        "get_tearing_hints",
        "get_session_lock",
        "get_xwayland_status",
        "get_color_management_status",
        "get_capture_status",
        "get_blur_status",
    ];

    let mut responses = serde_json::Map::new();
    for query in queries {
        let value = match send_ipc_query(query) {
            Ok(value) => value,
            Err(e) => serde_json::json!({
                "success": false,
                "error": e.to_string(),
            }),
        };
        responses.insert(query.to_string(), value);
    }

    let snapshot = serde_json::json!({
        "generated_at": Local::now().to_rfc3339(),
        "socket": ipc_socket_path(),
        "queries": responses,
    });

    if json_output {
        println!("{}", serde_json::to_string_pretty(&snapshot).unwrap());
        return Ok(());
    }

    let queries = &snapshot["queries"];
    let version = response_data(queries, "get_version")
        .and_then(|v| v.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    println!("=== JWM Wayland Status ===");
    println!("version: {}", version);
    println!("socket: {}", ipc_socket_path().display());

    let successful_queries = queries
        .as_object()
        .map(|m| {
            m.values()
                .filter(|v| v.get("success").and_then(|v| v.as_bool()) == Some(true))
                .count()
        })
        .unwrap_or(0);
    if successful_queries == 0 {
        let first_error = queries
            .as_object()
            .and_then(|m| m.values().find_map(|v| v.get("error")))
            .and_then(|v| v.as_str())
            .unwrap_or("no successful IPC queries");
        println!("ipc: unavailable ({})", first_error);
        return Ok(());
    }

    println!("monitors: {}", response_array_len(queries, "get_monitors"));
    println!(
        "workspaces: {}",
        response_array_len(queries, "get_workspaces")
    );
    println!("windows: {}", response_array_len(queries, "get_windows"));

    if let Some(config) = response_data(queries, "get_config_status") {
        let path = config
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let exists = config
            .get("exists")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let reload = config.get("reload").unwrap_or(&serde_json::Value::Null);
        let attempts = reload
            .get("attempt_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let last_success = reload
            .get("last_success")
            .and_then(|v| v.as_bool())
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".into());
        let last_error = reload
            .get("last_error")
            .and_then(|v| v.as_str())
            .unwrap_or("none");
        println!(
            "config: path={} exists={} reload_attempts={} last_success={} last_error=\"{}\"",
            path, exists, attempts, last_success, last_error
        );
    }

    if let Some(scrolling) = response_data(queries, "get_scrolling_status") {
        let active = scrolling
            .get("active_monitor_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let stored = scrolling
            .get("stored_state_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        println!(
            "scrolling: active_monitors={} stored_states={}",
            active, stored
        );
    }

    if let Some(gestures) = response_data(queries, "get_gesture_status") {
        let bindings = gestures
            .get("binding_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let scrolling_bindings = gestures
            .get("scrolling_binding_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let missing_recommended = gestures
            .get("recommended_scrolling_swipes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|entry| {
                        !entry
                            .get("configured")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0);
        println!(
            "gestures: swipe_bindings={} scrolling_bindings={} missing_recommended_scrolling={}",
            bindings, scrolling_bindings, missing_recommended
        );
    }

    if let Some(metrics) = response_data(queries, "get_metrics") {
        let fps = metrics.get("fps").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let input_p95 = metrics
            .get("input_latency_p95_ms")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let direct_scanout = metrics
            .get("direct_scanout_active")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        println!(
            "metrics: fps={:.1} input_p95_ms={:.2} direct_scanout={}",
            fps, input_p95, direct_scanout
        );
    }

    if let Some(hdr) = response_data(queries, "get_hdr_status") {
        let enabled = hdr
            .get("config_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let hdr_outputs = hdr
            .get("outputs")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|o| {
                        o.get("hdr_capable")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0);
        println!(
            "hdr: config_enabled={} capable_outputs={}",
            enabled, hdr_outputs
        );
    }

    if let Some(tearing) = response_data(queries, "get_tearing_hints") {
        let count = tearing
            .get("active_surface_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        println!("tearing_hints: active_surface_count={}", count);
    }

    if let Some(lock) = response_data(queries, "get_session_lock") {
        let locked = lock
            .get("locked")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let surfaces = lock
            .get("lock_surface_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        println!("session_lock: locked={} surfaces={}", locked, surfaces);
    }

    if let Some(color) = response_data(queries, "get_color_management_status") {
        let count = color
            .get("surface_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let hdr_surfaces = color
            .get("hdr_surface_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let transfer_functions = color
            .get("transfer_functions")
            .and_then(|v| v.as_object())
            .map(|counts| {
                counts
                    .iter()
                    .map(|(name, count)| format!("{}:{}", name, count.as_u64().unwrap_or(0)))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "none".into());
        println!(
            "color_management: surfaces={} hdr_surfaces={} tf=[{}]",
            count, hdr_surfaces, transfer_functions
        );
    }

    if let Some(capture) = response_data(queries, "get_capture_status") {
        let screencopy_pending = capture
            .get("screencopy")
            .and_then(|v| v.get("pending_frames"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let image_pending = capture
            .get("image_copy_capture")
            .and_then(|v| v.get("pending_frames"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let dmabuf = capture
            .get("dmabuf_advertised")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let queued_total = capture
            .get("screencopy_queued_total")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            + capture
                .get("image_copy_queued_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
        println!(
            "capture: screencopy_pending={} image_pending={} queued_total={} dmabuf={}",
            screencopy_pending, image_pending, queued_total, dmabuf
        );
    }

    match response_data(queries, "get_blur_status") {
        Some(blur) => {
            let strength = blur
                .get("current_strength")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let reuse = blur
                .get("temporal_reuse_rate_pct")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            println!("blur: strength={} temporal_reuse={:.1}%", strength, reuse);
        }
        None => {
            let err = queries
                .get("get_blur_status")
                .and_then(|v| v.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("unavailable");
            println!("blur: {}", err);
        }
    }

    Ok(())
}

fn read_ipc_line(stream: &mut UnixStream) -> io::Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match io::Read::read(stream, &mut byte) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed",
                ));
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    return Ok(String::from_utf8_lossy(&buf).to_string());
                }
                buf.push(byte[0]);
            }
            Err(e) => return Err(e),
        }
    }
}
