//! Session and power actions behind the shell's session menu.
//!
//! What each action *is* — its label, whether it needs confirming, whether the
//! machine can even do it — is decided here as pure data, so the menu's
//! arm-then-confirm behavior is unit tested without running anything. The
//! commands themselves come from configuration; only [`run`] touches the
//! system.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAction {
    Lock,
    Suspend,
    Hibernate,
    Logout,
    Reboot,
    Shutdown,
}

impl SessionAction {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Lock => "Lock Screen",
            Self::Suspend => "Suspend",
            Self::Hibernate => "Hibernate",
            Self::Logout => "Log Out",
            Self::Reboot => "Restart",
            Self::Shutdown => "Shut Down",
        }
    }

    #[must_use]
    pub fn icon(self) -> &'static str {
        match self {
            Self::Lock => "\u{f023}",      // fa-lock
            Self::Suspend => "\u{f186}",   // fa-moon
            Self::Hibernate => "\u{f2dc}", // fa-snowflake
            Self::Logout => "\u{f2f5}",    // fa-sign-out
            Self::Reboot => "\u{f021}",    // fa-refresh
            Self::Shutdown => "\u{f011}",  // fa-power-off
        }
    }

    /// Whether activating this needs a second Enter. Suspend and hibernate
    /// are recoverable by pressing a key; the rest lose the session or the
    /// machine, so an accidental keystroke must not be enough.
    #[must_use]
    pub fn needs_confirmation(self) -> bool {
        matches!(self, Self::Logout | Self::Reboot | Self::Shutdown)
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "lock" => Some(Self::Lock),
            "suspend" => Some(Self::Suspend),
            "hibernate" => Some(Self::Hibernate),
            "logout" => Some(Self::Logout),
            "reboot" | "restart" => Some(Self::Reboot),
            "shutdown" | "poweroff" => Some(Self::Shutdown),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lock => "lock",
            Self::Suspend => "suspend",
            Self::Hibernate => "hibernate",
            Self::Logout => "logout",
            Self::Reboot => "reboot",
            Self::Shutdown => "shutdown",
        }
    }
}

/// Rows the menu offers, dropping what this machine cannot do.
///
/// `hibernate_supported` is the only capability that varies in practice;
/// suspend is offered even on machines that will refuse it, because the
/// kernel's own report is the honest answer and logind will say so.
#[must_use]
pub fn available_actions(hibernate_supported: bool) -> Vec<SessionAction> {
    let mut actions = vec![SessionAction::Lock, SessionAction::Suspend];
    if hibernate_supported {
        actions.push(SessionAction::Hibernate);
    }
    actions.extend([
        SessionAction::Logout,
        SessionAction::Reboot,
        SessionAction::Shutdown,
    ]);
    actions
}

/// Whether the kernel advertises suspend-to-disk. Reading `/sys/power/state`
/// is what logind itself keys off, so an unavailable row is never drawn.
#[must_use]
pub fn hibernate_supported() -> bool {
    hibernate_supported_in(Path::new("/sys/power/state"))
}

fn hibernate_supported_in(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|states| states.split_whitespace().any(|state| state == "disk"))
        .unwrap_or(false)
}

/// One row's rendered text, including the confirmation prompt when armed.
#[must_use]
pub fn menu_row(action: SessionAction, armed: bool) -> String {
    if armed {
        format!(
            "{}  {}{:>28}",
            action.icon(),
            action.label(),
            "Enter to confirm"
        )
    } else {
        format!("{}  {}", action.icon(), action.label())
    }
}

/// Split a configured command line into program and arguments.
///
/// Deliberately not a shell: quotes only preserve argument boundaries and
/// operators such as `|` and `;` stay literal argv entries.
#[must_use]
pub fn split_command(command: &str) -> Option<(String, Vec<String>)> {
    let mut argv = crate::command_line::split_command_line(command).ok()?;
    let program = argv.first()?;
    if program.trim().is_empty() {
        return None;
    }
    let program = program.clone();
    argv.remove(0);
    Some((program, argv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hibernate_is_offered_only_when_the_kernel_supports_it() {
        assert!(available_actions(true).contains(&SessionAction::Hibernate));
        assert!(!available_actions(false).contains(&SessionAction::Hibernate));
    }

    #[test]
    fn the_menu_always_offers_lock_and_the_power_actions() {
        let actions = available_actions(false);
        assert_eq!(actions.first(), Some(&SessionAction::Lock));
        assert!(actions.contains(&SessionAction::Suspend));
        assert!(actions.contains(&SessionAction::Logout));
        assert!(actions.contains(&SessionAction::Reboot));
        assert!(actions.contains(&SessionAction::Shutdown));
    }

    #[test]
    fn only_the_destructive_actions_need_confirming() {
        assert!(!SessionAction::Lock.needs_confirmation());
        assert!(!SessionAction::Suspend.needs_confirmation());
        assert!(!SessionAction::Hibernate.needs_confirmation());
        assert!(SessionAction::Logout.needs_confirmation());
        assert!(SessionAction::Reboot.needs_confirmation());
        assert!(SessionAction::Shutdown.needs_confirmation());
    }

    #[test]
    fn action_names_round_trip_with_the_aliases_scripts_use() {
        for action in available_actions(true) {
            assert_eq!(SessionAction::from_name(action.as_str()), Some(action));
        }
        assert_eq!(
            SessionAction::from_name("poweroff"),
            Some(SessionAction::Shutdown)
        );
        assert_eq!(
            SessionAction::from_name("restart"),
            Some(SessionAction::Reboot)
        );
        assert_eq!(SessionAction::from_name("sleep"), None);
    }

    #[test]
    fn an_armed_row_says_what_the_next_key_does() {
        let row = menu_row(SessionAction::Shutdown, true);
        assert!(row.contains("Shut Down"));
        assert!(row.contains("Enter to confirm"));
        assert!(!menu_row(SessionAction::Shutdown, false).contains("confirm"));
    }

    #[test]
    fn commands_split_into_argv_without_a_shell() {
        let (program, args) = split_command("systemctl poweroff").expect("a command");
        assert_eq!(program, "systemctl");
        assert_eq!(args, vec!["poweroff"]);

        let (program, args) = split_command("  loginctl  lock-session  ").expect("a command");
        assert_eq!(program, "loginctl");
        assert_eq!(args, vec!["lock-session"]);

        let (program, args) =
            split_command("locker --message 'Back in five minutes'").expect("a command");
        assert_eq!(program, "locker");
        assert_eq!(args, vec!["--message", "Back in five minutes"]);

        assert!(split_command("   ").is_none());
        assert!(split_command("\"\" --argument").is_none());
        assert!(split_command("'   ' --argument").is_none());
        assert!(split_command("locker 'unfinished").is_none());
    }

    #[test]
    fn kernel_power_states_are_parsed_by_word() {
        let dir = std::env::temp_dir().join(format!("jwm-session-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");

        let with_disk = dir.join("with_disk");
        std::fs::write(&with_disk, "freeze mem disk\n").expect("write");
        assert!(hibernate_supported_in(&with_disk));

        let without_disk = dir.join("without_disk");
        std::fs::write(&without_disk, "freeze mem\n").expect("write");
        assert!(!hibernate_supported_in(&without_disk));

        // A missing file is a definite "no", not a panic.
        assert!(!hibernate_supported_in(&dir.join("absent")));

        // "diskless" must not be mistaken for "disk".
        let lookalike = dir.join("lookalike");
        std::fs::write(&lookalike, "freeze diskless\n").expect("write");
        assert!(!hibernate_supported_in(&lookalike));

        std::fs::remove_dir_all(&dir).ok();
    }
}
