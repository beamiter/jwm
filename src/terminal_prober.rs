use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::env;
use std::process::Command;
use std::sync::RwLock;

// ---------------------------------------------------------------------------
// Session type detection
// ---------------------------------------------------------------------------

/// Returns true if running in a Wayland session (no X11 display available).
/// Used by both terminal and launcher probers to pick the right tool set.
pub fn is_wayland_session() -> bool {
    let has_wayland = env::var("WAYLAND_DISPLAY").is_ok()
        || env::var("XDG_SESSION_TYPE")
            .map(|v| v.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false);
    let has_display = env::var("DISPLAY").is_ok();
    // Pure Wayland: WAYLAND_DISPLAY set and no X11 DISPLAY (or explicitly wayland type)
    has_wayland && !has_display
        || env::var("XDG_SESSION_TYPE")
            .map(|v| v.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Launcher prober (for Alt+r / dmenu-like spawn)
// ---------------------------------------------------------------------------

pub struct LauncherProber {
    /// Ordered list of (binary, extra_args) to try in Wayland sessions.
    wayland_candidates: Vec<(&'static str, Vec<&'static str>)>,
    /// Ordered list of (binary, extra_args) to try in X11 sessions.
    x11_candidates: Vec<(&'static str, Vec<&'static str>)>,
    cache: RwLock<HashMap<String, bool>>,
}

impl LauncherProber {
    fn new() -> Self {
        Self {
            wayland_candidates: vec![
                ("fuzzel", vec![]),
                ("wofi",   vec!["--show", "run"]),
                ("tofi-run", vec![]),
                ("tofi",   vec!["--prompt", "run: "]),
                ("bemenu-run", vec![]),
                // rofi with wayland backend works via -display-backend wayland
                ("rofi",   vec!["-show", "run"]),
                // last resort: dmenu_run via XWayland
                ("dmenu_run", vec![]),
            ],
            x11_candidates: vec![
                ("dmenu_run", vec![]),
                ("rofi",      vec!["-show", "run"]),
                ("gmrun",     vec![]),
                ("xfce4-appfinder", vec!["--collapsed"]),
                // Wayland-native launchers still work on X11 if installed
                ("fuzzel",    vec![]),
                ("wofi",      vec!["--show", "run"]),
            ],
            cache: RwLock::new(HashMap::new()),
        }
    }

    fn is_command_available(&self, cmd: &str) -> bool {
        {
            let r = self.cache.read().unwrap();
            if let Some(&v) = r.get(cmd) {
                return v;
            }
        }
        let result = Command::new("which")
            .arg(cmd)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        self.cache.write().unwrap().insert(cmd.to_string(), result);
        result
    }

    /// Probe for an available launcher and return it as a ready-to-spawn argv.
    /// Falls back to `["dmenu_run"]` if nothing is found.
    pub fn probe_launcher(&self) -> Vec<String> {
        let candidates = if is_wayland_session() {
            &self.wayland_candidates
        } else {
            &self.x11_candidates
        };
        for (bin, args) in candidates {
            if self.is_command_available(bin) {
                log::debug!("[LauncherProber] selected launcher: {}", bin);
                let mut cmd = vec![bin.to_string()];
                cmd.extend(args.iter().map(|a| a.to_string()));
                return cmd;
            }
        }
        log::warn!("[LauncherProber] no launcher found, falling back to dmenu_run");
        vec!["dmenu_run".to_string()]
    }

    pub fn clear_cache(&self) {
        self.cache.write().unwrap().clear();
    }
}

pub static LAUNCHER_PROBER: Lazy<LauncherProber> = Lazy::new(LauncherProber::new);

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TerminalConfig {
    pub command: String,
    pub execute_flag: String,
    pub title_flag: Option<String>,
    pub geometry_flag: Option<String>,
    pub working_dir_flag: Option<String>,
}

pub struct AdvancedTerminalProber {
    configs: HashMap<String, TerminalConfig>,
    priority_order: Vec<String>,
    cache: RwLock<HashMap<String, bool>>,
}

impl AdvancedTerminalProber {
    fn new() -> Self {
        let mut configs = HashMap::new();

        // terminal_emulator - preferred terminal
        configs.insert(
            "terminal_emulator".to_string(),
            TerminalConfig {
                command: "terminal_emulator".to_string(),
                execute_flag: "-e".to_string(),
                title_flag: None,
                geometry_flag: None,
                working_dir_flag: None,
            },
        );

        // Wayland-first terminals (often needed for DRM/udev compositors)
        configs.insert(
            "foot".to_string(),
            TerminalConfig {
                command: "foot".to_string(),
                execute_flag: "-e".to_string(),
                title_flag: Some("-T".to_string()),
                geometry_flag: None,
                working_dir_flag: Some("-D".to_string()),
            },
        );

        configs.insert(
            "wezterm".to_string(),
            TerminalConfig {
                command: "wezterm".to_string(),
                execute_flag: "start".to_string(),
                title_flag: Some("--class".to_string()),
                geometry_flag: None,
                working_dir_flag: Some("--cwd".to_string()),
            },
        );

        configs.insert(
            "alacritty".to_string(),
            TerminalConfig {
                command: "alacritty".to_string(),
                execute_flag: "-e".to_string(),
                title_flag: Some("--title".to_string()),
                geometry_flag: None,
                working_dir_flag: Some("--working-directory".to_string()),
            },
        );

        configs.insert(
            "kitty".to_string(),
            TerminalConfig {
                command: "kitty".to_string(),
                execute_flag: "--".to_string(),
                title_flag: Some("--title".to_string()),
                geometry_flag: Some("--geometry".to_string()),
                working_dir_flag: Some("--directory".to_string()),
            },
        );

        configs.insert(
            "weston-terminal".to_string(),
            TerminalConfig {
                command: "weston-terminal".to_string(),
                execute_flag: "--".to_string(),
                title_flag: None,
                geometry_flag: None,
                working_dir_flag: None,
            },
        );

        // Warp Terminal
        configs.insert(
            "warp-terminal".to_string(),
            TerminalConfig {
                command: "warp-terminal".to_string(),
                execute_flag: "-e".to_string(),
                title_flag: Some("--title".to_string()),
                geometry_flag: None,
                working_dir_flag: Some("--working-directory".to_string()),
            },
        );

        // Terminator
        configs.insert(
            "terminator".to_string(),
            TerminalConfig {
                command: "terminator".to_string(),
                execute_flag: "-e".to_string(),
                title_flag: Some("-T".to_string()),
                geometry_flag: Some("-g".to_string()),
                working_dir_flag: Some("--working-directory".to_string()),
            },
        );

        // GNOME Terminal
        configs.insert(
            "gnome-terminal".to_string(),
            TerminalConfig {
                command: "gnome-terminal".to_string(),
                execute_flag: "--".to_string(),
                title_flag: Some("--title".to_string()),
                geometry_flag: Some("--geometry".to_string()),
                working_dir_flag: Some("--working-directory".to_string()),
            },
        );

        // JTerm4
        configs.insert(
            "jterm4".to_string(),
            TerminalConfig {
                command: "jterm4".to_string(),
                execute_flag: "-e".to_string(),
                title_flag: Some("--title".to_string()),
                geometry_flag: None,
                working_dir_flag: Some("--workdir".to_string()),
            },
        );

        // Choose priority based on session hints.
        // - In udev/DRM (Wayland compositor) sessions, X11 terminals often won't show.
        // - In X11 sessions, Warp/Terminator/Gnome-terminal are usually fine.
        let priority_order = if is_wayland_session() {
            vec![
                "terminal_emulator".to_string(),
                "jterm4".to_string(),
                "foot".to_string(),
                "wezterm".to_string(),
                "alacritty".to_string(),
                "kitty".to_string(),
                "weston-terminal".to_string(),
                // Keep Warp last: it may depend on X11/desktop services.
                "warp-terminal".to_string(),
                "terminator".to_string(),
                "gnome-terminal".to_string(),
            ]
        } else {
            vec![
                "terminal_emulator".to_string(),
                "jterm4".to_string(),
                "warp-terminal".to_string(),
                "terminator".to_string(),
                "gnome-terminal".to_string(),
                "alacritty".to_string(),
                "kitty".to_string(),
                "wezterm".to_string(),
                "foot".to_string(),
                "weston-terminal".to_string(),
            ]
        };

        Self {
            configs,
            priority_order,
            cache: RwLock::new(HashMap::new()),
        }
    }

    pub fn get_available_terminal(&self) -> Option<&TerminalConfig> {
        for terminal_name in &self.priority_order {
            if let Some(config) = self.configs.get(terminal_name) {
                if self.is_command_available(&config.command) {
                    println!("[get_available_terminal] {:?}", config);
                    return Some(config);
                }
            }
        }
        None
    }

    pub fn get_available_terminal_with_priority(
        &self,
        preferred: Option<&str>,
    ) -> Option<&TerminalConfig> {
        // If a preferred terminal is specified and available, use it first
        if let Some(pref) = preferred {
            if let Some(config) = self.configs.get(pref) {
                if self.is_command_available(&config.command) {
                    println!(
                        "[get_available_terminal_with_priority] Using preferred terminal: {:?}",
                        config
                    );
                    return Some(config);
                }
            }
        }
        // Fall back to the default priority order
        self.get_available_terminal()
    }

    fn is_command_available(&self, cmd: &str) -> bool {
        {
            let cache_reader = self.cache.read().unwrap();
            if let Some(&cached_result) = cache_reader.get(cmd) {
                return cached_result;
            }
        }
        let result = self.check_command_exists(cmd);
        {
            let mut cache_writer = self.cache.write().unwrap();
            cache_writer.insert(cmd.to_string(), result);
        }
        result
    }

    fn check_command_exists(&self, cmd: &str) -> bool {
        Command::new("which")
            .arg(cmd)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    #[allow(dead_code)]
    pub fn build_command(
        &self,
        command: &str,
        title: Option<&str>,
        working_dir: Option<&str>,
    ) -> Option<Vec<String>> {
        let config = self.get_available_terminal()?;
        let mut cmd = vec![config.command.clone()];
        if let (Some(title), Some(title_flag)) = (title, &config.title_flag) {
            cmd.push(title_flag.clone());
            cmd.push(title.to_string());
        }
        if let (Some(working_dir), Some(dir_flag)) = (working_dir, &config.working_dir_flag) {
            cmd.push(dir_flag.clone());
            cmd.push(working_dir.to_string());
        }
        cmd.push(config.execute_flag.clone());
        cmd.push(command.to_string());
        Some(cmd)
    }

    #[allow(dead_code)]
    pub fn clear_cache(&self) {
        let mut cache_writer = self.cache.write().unwrap();
        cache_writer.clear();
    }
}

pub static ADVANCED_TERMINAL_PROBER: Lazy<AdvancedTerminalProber> =
    Lazy::new(|| AdvancedTerminalProber::new());
