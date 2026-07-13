use crate::sync_ext::RwLockExt;
use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::{LazyLock, RwLock};

fn command_exists(cmd: &str) -> bool {
    command_exists_in_path(cmd, env::var_os("PATH").as_deref())
}

fn command_exists_in_path(cmd: &str, path: Option<&OsStr>) -> bool {
    let command_path = Path::new(cmd);
    if command_path.components().count() > 1 {
        return is_executable(command_path);
    }
    path.is_some_and(|path| {
        env::split_paths(path).any(|directory| is_executable(&directory.join(command_path)))
    })
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
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
        name: "jterm1",
        command: "jterm1",
        execute_flag: "-e",
        title_flag: Some("--title"),
        geometry_flag: None,
        working_dir_flag: Some("--workdir"),
    },
    TerminalDefinition {
        name: "jterm2",
        command: "jterm2",
        execute_flag: "-e",
        title_flag: Some("--title"),
        geometry_flag: None,
        working_dir_flag: Some("--workdir"),
    },
    TerminalDefinition {
        name: "jterm3",
        command: "jterm3",
        execute_flag: "-e",
        title_flag: Some("--title"),
        geometry_flag: None,
        working_dir_flag: Some("--workdir"),
    },
    TerminalDefinition {
        name: "jterm4",
        command: "jterm4",
        execute_flag: "-e",
        title_flag: Some("--title"),
        geometry_flag: None,
        working_dir_flag: Some("--workdir"),
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
];

const WAYLAND_TERMINAL_PRIORITY: &[&str] = &[
    "jterm4",
    "jterm3",
    "jterm2",
    "jterm1",
    "alacritty",
    "terminator",
    "gnome-terminal",
    // Keep Warp last on Wayland: it may depend on X11/desktop services.
    "warp-terminal",
];

const X11_TERMINAL_PRIORITY: &[&str] = &[
    "jterm4",
    "jterm3",
    "jterm2",
    "jterm1",
    "warp-terminal",
    "terminator",
    "gnome-terminal",
    "alacritty",
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const SUPPORTED: &[&str] = &[
        "jterm1",
        "jterm2",
        "jterm3",
        "jterm4",
        "alacritty",
        "terminator",
        "gnome-terminal",
        "warp-terminal",
    ];

    #[test]
    fn definitions_only_contain_supported_terminals() {
        let definitions: HashSet<_> = TERMINAL_DEFINITIONS
            .iter()
            .map(|definition| definition.name)
            .collect();
        let supported: HashSet<_> = SUPPORTED.iter().copied().collect();

        assert_eq!(definitions, supported);
        assert_eq!(TERMINAL_DEFINITIONS.len(), SUPPORTED.len());
        assert!(
            TERMINAL_DEFINITIONS
                .iter()
                .all(|definition| definition.name == definition.command)
        );
    }

    #[test]
    fn priorities_cover_each_supported_terminal_once() {
        let supported: HashSet<_> = SUPPORTED.iter().copied().collect();
        for priority in [WAYLAND_TERMINAL_PRIORITY, X11_TERMINAL_PRIORITY] {
            let names: HashSet<_> = priority.iter().copied().collect();
            assert_eq!(names, supported);
            assert_eq!(priority.len(), SUPPORTED.len());
        }
    }

    #[test]
    fn path_probe_finds_executable_without_which() {
        let executable = std::env::current_exe().unwrap();
        assert!(command_exists_in_path(executable.to_str().unwrap(), None));

        let search_path = std::env::join_paths([executable.parent().unwrap()]).unwrap();
        assert!(command_exists_in_path(
            executable.file_name().unwrap().to_str().unwrap(),
            Some(&search_path)
        ));
        assert!(!command_exists_in_path(
            "definitely-not-a-jwm-terminal",
            Some(&search_path)
        ));
    }
}
