// src/miscellaneous.rs
use log::{error, info};
use std::process::{Command, Stdio};

use crate::terminal_prober::ADVANCED_TERMINAL_PROBER;

pub fn init_auto_command() {
    // 仅仅保留终端检测日志，这对于调试很有用
    let prober = &*ADVANCED_TERMINAL_PROBER;
    if let Some(terminal) = prober.get_available_terminal() {
        info!("Found terminal: {}", terminal.command);
    } else {
        error!("No terminal found!");
    }
}

pub fn init_auto_start() {
    // 1. 确定 autostart.sh 的路径
    // 优先查找 XDG_CONFIG_HOME/jwm/autostart.sh (通常是 ~/.config/jwm/autostart.sh)
    let config_path = dirs::config_dir()
        .map(|p| p.join("jwm").join("autostart.sh"))
        .or_else(|| {
            // 回退方案：尝试找 ~/.jwm/autostart.sh
            dirs::home_dir().map(|p| p.join(".jwm").join("autostart.sh"))
        });

    let Some(path) = config_path else {
        error!("Could not determine configuration directory.");
        return;
    };

    if path.exists() {
        info!("Found autostart script at: {}", path.display());

        // 2. 执行脚本
        // 使用 "sh" 来执行，这样即使用户忘记 chmod +x 也能运行
        match Command::new("sh")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => info!("Autostart script spawned successfully"),
            Err(e) => error!("Failed to execute autostart script: {e}"),
        }
    } else {
        info!(
            "No autostart script found at {}, skipping auto start tasks.",
            path.display()
        );
    }
}

fn path_has_executable(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(bin);
        candidate.is_file()
            && std::fs::metadata(&candidate)
                .map(|m| {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        m.permissions().mode() & 0o111 != 0
                    }
                    #[cfg(not(unix))]
                    {
                        true
                    }
                })
                .unwrap_or(false)
    })
}

pub fn ensure_restart_input_method() {
    if !path_has_executable("fcitx5") {
        info!("fcitx5 not found in PATH, skipping restart IM bootstrap");
        return;
    }

    let already_running = Command::new("pgrep")
        .args(["-x", "fcitx5"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if already_running {
        info!("fcitx5 already running on restart, skipping bootstrap");
        return;
    }

    match Command::new("fcitx5")
        .arg("-d")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => info!("Spawned fcitx5 for restart session"),
        Err(e) => error!("Failed to spawn fcitx5 on restart: {e}"),
    }
}
