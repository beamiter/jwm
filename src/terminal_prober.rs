use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::LazyLock;

fn command_exists(cmd: &str) -> bool {
    command_exists_in_path(cmd, env::var_os("PATH").as_deref())
}

pub(crate) fn command_exists_in_path(cmd: &str, path: Option<&OsStr>) -> bool {
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
    is_wayland_session_from(
        env::var_os("WAYLAND_DISPLAY").as_deref(),
        env::var_os("XDG_SESSION_TYPE").as_deref(),
        env::var_os("DISPLAY").as_deref(),
    )
}

pub(crate) fn is_wayland_session_from(
    wayland_display: Option<&OsStr>,
    session_type: Option<&OsStr>,
    display: Option<&OsStr>,
) -> bool {
    let explicitly_wayland = session_type
        .and_then(OsStr::to_str)
        .is_some_and(|value| value.eq_ignore_ascii_case("wayland"));
    let has_wayland_display = wayland_display.is_some_and(|value| !value.is_empty());
    let has_x11_display = display.is_some_and(|value| !value.is_empty());
    explicitly_wayland || (has_wayland_display && !has_x11_display)
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TerminalConfig {
    pub command: String,
    pub execute_flag: Option<String>,
    pub title_flag: Option<String>,
    pub geometry_flag: Option<String>,
    pub working_dir_flag: Option<String>,
    pub scratchpad_pid_stable: bool,
}

/// The capability a caller needs from a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalPurpose {
    /// Open an ordinary interactive shell.
    Interactive,
    /// Run a caller-supplied command inside the terminal.
    Execute,
    /// Keep the spawned process as the owner of the mapped terminal window.
    Scratchpad,
}

impl TerminalConfig {
    fn supports(&self, purpose: TerminalPurpose) -> bool {
        match purpose {
            TerminalPurpose::Interactive => true,
            TerminalPurpose::Execute => self.execute_flag.is_some(),
            TerminalPurpose::Scratchpad => self.scratchpad_pid_stable,
        }
    }
}

pub struct AdvancedTerminalProber {
    configs: HashMap<String, TerminalConfig>,
    priority_order: Vec<String>,
}

struct TerminalDefinition {
    name: &'static str,
    command: &'static str,
    execute_flag: Option<&'static str>,
    title_flag: Option<&'static str>,
    geometry_flag: Option<&'static str>,
    working_dir_flag: Option<&'static str>,
    scratchpad_pid_stable: bool,
}

impl TerminalDefinition {
    fn config(&self) -> TerminalConfig {
        TerminalConfig {
            command: self.command.to_string(),
            execute_flag: self.execute_flag.map(str::to_string),
            title_flag: self.title_flag.map(str::to_string),
            geometry_flag: self.geometry_flag.map(str::to_string),
            working_dir_flag: self.working_dir_flag.map(str::to_string),
            scratchpad_pid_stable: self.scratchpad_pid_stable,
        }
    }
}

const TERMINAL_DEFINITIONS: &[TerminalDefinition] = &[
    TerminalDefinition {
        name: "forge",
        command: "forge",
        execute_flag: Some("-e"),
        title_flag: None,
        geometry_flag: None,
        working_dir_flag: Some("--working-directory"),
        scratchpad_pid_stable: true,
    },
    TerminalDefinition {
        name: "anvil",
        command: "anvil",
        execute_flag: Some("-e"),
        title_flag: None,
        geometry_flag: None,
        working_dir_flag: Some("--working-directory"),
        scratchpad_pid_stable: true,
    },
    TerminalDefinition {
        name: "ember",
        command: "ember",
        execute_flag: None,
        title_flag: None,
        geometry_flag: None,
        working_dir_flag: None,
        scratchpad_pid_stable: true,
    },
    TerminalDefinition {
        name: "frost",
        command: "frost",
        execute_flag: None,
        title_flag: None,
        geometry_flag: None,
        working_dir_flag: None,
        scratchpad_pid_stable: true,
    },
    TerminalDefinition {
        name: "alacritty",
        command: "alacritty",
        execute_flag: Some("-e"),
        title_flag: Some("--title"),
        geometry_flag: None,
        working_dir_flag: Some("--working-directory"),
        scratchpad_pid_stable: true,
    },
    TerminalDefinition {
        name: "warp-terminal",
        command: "warp-terminal",
        execute_flag: Some("-e"),
        title_flag: Some("--title"),
        geometry_flag: None,
        working_dir_flag: Some("--working-directory"),
        scratchpad_pid_stable: false,
    },
    TerminalDefinition {
        name: "terminator",
        command: "terminator",
        execute_flag: Some("-x"),
        title_flag: Some("-T"),
        geometry_flag: Some("--geometry"),
        working_dir_flag: Some("--working-directory"),
        scratchpad_pid_stable: false,
    },
    TerminalDefinition {
        name: "gnome-terminal",
        command: "gnome-terminal",
        execute_flag: Some("--"),
        title_flag: Some("--title"),
        geometry_flag: Some("--geometry"),
        working_dir_flag: Some("--working-directory"),
        scratchpad_pid_stable: false,
    },
];

