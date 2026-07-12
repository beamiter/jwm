use crate::sync_ext::RwLockExt;
use std::collections::HashMap;
use std::env;
use std::process::Command;
use std::sync::{LazyLock, RwLock};

fn command_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .is_ok_and(|output| output.status.success())
}

// ---------------------------------------------------------------------------
// Session type detection
// ---------------------------------------------------------------------------

/// Returns true if running in a Wayland session (no X11 display available).
/// Used by the terminal prober to pick the right tool set.
#[must_use]
pub fn is_wayland_session() -> bool {
    let has_wayland = env::var("WAYLAND_DISPLAY").is_ok()
        || env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"));
    let has_display = env::var("DISPLAY").is_ok();
    // Pure Wayland: WAYLAND_DISPLAY set and no X11 DISPLAY (or explicitly wayland type)
    has_wayland && !has_display
        || env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"))
}

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

struct TerminalDefinition {
    name: &'static str,
    command: &'static str,
    execute_flag: &'static str,
    title_flag: Option<&'static str>,
    geometry_flag: Option<&'static str>,
    working_dir_flag: Option<&'static str>,
}

impl TerminalDefinition {
    fn config(&self) -> TerminalConfig {
        TerminalConfig {
            command: self.command.to_string(),
            execute_flag: self.execute_flag.to_string(),
            title_flag: self.title_flag.map(str::to_string),
            geometry_flag: self.geometry_flag.map(str::to_string),
            working_dir_flag: self.working_dir_flag.map(str::to_string),
        }
    }
}

const TERMINAL_DEFINITIONS: &[TerminalDefinition] = &[
    TerminalDefinition {
        name: "terminal_emulator",
        command: "terminal_emulator",
        execute_flag: "-e",
        title_flag: None,
        geometry_flag: None,
        working_dir_flag: None,
    },
    TerminalDefinition {
        name: "foot",
        command: "foot",
        execute_flag: "-e",
        title_flag: Some("-T"),
        geometry_flag: None,
        working_dir_flag: Some("-D"),
    },
    TerminalDefinition {
        name: "wezterm",
        command: "wezterm",
        execute_flag: "start",
        title_flag: Some("--class"),
        geometry_flag: None,
        working_dir_flag: Some("--cwd"),
    },
    TerminalDefinition {
        name: "alacritty",
        command: "alacritty",
        execute_flag: "-e",
        title_flag: Some("--title"),
        geometry_flag: None,
        working_dir_flag: Some("--working-directory"),
    },
    TerminalDefinition {
        name: "kitty",
        command: "kitty",
        execute_flag: "--",
        title_flag: Some("--title"),
        geometry_flag: Some("--geometry"),
        working_dir_flag: Some("--directory"),
    },
    TerminalDefinition {
        name: "weston-terminal",
        command: "weston-terminal",
        execute_flag: "--",
        title_flag: None,
        geometry_flag: None,
        working_dir_flag: None,
    },
    TerminalDefinition {
        name: "warp-terminal",
        command: "warp-terminal",
        execute_flag: "-e",
        title_flag: Some("--title"),
        geometry_flag: None,
        working_dir_flag: Some("--working-directory"),
    },
    TerminalDefinition {
        name: "terminator",
        command: "terminator",
        execute_flag: "-e",
        title_flag: Some("-T"),
        geometry_flag: Some("-g"),
        working_dir_flag: Some("--working-directory"),
    },
    TerminalDefinition {
        name: "gnome-terminal",
        command: "gnome-terminal",
        execute_flag: "--",
        title_flag: Some("--title"),
        geometry_flag: Some("--geometry"),
        working_dir_flag: Some("--working-directory"),
    },
    TerminalDefinition {
        name: "jterm4",
        command: "jterm4",
        execute_flag: "-e",
        title_flag: Some("--title"),
        geometry_flag: None,
        working_dir_flag: Some("--workdir"),
    },
];

const WAYLAND_TERMINAL_PRIORITY: &[&str] = &[
    "terminal_emulator",
    "jterm4",
    "foot",
    "wezterm",
    "alacritty",
    "kitty",
    "weston-terminal",
    // Keep Warp last: it may depend on X11/desktop services.
    "warp-terminal",
    "terminator",
    "gnome-terminal",
];

const X11_TERMINAL_PRIORITY: &[&str] = &[
    "terminal_emulator",
    "jterm4",
    "warp-terminal",
    "terminator",
    "gnome-terminal",
    "alacritty",
    "kitty",
    "wezterm",
    "foot",
    "weston-terminal",
];

impl AdvancedTerminalProber {
    fn new() -> Self {
        let configs = TERMINAL_DEFINITIONS
            .iter()
            .map(|definition| (definition.name.to_string(), definition.config()))
            .collect();

        // Choose priority based on session hints.
        // - In udev/DRM (Wayland compositor) sessions, X11 terminals often won't show.
        // - In X11 sessions, Warp/Terminator/Gnome-terminal are usually fine.
        let priority = if is_wayland_session() {
            WAYLAND_TERMINAL_PRIORITY
        } else {
            X11_TERMINAL_PRIORITY
        };
        let priority_order = priority.iter().map(|name| (*name).to_string()).collect();

        Self {
            configs,
            priority_order,
            cache: RwLock::new(HashMap::new()),
        }
    }

    pub fn get_available_terminal(&self) -> Option<&TerminalConfig> {
        for terminal_name in &self.priority_order {
            if let Some(config) = self.configs.get(terminal_name)
                && self.is_command_available(&config.command)
            {
                log::debug!("[get_available_terminal] {config:?}");
                return Some(config);
            }
        }
        None
    }

    pub fn get_available_terminal_with_priority(
        &self,
        preferred: Option<&str>,
    ) -> Option<&TerminalConfig> {
        // If a preferred terminal is specified and available, use it first
        if let Some(pref) = preferred
            && let Some(config) = self.configs.get(pref)
            && self.is_command_available(&config.command)
        {
            log::debug!(
                "[get_available_terminal_with_priority] using preferred terminal: {config:?}"
            );
            return Some(config);
        }
        // Fall back to the default priority order
        self.get_available_terminal()
    }

    fn is_command_available(&self, cmd: &str) -> bool {
        {
            let cache_reader = self.cache.read_safe();
            if let Some(&cached_result) = cache_reader.get(cmd) {
                return cached_result;
            }
        }
        let result = command_exists(cmd);
        {
            let mut cache_writer = self.cache.write_safe();
            cache_writer.insert(cmd.to_string(), result);
        }
        result
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
        let mut cache_writer = self.cache.write_safe();
        cache_writer.clear();
    }
}

pub static ADVANCED_TERMINAL_PROBER: LazyLock<AdvancedTerminalProber> =
    LazyLock::new(AdvancedTerminalProber::new);
