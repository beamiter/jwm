//! Quickshell-inspired navigation for JWM's native shell.
//!
//! DMS, Noctalia, Caelestia and end-4 all converge on one useful interaction:
//! a single surface routes to apps, notifications, clipboard, calendar and
//! appearance controls. JWM keeps that interaction but implements it in its
//! own Rust state machine, without a QML runtime or shell command evaluation.

use std::path::Path;

const MAX_STATUS_CHARS: usize = 22;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShellHubRoute {
    Applications,
    Notifications,
    Clipboard,
    Calendar,
    Wallpaper,
}

impl ShellHubRoute {
    pub const ALL: [Self; 5] = [
        Self::Applications,
        Self::Notifications,
        Self::Clipboard,
        Self::Calendar,
        Self::Wallpaper,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Applications => "Applications",
            Self::Notifications => "Notifications",
            Self::Clipboard => "Clipboard",
            Self::Calendar => "Calendar",
            Self::Wallpaper => "Wallpaper",
        }
    }

    #[must_use]
    pub const fn icon(self) -> &'static str {
        match self {
            Self::Applications => "\u{f135}",  // rocket
            Self::Notifications => "\u{f0f3}", // bell
            Self::Clipboard => "\u{f0ea}",     // clipboard
            Self::Calendar => "\u{f073}",      // calendar
            Self::Wallpaper => "\u{f03e}",     // image
        }
    }

    #[must_use]
    pub const fn shortcut(self) -> char {
        match self {
            Self::Applications => 'a',
            Self::Notifications => 'n',
            Self::Clipboard => 'c',
            Self::Calendar => 'd',
            Self::Wallpaper => 'w',
        }
    }

    #[must_use]
    pub fn from_shortcut(ch: char) -> Option<Self> {
        match ch.to_ascii_lowercase() {
            'a' => Some(Self::Applications),
            'n' => Some(Self::Notifications),
            'c' => Some(Self::Clipboard),
            'd' => Some(Self::Calendar),
            'w' => Some(Self::Wallpaper),
            _ => None,
        }
    }

    /// A compact action row with the same information density as a shell tile.
    #[must_use]
    pub fn row(self, count: Option<usize>, detail: Option<&str>) -> String {
        let status = match self {
            Self::Applications => "search & run".to_string(),
            Self::Notifications => match count.unwrap_or_default() {
                0 => "all clear".to_string(),
                n => format!("{n} waiting"),
            },
            Self::Clipboard => match count.unwrap_or_default() {
                0 => "empty".to_string(),
                n => format!("{n} saved"),
            },
            Self::Calendar => "month view".to_string(),
            Self::Wallpaper => detail
                .and_then(|path| Path::new(path).file_name())
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .map(compact_status)
                .unwrap_or_else(|| "choose image".to_string()),
        };
        let shortcut = self.shortcut().to_ascii_uppercase();
        format!(
            "{}  {:<18} {:<24} [{shortcut}]  \u{f054}",
            self.icon(),
            self.label(),
            compact_status(&status)
        )
    }
}

fn compact_status(text: &str) -> String {
    if text.chars().count() <= MAX_STATUS_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_STATUS_CHARS - 1).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_are_case_insensitive_and_unique() {
        for route in ShellHubRoute::ALL {
            assert_eq!(ShellHubRoute::from_shortcut(route.shortcut()), Some(route));
            assert_eq!(
                ShellHubRoute::from_shortcut(route.shortcut().to_ascii_uppercase()),
                Some(route)
            );
        }
        assert_eq!(ShellHubRoute::from_shortcut('x'), None);
    }

    #[test]
    fn rows_show_live_badges_and_only_the_wallpaper_file_name() {
        let notifications = ShellHubRoute::Notifications.row(Some(7), None);
        assert!(notifications.contains("7 waiting"));
        assert!(notifications.contains("[N]"));

        let wallpaper =
            ShellHubRoute::Wallpaper.row(None, Some("/home/user/Pictures/aurora-night.png"));
        assert!(wallpaper.contains("aurora-night.png"));
        assert!(!wallpaper.contains("/home/user"));
    }

    #[test]
    fn long_status_text_is_bounded() {
        let row = ShellHubRoute::Wallpaper.row(
            None,
            Some("/tmp/a-wallpaper-name-that-is-far-too-wide-for-the-card.png"),
        );
        assert!(row.contains('\u{2026}'));
        assert!(!row.contains("far-too-wide-for-the-card.png"));
    }
}