const WAYLAND_TERMINAL_PRIORITY: &[&str] = &[
    "frost",
    "ember",
    "anvil",
    "forge",
    "alacritty",
    "terminator",
    "gnome-terminal",
    // Keep Warp last on Wayland: it may depend on X11/desktop services.
    "warp-terminal",
];

const X11_TERMINAL_PRIORITY: &[&str] = &[
    "frost",
    "ember",
    "anvil",
    "forge",
    "warp-terminal",
    "terminator",
    "gnome-terminal",
    "alacritty",
];

impl AdvancedTerminalProber {
    fn new() -> Self {
        Self::for_session(is_wayland_session())
    }

    fn for_session(wayland: bool) -> Self {
        let configs = TERMINAL_DEFINITIONS
            .iter()
            .map(|definition| (definition.name.to_string(), definition.config()))
            .collect();

        // Choose priority based on session hints.
        // - In udev/DRM (Wayland compositor) sessions, X11 terminals often won't show.
        // - In X11 sessions, Warp/Terminator/Gnome-terminal are usually fine.
        let priority = if wayland {
            WAYLAND_TERMINAL_PRIORITY
        } else {
            X11_TERMINAL_PRIORITY
        };
        let priority_order = priority.iter().map(|name| (*name).to_string()).collect();

        Self {
            configs,
            priority_order,
        }
    }

    /// Select an interactive terminal using the session's normal priority.
    ///
    /// Kept as the compatibility entry point for callers that only need to
    /// open a shell. Capability-sensitive callers should use
    /// [`Self::get_available_terminal_for`].
    #[must_use]
    pub fn get_available_terminal(&self) -> Option<&TerminalConfig> {
        self.get_available_terminal_for(TerminalPurpose::Interactive)
    }

    /// Select an interactive terminal, trying `preferred` first.
    ///
    /// Kept as the compatibility entry point for existing configuration code.
    #[must_use]
    pub fn get_available_terminal_with_priority(
        &self,
        preferred: Option<&str>,
    ) -> Option<&TerminalConfig> {
        self.get_available_terminal_for_with_priority(TerminalPurpose::Interactive, preferred)
    }

    /// Select a terminal that supports `purpose` using the session's normal
    /// priority order. Availability is checked against the current `PATH` on
    /// every call so installs, removals, and environment changes take effect
    /// without restarting JWM.
    #[must_use]
    pub fn get_available_terminal_for(&self, purpose: TerminalPurpose) -> Option<&TerminalConfig> {
        self.get_available_terminal_for_with_priority(purpose, None)
    }

    /// Select a terminal that supports `purpose`, trying `preferred` before
    /// the normal priority order when it has the required capability.
    #[must_use]
    pub fn get_available_terminal_for_with_priority(
        &self,
        purpose: TerminalPurpose,
        preferred: Option<&str>,
    ) -> Option<&TerminalConfig> {
        self.select_available_terminal(purpose, preferred, command_exists)
    }

    /// Look up capabilities for an explicitly configured terminal command.
    /// Paths are matched by basename so `/usr/bin/gnome-terminal` retains the
    /// same execution protocol as `gnome-terminal` discovered on `PATH`.
    #[must_use]
    pub fn config_for_command(&self, command: &str) -> Option<&TerminalConfig> {
        let name = Path::new(command)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(command);
        self.configs.get(name)
    }

    fn select_available_terminal<F>(
        &self,
        purpose: TerminalPurpose,
        preferred: Option<&str>,
        mut is_available: F,
    ) -> Option<&TerminalConfig>
    where
        F: FnMut(&str) -> bool,
    {
        if let Some(preferred) = preferred
            && let Some(config) = self.configs.get(preferred)
            && config.supports(purpose)
            && is_available(&config.command)
        {
            log::debug!("[terminal-prober] using preferred terminal for {purpose:?}: {config:?}");
            return Some(config);
        }

        for terminal_name in &self.priority_order {
            if preferred.is_some_and(|preferred| preferred == terminal_name) {
                continue;
            }
            if let Some(config) = self.configs.get(terminal_name)
                && config.supports(purpose)
                && is_available(&config.command)
            {
                log::debug!("[terminal-prober] selected terminal for {purpose:?}: {config:?}");
                return Some(config);
            }
        }
        None
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn build_command(
        &self,
        command: &str,
        title: Option<&str>,
        working_dir: Option<&str>,
    ) -> Option<Vec<String>> {
        let config = self.get_available_terminal_for(TerminalPurpose::Execute)?;
        let mut cmd = vec![config.command.clone()];
        if let (Some(title), Some(title_flag)) = (title, &config.title_flag) {
            cmd.push(title_flag.clone());
            cmd.push(title.to_string());
        }
        if let (Some(working_dir), Some(dir_flag)) = (working_dir, &config.working_dir_flag) {
            cmd.push(dir_flag.clone());
            cmd.push(working_dir.to_string());
        }
        cmd.push(config.execute_flag.clone()?);
        cmd.push(command.to_string());
        Some(cmd)
    }
}

pub(crate) fn available_terminal_in_path(
    path: Option<&OsStr>,
    wayland: bool,
    purpose: TerminalPurpose,
) -> Option<TerminalConfig> {
    AdvancedTerminalProber::for_session(wayland)
        .select_available_terminal(purpose, None, |command| {
            command_exists_in_path(command, path)
        })
        .cloned()
}

pub static ADVANCED_TERMINAL_PROBER: LazyLock<AdvancedTerminalProber> =
    LazyLock::new(AdvancedTerminalProber::new);

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const SUPPORTED: &[&str] = &[
        "forge",
        "anvil",
        "ember",
        "frost",
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
    fn native_terminal_capabilities_match_their_command_line_contracts() {
        let prober = AdvancedTerminalProber::new();

        for name in ["forge", "anvil"] {
            let config = &prober.configs[name];
            assert_eq!(config.execute_flag.as_deref(), Some("-e"));
            assert_eq!(config.title_flag, None);
            assert_eq!(
                config.working_dir_flag.as_deref(),
                Some("--working-directory")
            );
            assert!(config.scratchpad_pid_stable);
        }
        for name in ["ember", "frost"] {
            let config = &prober.configs[name];
            assert_eq!(config.execute_flag, None);
            assert_eq!(config.title_flag, None);
            assert_eq!(config.working_dir_flag, None);
            assert!(config.scratchpad_pid_stable);
        }
        assert_eq!(
            prober.configs["gnome-terminal"].execute_flag.as_deref(),
            Some("--")
        );
        assert_eq!(
            prober.configs["terminator"].execute_flag.as_deref(),
            Some("-x")
        );
        assert_eq!(
            prober.configs["terminator"].geometry_flag.as_deref(),
            Some("--geometry")
        );

        let pid_stable: HashSet<_> = prober
            .configs
            .iter()
            .filter_map(|(name, config)| config.scratchpad_pid_stable.then_some(name.as_str()))
            .collect();
        assert_eq!(
            pid_stable,
            HashSet::from(["forge", "anvil", "ember", "frost", "alacritty"])
        );
    }

    #[test]
    fn purpose_selection_filters_by_capability_before_priority() {
        let prober = AdvancedTerminalProber::for_session(true);
        let frost_and_forge = |command: &str| matches!(command, "frost" | "forge");

        assert_eq!(
            prober
                .select_available_terminal(TerminalPurpose::Interactive, None, frost_and_forge,)
                .map(|config| config.command.as_str()),
            Some("frost")
        );
        assert_eq!(
            prober
                .select_available_terminal(TerminalPurpose::Execute, None, frost_and_forge)
                .map(|config| config.command.as_str()),
            Some("forge")
        );
        assert_eq!(
            prober
                .select_available_terminal(TerminalPurpose::Scratchpad, None, frost_and_forge)
                .map(|config| config.command.as_str()),
            Some("frost")
        );
    }

    #[test]
    fn scratchpad_selection_rejects_pid_unstable_terminals() {
        let prober = AdvancedTerminalProber::new();
        let gnome_and_alacritty = |command: &str| matches!(command, "gnome-terminal" | "alacritty");

        assert_eq!(
            prober
                .select_available_terminal(
                    TerminalPurpose::Scratchpad,
                    Some("gnome-terminal"),
                    gnome_and_alacritty,
                )
                .map(|config| config.command.as_str()),
            Some("alacritty")
        );
    }

    #[test]
    fn session_detection_ignores_empty_display_variables() {
        let empty = OsStr::new("");
        let wayland = OsStr::new("wayland-1");
        let x11 = OsStr::new(":0");

        assert!(is_wayland_session_from(Some(wayland), None, Some(empty)));
        assert!(!is_wayland_session_from(Some(empty), None, None));
        assert!(!is_wayland_session_from(Some(wayland), None, Some(x11)));
        assert!(is_wayland_session_from(
            None,
            Some(OsStr::new("Wayland")),
            Some(x11)
        ));
    }

    #[test]
    fn explicit_paths_reuse_known_terminal_capabilities() {
        let prober = AdvancedTerminalProber::for_session(false);
        assert_eq!(
            prober
                .config_for_command("/usr/bin/gnome-terminal")
                .and_then(|config| config.execute_flag.as_deref()),
            Some("--")
        );
        assert!(prober.config_for_command("/opt/custom-terminal").is_none());
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
