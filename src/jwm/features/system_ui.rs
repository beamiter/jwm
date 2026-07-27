//! Backend-independent modal system UI state.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchEntry {
    pub name: String,
    pub command: Vec<String>,
    search: String,
}

/// Structured content of the modal system UI panel, consumed by the
/// compositor's styled-card renderer (rounded panel, search bar, highlighted
/// selection row) and by [`SystemUiState::overlay_text`] for flat text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OverlayParts {
    pub title: String,
    /// Search-field content; `Some` renders a query bar with a caret.
    pub query: Option<String>,
    pub items: Vec<String>,
    /// Row in `items` to highlight.
    pub selected: Option<usize>,
    pub hint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorLayoutEntry {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorDirection {
    Left,
    Right,
    Above,
    Below,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MonitorAlignment {
    Start,
    Center,
    End,
}

#[derive(Debug, Default)]
pub enum SystemUiState {
    #[default]
    Inactive,
    Launcher {
        query: String,
        entries: Vec<LaunchEntry>,
        matches: Vec<usize>,
        selected: usize,
    },
    Info {
        title: String,
        lines: Vec<String>,
        query: String,
        matches: Vec<usize>,
        offset: usize,
    },
    MonitorLayout {
        entries: Vec<MonitorLayoutEntry>,
        selected: usize,
        reference: usize,
        message: String,
    },
    Locked {
        password: String,
        message: String,
    },
    ControlCenter {
        entries: Vec<ControlEntry>,
        selected: usize,
        /// Whether the selected row has been armed by a first Enter, for the
        /// rows whose "off" state the user cannot recover from. Moving the
        /// selection disarms it.
        armed: bool,
    },
    /// Notifications, Wi-Fi networks, Bluetooth devices, and wallpapers are
    /// all the same panel: a scrolling list with a status line and an
    /// optional masked prompt. Only what a row *means* differs, which is
    /// what [`ListKind`] and [`RowData`] carry.
    ListPanel {
        kind: ListKind,
        rows: Vec<ListRow>,
        selected: usize,
        /// Status line: scanning, connecting, or why something failed.
        message: String,
        /// Masked secret entry, while the panel is asking for one.
        prompt: Option<String>,
        /// Shown when the list is empty and there is no message.
        empty: String,
    },
    Calendar {
        view: crate::jwm::features::CalendarView,
        /// The clock line above the grid, captured when the card opened.
        clock: String,
    },
    SessionMenu {
        entries: Vec<crate::jwm::features::SessionAction>,
        selected: usize,
        /// Whether the selected row has been armed by a first Enter. Moving
        /// the selection disarms it.
        armed: bool,
    },
}

/// One notification-center row: the pre-rendered text plus what activating it
/// needs. Rendering happens when the panel opens, so the compositor never
/// reaches back into the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationEntry {
    pub id: u32,
    pub row: String,
    /// Action key the sender marked as default, invoked on Return.
    pub default_action: Option<String>,
}

/// What a [`SystemUiState::ListPanel`] is listing. Decides the title, the
/// hint, and how many rows fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    Notifications,
    Wifi,
    Bluetooth,
    Wallpaper,
}

impl ListKind {
    fn title(self) -> &'static str {
        match self {
            Self::Notifications => "\u{f0f3}  NOTIFICATIONS",
            Self::Wifi => "\u{f1eb}  WI-FI",
            Self::Bluetooth => "\u{f293}  BLUETOOTH",
            Self::Wallpaper => "\u{f03e}  WALLPAPER",
        }
    }

    fn hint(self, prompting: bool) -> &'static str {
        if prompting {
            return "Enter  join    Esc  cancel";
        }
        match self {
            Self::Notifications => "Enter  activate    d  dismiss    c  clear all    Esc  close",
            Self::Wifi => "Enter  join    \u{f062}/\u{f063}  select    Esc  close",
            Self::Bluetooth => "Enter  connect/disconnect    r  refresh    Esc  close",
            Self::Wallpaper => "Enter  apply    \u{f062}/\u{f063}  select    Esc  close",
        }
    }

    /// Rows drawn at once. Notifications get more because their history is
    /// the one list users scroll rather than pick from.
    fn window(self) -> usize {
        match self {
            Self::Notifications => 14,
            _ => 12,
        }
    }
}

/// What activating a row does, per kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowData {
    Notification {
        id: u32,
        /// Action key the sender marked as default, invoked on Return.
        default_action: Option<String>,
    },
    /// Whether the network is secured, i.e. may need a passphrase.
    Wifi {
        secured: bool,
    },
    /// `connect` or `disconnect`, decided when the list was built.
    Bluetooth {
        action: &'static str,
    },
    Wallpaper,
}

/// One row of a list panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListRow {
    /// Stable identity: the SSID, the device address, the wallpaper path.
    /// Used to hold the selection steady across a refresh.
    pub key: String,
    pub text: String,
    pub data: RowData,
}

/// One row of the control center: sliders react to Left/Right, toggles and
/// actions to Return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKind {
    /// Transport row for the active MPRIS player: Left/Right skip, Return
    /// toggles playback.
    Media,
    /// Wi-Fi radio toggle; the label carries the connection and signal.
    Network,
    /// Bluetooth controller toggle.
    Bluetooth,
    Volume,
    Brightness,
    /// Read-only battery readout; no interaction.
    Battery,
    /// Power profile selector: Left/Right cycles the driver's profiles.
    PowerProfile,
    NightLight,
    DoNotDisturb,
    LockScreen,
    /// Opens the session menu, the way `LockScreen` opens the lock overlay.
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlEntry {
    pub kind: ControlKind,
    /// Slider position for Volume/Brightness; unused for toggles.
    pub percent: u8,
    /// Mute state for Volume, on/off for DoNotDisturb; unused otherwise.
    pub enabled: bool,
    /// Pre-rendered text for rows whose content is not derived from
    /// `percent`/`enabled`: media, network, Bluetooth, battery, and the
    /// power profile.
    pub label: String,
}

/// Everything the control center renders from. A struct rather than a long
/// positional argument list: rows come and go as hardware appears, and a
/// mis-ordered bool would silently light the wrong toggle.
#[derive(Debug, Clone, Copy, Default)]
pub struct ControlCenterInputs<'a> {
    pub media: Option<&'a crate::jwm::features::MediaState>,
    /// Percentage and mute state, when a working audio control exists.
    pub volume: Option<(u8, bool)>,
    pub brightness: Option<u8>,
    pub battery: Option<&'a crate::jwm::features::BatteryState>,
    /// Wi-Fi state, when this machine has a radio to report on.
    pub network: Option<&'a crate::jwm::features::NetworkState>,
    /// Bluetooth state; the row is hidden without a controller.
    pub bluetooth: Option<&'a crate::jwm::features::BluetoothState>,
    /// Name of the active power profile, when this machine has profiles.
    pub power_profile: Option<&'a str>,
    pub night_light: bool,
    pub do_not_disturb: bool,
}

/// Whether activating this row needs a second Enter to confirm.
///
/// The test is not "is this destructive" but "can the user undo it with the
/// input they have left". Switching Bluetooth off on a machine driven by a
/// Bluetooth keyboard removes the very keys needed to switch it back on, so
/// turning it *off* confirms; turning it on never does. Everything else in
/// the panel is either recoverable from the keyboard or has its own
/// confirmation further in (the session menu).
#[must_use]
pub fn needs_confirmation(kind: ControlKind, currently_enabled: bool) -> bool {
    matches!(kind, ControlKind::Bluetooth) && currently_enabled
}

impl ControlEntry {
    fn simple(kind: ControlKind, percent: u8, enabled: bool) -> Self {
        Self {
            kind,
            percent,
            enabled,
            label: String::new(),
        }
    }
}

/// Render a 20-cell slider bar, e.g. `█████████░░░░░░░░░░░`.
fn slider_bar(percent: u8) -> String {
    const CELLS: usize = 20;
    let filled = (usize::from(percent.min(100)) * CELLS + 50) / 100;
    let mut bar = String::with_capacity(CELLS * 3);
    for cell in 0..CELLS {
        bar.push(if cell < filled {
            '\u{2588}'
        } else {
            '\u{2591}'
        });
    }
    bar
}

impl Clone for SystemUiState {
    fn clone(&self) -> Self {
        match self {
            Self::Inactive => Self::Inactive,
            Self::Launcher {
                query,
                entries,
                matches,
                selected,
            } => Self::Launcher {
                query: query.clone(),
                entries: entries.clone(),
                matches: matches.clone(),
                selected: *selected,
            },
            Self::Info {
                title,
                lines,
                query,
                matches,
                offset,
            } => Self::Info {
                title: title.clone(),
                lines: lines.clone(),
                query: query.clone(),
                matches: matches.clone(),
                offset: *offset,
            },
            Self::MonitorLayout {
                entries,
                selected,
                reference,
                message,
            } => Self::MonitorLayout {
                entries: entries.clone(),
                selected: *selected,
                reference: *reference,
                message: message.clone(),
            },
            // Never duplicate credentials into another allocation.
            Self::Locked { message, .. } => Self::Locked {
                password: String::new(),
                message: message.clone(),
            },
            Self::ControlCenter {
                entries,
                selected,
                armed,
            } => Self::ControlCenter {
                entries: entries.clone(),
                selected: *selected,
                armed: *armed,
            },
            Self::ListPanel {
                kind,
                rows,
                selected,
                message,
                prompt,
                empty,
            } => Self::ListPanel {
                kind: *kind,
                rows: rows.clone(),
                selected: *selected,
                message: message.clone(),
                // Never duplicate a passphrase into another allocation.
                prompt: prompt.as_ref().map(|_| String::new()),
                empty: empty.clone(),
            },
            Self::Calendar { view, clock } => Self::Calendar {
                view: *view,
                clock: clock.clone(),
            },
            Self::SessionMenu {
                entries,
                selected,
                armed,
            } => Self::SessionMenu {
                entries: entries.clone(),
                selected: *selected,
                armed: *armed,
            },
        }
    }
}

impl SystemUiState {
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Inactive)
    }
    pub fn is_locked(&self) -> bool {
        matches!(self, Self::Locked { .. })
    }

    pub fn is_monitor_layout(&self) -> bool {
        matches!(self, Self::MonitorLayout { .. })
    }

    pub fn cancel(&mut self) {
        // Keep the optimizer from eliding the overwrites before dropping.
        match self {
            Self::Locked { password, .. } => unsafe { password.as_bytes_mut().fill(0) },
            Self::ListPanel {
                prompt: Some(typed),
                ..
            } => unsafe { typed.as_bytes_mut().fill(0) },
            _ => {}
        }
        *self = Self::Inactive;
    }

    pub fn open_launcher() -> Self {
        let entries = discover_applications();
        let matches = (0..entries.len()).collect();
        Self::Launcher {
            query: String::new(),
            entries,
            matches,
            selected: 0,
        }
    }

    pub fn lock() -> Self {
        Self::Locked {
            password: String::new(),
            message: String::new(),
        }
    }

    /// Build the control center from the currently available controls.
    /// Volume/brightness rows appear only when a working control exists.
    pub fn control_center(inputs: &ControlCenterInputs<'_>) -> Self {
        let ControlCenterInputs {
            media,
            volume,
            brightness,
            battery,
            network,
            bluetooth,
            power_profile,
            night_light,
            do_not_disturb,
        } = *inputs;
        let mut entries = Vec::new();
        if let Some(media) = media {
            entries.push(ControlEntry {
                kind: ControlKind::Media,
                percent: 0,
                enabled: media.status == crate::jwm::features::PlaybackStatus::Playing,
                label: crate::jwm::features::media::control_row(media),
            });
        }
        if let Some((percent, muted)) = volume {
            entries.push(ControlEntry::simple(ControlKind::Volume, percent, muted));
        }
        if let Some(percent) = brightness {
            entries.push(ControlEntry::simple(
                ControlKind::Brightness,
                percent,
                false,
            ));
        }
        if let Some(network) = network {
            entries.push(ControlEntry {
                kind: ControlKind::Network,
                percent: network.signal.unwrap_or(0),
                enabled: network.wifi_enabled,
                label: crate::jwm::features::connectivity::network_row(network),
            });
        }
        if let Some(bluetooth) = bluetooth.filter(|state| state.present) {
            entries.push(ControlEntry {
                kind: ControlKind::Bluetooth,
                percent: 0,
                enabled: bluetooth.powered,
                label: crate::jwm::features::connectivity::bluetooth_row(bluetooth),
            });
        }
        if let Some(battery) = battery {
            entries.push(ControlEntry {
                kind: ControlKind::Battery,
                percent: battery.percent,
                enabled: matches!(battery.status, crate::jwm::features::ChargeStatus::Charging),
                label: crate::jwm::features::power::control_row(battery),
            });
        }
        if let Some(profile) = power_profile {
            entries.push(ControlEntry {
                kind: ControlKind::PowerProfile,
                percent: 0,
                enabled: false,
                label: crate::jwm::features::power::profile_row(profile),
            });
        }
        entries.push(ControlEntry::simple(
            ControlKind::NightLight,
            0,
            night_light,
        ));
        entries.push(ControlEntry::simple(
            ControlKind::DoNotDisturb,
            0,
            do_not_disturb,
        ));
        entries.push(ControlEntry::simple(ControlKind::LockScreen, 0, false));
        entries.push(ControlEntry::simple(ControlKind::Session, 0, false));
        Self::ControlCenter {
            entries,
            selected: 0,
            armed: false,
        }
    }

    /// Open the calendar card on the month containing `today`.
    pub fn calendar(now: chrono::NaiveDateTime) -> Self {
        Self::Calendar {
            view: crate::jwm::features::CalendarView::new(now.date()),
            clock: crate::jwm::features::calendar::clock_line(&now),
        }
    }

    pub fn is_calendar(&self) -> bool {
        matches!(self, Self::Calendar { .. })
    }

    /// Step the shown month, year, or jump back to today.
    pub fn shift_calendar(&mut self, months: i32, years: i32, to_today: bool) {
        if let Self::Calendar { view, .. } = self {
            if to_today {
                view.reset();
                return;
            }
            if months != 0 {
                view.shift_month(months);
            }
            if years != 0 {
                view.shift_year(years);
            }
        }
    }

    // -----------------------------------------------------------------
    // List panels
    //
    // Four panels share one representation; these keep the callers' names,
    // so each still reads as "the Wi-Fi picker" or "the notification
    // center" without four copies of the same state machine underneath.
    // -----------------------------------------------------------------

    fn list_panel(&self) -> Option<(ListKind, &[ListRow], usize)> {
        let Self::ListPanel {
            kind,
            rows,
            selected,
            ..
        } = self
        else {
            return None;
        };
        Some((*kind, rows.as_slice(), *selected))
    }

    fn is_list(&self, wanted: ListKind) -> bool {
        matches!(self, Self::ListPanel { kind, .. } if *kind == wanted)
    }

    fn selected_row(&self, wanted: ListKind) -> Option<&ListRow> {
        let (kind, rows, selected) = self.list_panel()?;
        (kind == wanted).then(|| rows.get(selected)).flatten()
    }

    /// Replace a panel's rows, holding the selection on the same key when it
    /// survived the refresh.
    fn set_rows(&mut self, wanted: ListKind, next: Vec<ListRow>) {
        let Self::ListPanel {
            kind,
            rows,
            selected,
            message,
            ..
        } = self
        else {
            return;
        };
        if *kind != wanted {
            return;
        }
        let previous = rows.get(*selected).map(|row| row.key.clone());
        *rows = next;
        *selected = previous
            .and_then(|key| rows.iter().position(|row| row.key == key))
            .unwrap_or(0);
        message.clear();
    }

    fn set_list_message(&mut self, wanted: ListKind, text: impl Into<String>) {
        if let Self::ListPanel { kind, message, .. } = self
            && *kind == wanted
        {
            *message = text.into();
        }
    }

    fn open_list(kind: ListKind, message: impl Into<String>, empty: impl Into<String>) -> Self {
        Self::ListPanel {
            kind,
            rows: Vec::new(),
            selected: 0,
            message: message.into(),
            prompt: None,
            empty: empty.into(),
        }
    }

    // --- Notification center ---

    /// Build the notification center from the live history, newest first.
    pub fn notification_center(
        center: &crate::jwm::features::NotificationCenter,
        now_unix_ms: u64,
    ) -> Self {
        let rows = center
            .recent()
            .map(|record| ListRow {
                key: record.id.to_string(),
                text: crate::jwm::features::notifications::panel_row(record, now_unix_ms),
                data: RowData::Notification {
                    id: record.id,
                    default_action: record.default_action.clone(),
                },
            })
            .collect();
        Self::ListPanel {
            kind: ListKind::Notifications,
            rows,
            selected: 0,
            message: String::new(),
            prompt: None,
            empty: "No notifications".to_string(),
        }
    }

    pub fn is_notification_center(&self) -> bool {
        self.is_list(ListKind::Notifications)
    }

    /// The selected notification: its identifier and default action.
    pub fn selected_notification(&self) -> Option<(u32, Option<String>)> {
        match &self.selected_row(ListKind::Notifications)?.data {
            RowData::Notification { id, default_action } => Some((*id, default_action.clone())),
            _ => None,
        }
    }

    /// Drop one row after its notification was dismissed, keeping the
    /// selection on the row that slid into its place.
    pub fn remove_notification(&mut self, id: u32) {
        let Self::ListPanel {
            kind,
            rows,
            selected,
            ..
        } = self
        else {
            return;
        };
        if *kind != ListKind::Notifications {
            return;
        }
        let Some(index) = rows.iter().position(
            |row| matches!(&row.data, RowData::Notification { id: other, .. } if *other == id),
        ) else {
            return;
        };
        rows.remove(index);
        *selected = (*selected).min(rows.len().saturating_sub(1));
    }

    /// Empty the open notification center after a clear-all.
    pub fn clear_notifications(&mut self) {
        if let Self::ListPanel {
            kind,
            rows,
            selected,
            ..
        } = self
            && *kind == ListKind::Notifications
        {
            rows.clear();
            *selected = 0;
        }
    }

    // --- Wi-Fi picker ---

    /// Open the Wi-Fi picker in its scanning state. The list arrives later:
    /// nmcli's first scan takes seconds and must not block the compositor.
    pub fn wifi_picker(message: impl Into<String>) -> Self {
        Self::open_list(ListKind::Wifi, message, "No networks in range")
    }

    pub fn is_wifi_picker(&self) -> bool {
        self.is_list(ListKind::Wifi)
    }

    /// Fill in a finished scan, keeping the selection on the same network
    /// when it is still in range.
    pub fn set_wifi_networks(&mut self, networks: &[crate::jwm::features::WifiNetwork]) {
        let rows = networks
            .iter()
            .map(|network| ListRow {
                key: network.ssid.clone(),
                text: crate::jwm::features::connectivity::picker_row(network),
                data: RowData::Wifi {
                    secured: !network.is_open(),
                },
            })
            .collect();
        self.set_rows(ListKind::Wifi, rows);
    }

    /// The selected network: its SSID and whether it is secured.
    pub fn selected_wifi(&self) -> Option<(String, bool)> {
        let row = self.selected_row(ListKind::Wifi)?;
        match row.data {
            RowData::Wifi { secured } => Some((row.key.clone(), secured)),
            _ => None,
        }
    }

    /// Start prompting for the selected network's passphrase.
    pub fn prompt_wifi_passphrase(&mut self) {
        if let Self::ListPanel {
            kind,
            prompt,
            message,
            ..
        } = self
            && *kind == ListKind::Wifi
        {
            *prompt = Some(String::new());
            message.clear();
        }
    }

    /// Whether the picker is currently asking for a passphrase.
    pub fn is_prompting_wifi_passphrase(&self) -> bool {
        matches!(
            self,
            Self::ListPanel {
                prompt: Some(_),
                ..
            }
        )
    }

    /// Take the typed passphrase, clearing the prompt. The caller owns the
    /// only copy afterwards and is responsible for wiping it.
    pub fn take_wifi_passphrase(&mut self) -> Option<String> {
        let Self::ListPanel { prompt, .. } = self else {
            return None;
        };
        prompt.take()
    }

    /// Abandon the passphrase prompt, wiping what was typed.
    pub fn cancel_wifi_passphrase(&mut self) -> bool {
        let Self::ListPanel { prompt, .. } = self else {
            return false;
        };
        let Some(mut typed) = prompt.take() else {
            return false;
        };
        // Keep the optimizer from eliding the overwrite before dropping.
        unsafe { typed.as_bytes_mut().fill(0) };
        true
    }

    /// Replace the Wi-Fi picker's status line.
    pub fn set_wifi_message(&mut self, text: impl Into<String>) {
        self.set_list_message(ListKind::Wifi, text);
    }

    // --- Bluetooth picker ---

    /// Open the Bluetooth picker while its device list is being read.
    pub fn bluetooth_picker(message: impl Into<String>) -> Self {
        Self::open_list(ListKind::Bluetooth, message, "No remembered devices")
    }

    pub fn is_bluetooth_picker(&self) -> bool {
        self.is_list(ListKind::Bluetooth)
    }

    /// Fill in a finished device list, keeping the selection on the same
    /// device when it is still known.
    pub fn set_bluetooth_devices(&mut self, devices: &[crate::jwm::features::BluetoothDevice]) {
        let rows = devices
            .iter()
            .map(|device| ListRow {
                key: device.address.clone(),
                text: crate::jwm::features::connectivity::device_row(device),
                data: RowData::Bluetooth {
                    action: crate::jwm::features::connectivity::device_action(device),
                },
            })
            .collect();
        self.set_rows(ListKind::Bluetooth, rows);
    }

    /// The selected device: its address and what activating it would do.
    pub fn selected_bluetooth(&self) -> Option<(String, &'static str)> {
        let row = self.selected_row(ListKind::Bluetooth)?;
        match row.data {
            RowData::Bluetooth { action } => Some((row.key.clone(), action)),
            _ => None,
        }
    }

    /// Replace the Bluetooth picker's status line.
    pub fn set_bluetooth_message(&mut self, text: impl Into<String>) {
        self.set_list_message(ListKind::Bluetooth, text);
    }

    // --- Wallpaper picker ---

    /// Build the wallpaper picker from a directory listing.
    pub fn wallpaper_picker(paths: &[std::path::PathBuf], current: &str, directory: &str) -> Self {
        let rows: Vec<ListRow> = paths
            .iter()
            .map(|path| ListRow {
                key: path.to_string_lossy().into_owned(),
                text: crate::jwm::features::wallpaper::picker_row(path, current),
                data: RowData::Wallpaper,
            })
            .collect();
        // Start on the wallpaper already in use, so Escape-ing out of the
        // panel and reopening does not lose the user's place.
        let selected = rows.iter().position(|row| row.key == current).unwrap_or(0);
        Self::ListPanel {
            kind: ListKind::Wallpaper,
            rows,
            selected,
            message: String::new(),
            prompt: None,
            empty: format!("No images in {directory}"),
        }
    }

    pub fn is_wallpaper_picker(&self) -> bool {
        self.is_list(ListKind::Wallpaper)
    }

    /// The wallpaper the selection rests on.
    pub fn selected_wallpaper(&self) -> Option<&str> {
        Some(self.selected_row(ListKind::Wallpaper)?.key.as_str())
    }

    /// Build the session menu from what this machine can actually do.
    pub fn session_menu() -> Self {
        Self::SessionMenu {
            entries: crate::jwm::features::session::available_actions(
                crate::jwm::features::session::hibernate_supported(),
            ),
            selected: 0,
            armed: false,
        }
    }

    pub fn is_session_menu(&self) -> bool {
        matches!(self, Self::SessionMenu { .. })
    }

    /// Activate the selected row, returning the action only once it is
    /// confirmed. Destructive rows arm on the first Enter and run on the
    /// second, so a stray keystroke cannot end the session.
    pub fn activate_session_entry(&mut self) -> Option<crate::jwm::features::SessionAction> {
        let Self::SessionMenu {
            entries,
            selected,
            armed,
        } = self
        else {
            return None;
        };
        let action = *entries.get(*selected)?;
        if action.needs_confirmation() && !*armed {
            *armed = true;
            return None;
        }
        *armed = false;
        Some(action)
    }

    /// Render one control-center row. Rows whose content is not derived from
    /// `percent`/`enabled` carry it pre-rendered in `label`.
    fn control_row_text(entry: &ControlEntry) -> String {
        match entry.kind {
            ControlKind::Media
            | ControlKind::Battery
            | ControlKind::PowerProfile
            | ControlKind::Network
            | ControlKind::Bluetooth => entry.label.clone(),
            ControlKind::Volume => {
                let icon = if entry.enabled {
                    "\u{f026}" // fa-volume-off (muted)
                } else {
                    "\u{f028}" // fa-volume-up
                };
                let value = if entry.enabled {
                    "  mute".to_string()
                } else {
                    format!("{:>4}%", entry.percent)
                };
                format!(
                    "{icon}  Volume       {}  {value}",
                    slider_bar(if entry.enabled { 0 } else { entry.percent })
                )
            }
            ControlKind::Brightness => format!(
                "\u{f185}  Brightness   {}  {:>4}%",
                slider_bar(entry.percent),
                entry.percent
            ),
            ControlKind::NightLight => format!(
                "\u{f186}  Night Light{:>26}",
                if entry.enabled { "[ on ]" } else { "[ off ]" }
            ),
            ControlKind::DoNotDisturb => format!(
                "\u{f1f6}  Do Not Disturb{:>23}",
                if entry.enabled { "[ on ]" } else { "[ off ]" }
            ),
            ControlKind::LockScreen => "\u{f023}  Lock Screen".to_string(),
            ControlKind::Session => "\u{f011}  Session\u{2026}".to_string(),
        }
    }

    /// Activate the selected control, returning it only once confirmed.
    /// Rows that need confirming arm on the first Enter and fire on the
    /// second; everything else fires immediately.
    pub fn activate_control(&mut self) -> Option<ControlKind> {
        let Self::ControlCenter {
            entries,
            selected,
            armed,
        } = self
        else {
            return None;
        };
        let entry = entries.get(*selected)?;
        if needs_confirmation(entry.kind, entry.enabled) && !*armed {
            *armed = true;
            return None;
        }
        *armed = false;
        Some(entry.kind)
    }

    /// Whether the selected control row is armed for confirmation.
    pub fn control_is_armed(&self) -> bool {
        matches!(self, Self::ControlCenter { armed: true, .. })
    }

    /// Put the selection back on a rebuilt control center, clamped in case the
    /// row count shrank (a player that went away drops the media row).
    pub fn restore_control_selection(&mut self, previous: usize) {
        if let Self::ControlCenter {
            entries, selected, ..
        } = self
        {
            *selected = previous.min(entries.len().saturating_sub(1));
        }
    }

    /// The control the selection currently rests on, if the panel is open.
    pub fn selected_control(&self) -> Option<ControlKind> {
        let Self::ControlCenter {
            entries, selected, ..
        } = self
        else {
            return None;
        };
        entries.get(*selected).map(|entry| entry.kind)
    }

    /// Write back the live value of one control row after a side effect.
    pub fn update_control(&mut self, kind: ControlKind, percent: u8, enabled: bool) {
        if let Self::ControlCenter { entries, .. } = self {
            if let Some(entry) = entries.iter_mut().find(|entry| entry.kind == kind) {
                entry.percent = percent;
                entry.enabled = enabled;
            }
        }
    }

    pub fn info(title: impl Into<String>, lines: Vec<String>) -> Self {
        let matches = (0..lines.len()).collect();
        Self::Info {
            title: title.into(),
            lines,
            query: String::new(),
            matches,
            offset: 0,
        }
    }

    #[must_use]
    pub fn monitor_layout(mut entries: Vec<MonitorLayoutEntry>) -> Self {
        normalize_monitor_positions(&mut entries);
        Self::MonitorLayout {
            entries,
            selected: 0,
            reference: 1,
            message: String::new(),
        }
    }

    pub fn cycle_monitor(&mut self, delta: isize) {
        let Self::MonitorLayout {
            entries,
            selected,
            reference,
            message,
        } = self
        else {
            return;
        };
        if entries.len() < 2 {
            return;
        }
        let previous = *selected;
        *selected = cycle_index(*selected, entries.len(), delta);
        if *reference == *selected {
            *reference = previous;
        }
        message.clear();
    }

    pub fn cycle_monitor_reference(&mut self, delta: isize) {
        let Self::MonitorLayout {
            entries,
            selected,
            reference,
            message,
        } = self
        else {
            return;
        };
        if entries.len() < 2 {
            return;
        }
        loop {
            *reference = cycle_index(*reference, entries.len(), delta);
            if *reference != *selected {
                break;
            }
        }
        message.clear();
    }

    pub fn place_monitor(&mut self, direction: MonitorDirection) {
        let Self::MonitorLayout {
            entries,
            selected,
            reference,
            message,
        } = self
        else {
            return;
        };
        let Some(anchor) = entries.get(*reference).cloned() else {
            return;
        };
        let Some(target) = entries.get_mut(*selected) else {
            return;
        };
        match direction {
            MonitorDirection::Left => {
                target.x = anchor.x - target.width;
                target.y = anchor.y;
            }
            MonitorDirection::Right => {
                target.x = anchor.x + anchor.width;
                target.y = anchor.y;
            }
            MonitorDirection::Above => {
                target.x = anchor.x;
                target.y = anchor.y - target.height;
            }
            MonitorDirection::Below => {
                target.x = anchor.x;
                target.y = anchor.y + anchor.height;
            }
        }
        normalize_monitor_positions(entries);
        message.clear();
    }

    /// Move the selected monitor along the cross axis while preserving its
    /// attached side relative to the reference monitor.
    pub fn fine_tune_monitor(&mut self, direction: MonitorDirection, pixels: i32) {
        let Self::MonitorLayout {
            entries,
            selected,
            reference,
            message,
        } = self
        else {
            return;
        };
        let Some(anchor) = entries.get(*reference).cloned() else {
            return;
        };
        let Some(target_snapshot) = entries.get(*selected).cloned() else {
            return;
        };
        let Some(attachment) = monitor_attachment(&target_snapshot, &anchor) else {
            *message = "Place the target with an arrow key before fine tuning".into();
            return;
        };
        let Some(target) = entries.get_mut(*selected) else {
            return;
        };
        let pixels = pixels.max(1);
        let adjusted = match (attachment, direction) {
            (MonitorDirection::Left | MonitorDirection::Right, MonitorDirection::Above) => {
                target.y = target.y.saturating_sub(pixels);
                true
            }
            (MonitorDirection::Left | MonitorDirection::Right, MonitorDirection::Below) => {
                target.y = target.y.saturating_add(pixels);
                true
            }
            (MonitorDirection::Above | MonitorDirection::Below, MonitorDirection::Left) => {
                target.x = target.x.saturating_sub(pixels);
                true
            }
            (MonitorDirection::Above | MonitorDirection::Below, MonitorDirection::Right) => {
                target.x = target.x.saturating_add(pixels);
                true
            }
            (MonitorDirection::Left | MonitorDirection::Right, _) => {
                *message = "Left/right attachment is locked; fine-tune with Up/Down".into();
                false
            }
            (MonitorDirection::Above | MonitorDirection::Below, _) => {
                *message = "Above/below attachment is locked; fine-tune with Left/Right".into();
                false
            }
        };
        if adjusted {
            normalize_monitor_positions(entries);
            message.clear();
        }
    }

    pub fn align_monitor_start(&mut self) {
        self.align_monitor(MonitorAlignment::Start);
    }

    pub fn align_monitor_center(&mut self) {
        self.align_monitor(MonitorAlignment::Center);
    }

    pub fn align_monitor_end(&mut self) {
        self.align_monitor(MonitorAlignment::End);
    }

    fn align_monitor(&mut self, alignment: MonitorAlignment) {
        let Self::MonitorLayout {
            entries,
            selected,
            reference,
            message,
        } = self
        else {
            return;
        };
        let Some(anchor) = entries.get(*reference).cloned() else {
            return;
        };
        let Some(target_snapshot) = entries.get(*selected).cloned() else {
            return;
        };
        let Some(attachment) = monitor_attachment(&target_snapshot, &anchor) else {
            *message = "Place the target with an arrow key before aligning".into();
            return;
        };
        let Some(target) = entries.get_mut(*selected) else {
            return;
        };
        match attachment {
            MonitorDirection::Left | MonitorDirection::Right => {
                target.y = aligned_position(anchor.y, anchor.height, target.height, alignment);
            }
            MonitorDirection::Above | MonitorDirection::Below => {
                target.x = aligned_position(anchor.x, anchor.width, target.width, alignment);
            }
        }
        normalize_monitor_positions(entries);
        message.clear();
    }

    #[must_use]
    pub fn monitor_layout_xrandr_args(&self) -> Option<Vec<String>> {
        let Self::MonitorLayout { entries, .. } = self else {
            return None;
        };
        let mut args = Vec::with_capacity(entries.len() * 4);
        for entry in entries {
            args.push("--output".into());
            args.push(entry.name.clone());
            args.push("--pos".into());
            args.push(format!("{}x{}", entry.x, entry.y));
        }
        Some(args)
    }

    pub fn monitor_layout_error(&mut self, error: impl Into<String>) {
        if let Self::MonitorLayout { message, .. } = self {
            *message = error.into();
        }
    }

    pub fn push_char(&mut self, ch: char) {
        match self {
            Self::Launcher { query, .. } | Self::Info { query, .. } => query.push(ch),
            Self::ListPanel {
                prompt: Some(typed),
                message,
                ..
            } => {
                typed.push(ch);
                message.clear();
                return;
            }
            Self::Locked { password, message } => {
                password.push(ch);
                message.clear();
            }
            Self::Inactive
            | Self::MonitorLayout { .. }
            | Self::ControlCenter { .. }
            | Self::ListPanel { .. }
            | Self::Calendar { .. }
            | Self::SessionMenu { .. } => return,
        }
        self.refresh_matches();
    }

    pub fn backspace(&mut self) {
        match self {
            Self::Launcher { query, .. } | Self::Info { query, .. } => {
                query.pop();
            }
            Self::ListPanel {
                prompt: Some(typed),
                message,
                ..
            } => {
                typed.pop();
                message.clear();
                return;
            }
            Self::Locked { password, message } => {
                password.pop();
                message.clear();
            }
            Self::Inactive
            | Self::MonitorLayout { .. }
            | Self::ControlCenter { .. }
            | Self::ListPanel { .. }
            | Self::Calendar { .. }
            | Self::SessionMenu { .. } => return,
        }
        self.refresh_matches();
    }

    pub fn move_selection(&mut self, delta: isize) {
        if let Self::Launcher {
            matches, selected, ..
        } = self
        {
            if matches.is_empty() {
                *selected = 0;
                return;
            }
            *selected = (*selected as isize + delta).rem_euclid(matches.len() as isize) as usize;
        } else if let Self::Info {
            matches, offset, ..
        } = self
        {
            let max = matches.len().saturating_sub(28);
            *offset = (*offset as isize + delta).clamp(0, max as isize) as usize;
        } else if let Self::ControlCenter {
            entries,
            selected,
            armed,
        } = self
        {
            // Moving off an armed row cancels the confirmation: it belonged to
            // the row the user was looking at.
            *armed = false;
            if entries.is_empty() {
                *selected = 0;
                return;
            }
            *selected = (*selected as isize + delta).rem_euclid(entries.len() as isize) as usize;
        } else if let Self::ListPanel { rows, selected, .. } = self {
            if rows.is_empty() {
                *selected = 0;
                return;
            }
            *selected = (*selected as isize + delta).rem_euclid(rows.len() as isize) as usize;
        } else if let Self::SessionMenu {
            entries,
            selected,
            armed,
        } = self
        {
            // Moving off an armed row cancels it: the confirmation belongs to
            // the row the user was looking at.
            *armed = false;
            if entries.is_empty() {
                *selected = 0;
                return;
            }
            *selected = (*selected as isize + delta).rem_euclid(entries.len() as isize) as usize;
        }
    }

    pub fn selected_command(&self) -> Option<Vec<String>> {
        let Self::Launcher {
            entries,
            matches,
            selected,
            ..
        } = self
        else {
            return None;
        };
        entries
            .get(*matches.get(*selected)?)
            .map(|entry| entry.command.clone())
    }

    pub fn take_password(&mut self) -> Option<String> {
        let Self::Locked { password, message } = self else {
            return None;
        };
        message.clear();
        Some(std::mem::take(password))
    }

    pub fn authentication_failed(&mut self) {
        if let Self::Locked { password, message } = self {
            unsafe { password.as_bytes_mut().fill(0) };
            password.clear();
            *message = "Authentication failed".into();
        }
    }

    /// Structured overlay content the compositor renders as a styled panel:
    /// headline, optional search field, list rows with an optional highlighted
    /// row, and a footer hint.
    pub fn overlay_parts(&self) -> OverlayParts {
        match self {
            Self::Inactive => OverlayParts::default(),
            Self::Locked { password, message } => {
                let status = if message.is_empty() {
                    "Enter password to unlock"
                } else {
                    message
                };
                OverlayParts {
                    title: "\u{f023}  JWM LOCKED".into(),
                    query: None,
                    items: vec![
                        status.to_string(),
                        format!(
                            "\u{f084}  Password  {}",
                            "*".repeat(password.chars().count())
                        ),
                    ],
                    selected: None,
                    hint: "Enter  unlock    Esc  clear".into(),
                }
            }
            Self::Launcher {
                query,
                entries,
                matches,
                selected,
            } => {
                let start = selected.saturating_sub(11);
                let items: Vec<String> = if matches.is_empty() {
                    vec!["  No matching applications".into()]
                } else {
                    matches
                        .iter()
                        .skip(start)
                        .take(12)
                        .map(|&index| entries[index].name.clone())
                        .collect()
                };
                OverlayParts {
                    title: "\u{f135}  APPLICATIONS".into(),
                    query: Some(query.clone()),
                    selected: (!matches.is_empty()).then(|| selected - start),
                    items,
                    hint: "\u{f2f6} Enter  launch    Esc  close    \u{f062}/\u{f063}  select"
                        .into(),
                }
            }
            Self::Info {
                title,
                lines,
                query,
                matches,
                offset,
            } => {
                let items: Vec<String> = if matches.is_empty() {
                    vec!["  No matching shortcuts".into()]
                } else {
                    matches
                        .iter()
                        .skip(*offset)
                        .take(28)
                        .map(|&index| lines[index].clone())
                        .collect()
                };
                OverlayParts {
                    title: title.clone(),
                    query: Some(query.clone()),
                    items,
                    selected: None,
                    hint:
                        "Type  search    Backspace  erase    Esc  close    \u{f062}/\u{f063}  scroll"
                            .into(),
                }
            }
            Self::ControlCenter {
                entries,
                selected,
                armed,
            } => {
                let items = entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        let row = Self::control_row_text(entry);
                        if *armed && index == *selected {
                            format!("{row}   \u{2190} Enter to confirm")
                        } else {
                            row
                        }
                    })
                    .collect();
                OverlayParts {
                    title: "\u{f1de}  CONTROL CENTER".into(),
                    query: None,
                    items,
                    selected: Some((*selected).min(entries.len().saturating_sub(1))),
                    hint: if *armed {
                        "Enter  confirm    Esc  close".into()
                    } else {
                        "\u{f060}/\u{f061}  adjust    Enter  toggle    Esc  close".into()
                    },
                }
            }
            Self::ListPanel {
                kind,
                rows,
                selected,
                message,
                prompt,
                empty,
            } => {
                // One renderer for the notification center and the three
                // pickers: a scrolling window over the rows, then the status
                // line or the masked prompt underneath.
                let window = kind.window();
                let start = selected.saturating_sub(window.saturating_sub(1));
                let mut items: Vec<String> = if rows.is_empty() {
                    vec![format!(
                        "  {}",
                        if message.is_empty() { empty } else { message }
                    )]
                } else {
                    rows.iter()
                        .skip(start)
                        .take(window)
                        .map(|row| row.text.clone())
                        .collect()
                };
                if let Some(typed) = prompt {
                    items.push(String::new());
                    // Name the network: the selection highlight is dropped
                    // while prompting, so the row alone would not say which
                    // passphrase is being asked for.
                    let subject = rows
                        .get(*selected)
                        .map_or("network", |row| row.key.as_str());
                    items.push(format!(
                        "\u{f084}  Passphrase for {subject}  {}",
                        "*".repeat(typed.chars().count())
                    ));
                } else if !message.is_empty() && !rows.is_empty() {
                    items.push(String::new());
                    items.push(format!("  {message}"));
                }
                OverlayParts {
                    title: kind.title().to_string(),
                    query: None,
                    selected: (!rows.is_empty() && prompt.is_none()).then(|| selected - start),
                    items,
                    hint: kind.hint(prompt.is_some()).to_string(),
                }
            }
            Self::Calendar { view, clock } => {
                let mut items = vec![clock.clone(), String::new()];
                items.extend(crate::jwm::features::calendar::month_grid(view));
                OverlayParts {
                    title: format!("\u{f073}  {}", view.title()),
                    query: None,
                    items,
                    selected: None,
                    hint: "\u{f060}/\u{f061}  month    \u{f062}/\u{f063}  year    t  today    Esc  close"
                        .into(),
                }
            }
            Self::SessionMenu {
                entries,
                selected,
                armed,
            } => {
                let items = entries
                    .iter()
                    .enumerate()
                    .map(|(index, action)| {
                        crate::jwm::features::session::menu_row(
                            *action,
                            *armed && index == *selected,
                        )
                    })
                    .collect();
                let hint = if *armed {
                    "Enter  confirm    Esc  cancel".to_string()
                } else {
                    "Enter  select    \u{f062}/\u{f063}  move    Esc  close".to_string()
                };
                OverlayParts {
                    title: "\u{f011}  SESSION".into(),
                    query: None,
                    items,
                    selected: Some((*selected).min(entries.len().saturating_sub(1))),
                    hint,
                }
            }
            Self::MonitorLayout {
                entries,
                selected,
                reference,
                message,
            } => {
                let text = monitor_layout_overlay(entries, *selected, *reference, message);
                let mut lines = text.lines().map(str::to_string);
                let title = lines.next().unwrap_or_default();
                let mut items: Vec<String> = lines.collect();
                if items.first().is_some_and(String::is_empty) {
                    items.remove(0);
                }
                let hint = if items.len() >= 4 {
                    let tail = items.split_off(items.len() - 4);
                    while items.last().is_some_and(String::is_empty) {
                        items.pop();
                    }
                    tail.join("\n")
                } else {
                    String::new()
                };
                OverlayParts {
                    title,
                    query: None,
                    items,
                    selected: None,
                    hint,
                }
            }
        }
    }

    /// Flat-text form of [`Self::overlay_parts`]; the layout contract several
    /// tests (and any plain-text consumer) rely on.
    pub fn overlay_text(&self) -> String {
        if let Self::MonitorLayout {
            entries,
            selected,
            reference,
            message,
        } = self
        {
            return monitor_layout_overlay(entries, *selected, *reference, message);
        }
        let parts = self.overlay_parts();
        if !parts.title.is_empty() || !parts.items.is_empty() {
            let mut out = format!("{}\n\n", parts.title);
            if let Some(query) = &parts.query {
                let _ = writeln!(out, "\u{f002}  {query}_\n");
            }
            for (row, item) in parts.items.iter().enumerate() {
                let marker = if parts.selected == Some(row) {
                    "\u{f054}"
                } else {
                    " "
                };
                let _ = writeln!(out, "{marker} {item}");
            }
            let _ = write!(out, "\n{}", parts.hint);
            out
        } else {
            String::new()
        }
    }

    fn refresh_matches(&mut self) {
        match self {
            Self::Launcher {
                query,
                entries,
                matches,
                selected,
            } => {
                let needle = query.to_lowercase();
                let mut scored: Vec<(usize, usize)> = entries
                    .iter()
                    .enumerate()
                    .filter_map(|(i, entry)| {
                        fuzzy_score(&entry.search, &needle).map(|score| (i, score))
                    })
                    .collect();
                scored.sort_by_key(|&(i, score)| (Reverse(score), entries[i].name.to_lowercase()));
                *matches = scored.into_iter().map(|(i, _)| i).collect();
                *selected = 0;
            }
            Self::Info {
                query,
                lines,
                matches,
                offset,
                ..
            } => {
                let needle = query.to_lowercase();
                let mut scored: Vec<(usize, usize)> = lines
                    .iter()
                    .enumerate()
                    .filter_map(|(i, line)| {
                        fuzzy_score(&line.to_lowercase(), &needle).map(|score| (i, score))
                    })
                    .collect();
                scored.sort_by_key(|&(i, score)| (Reverse(score), i));
                *matches = scored.into_iter().map(|(i, _)| i).collect();
                *offset = 0;
            }
            Self::Inactive
            | Self::Locked { .. }
            | Self::MonitorLayout { .. }
            | Self::ControlCenter { .. }
            | Self::ListPanel { .. }
            | Self::Calendar { .. }
            | Self::SessionMenu { .. } => {}
        }
    }
}

fn cycle_index(index: usize, len: usize, delta: isize) -> usize {
    debug_assert!(len > 0);
    let distance = delta.unsigned_abs() % len;
    if delta.is_negative() {
        (index + len - distance) % len
    } else {
        (index + distance) % len
    }
}

fn normalize_monitor_positions(entries: &mut [MonitorLayoutEntry]) {
    let min_x = entries.iter().map(|entry| entry.x).min().unwrap_or(0);
    let min_y = entries.iter().map(|entry| entry.y).min().unwrap_or(0);
    if min_x == 0 && min_y == 0 {
        return;
    }
    for entry in entries {
        entry.x -= min_x;
        entry.y -= min_y;
    }
}

fn monitor_attachment(
    target: &MonitorLayoutEntry,
    anchor: &MonitorLayoutEntry,
) -> Option<MonitorDirection> {
    if target.x.saturating_add(target.width) == anchor.x {
        Some(MonitorDirection::Left)
    } else if target.x == anchor.x.saturating_add(anchor.width) {
        Some(MonitorDirection::Right)
    } else if target.y.saturating_add(target.height) == anchor.y {
        Some(MonitorDirection::Above)
    } else if target.y == anchor.y.saturating_add(anchor.height) {
        Some(MonitorDirection::Below)
    } else {
        None
    }
}

fn aligned_position(
    anchor_start: i32,
    anchor_size: i32,
    target_size: i32,
    alignment: MonitorAlignment,
) -> i32 {
    match alignment {
        MonitorAlignment::Start => anchor_start,
        MonitorAlignment::Center => {
            anchor_start.saturating_add(anchor_size.saturating_sub(target_size) / 2)
        }
        MonitorAlignment::End => anchor_start
            .saturating_add(anchor_size)
            .saturating_sub(target_size),
    }
}

fn monitor_attachment_summary(
    entries: &[MonitorLayoutEntry],
    selected: usize,
    reference: usize,
) -> Option<String> {
    let target = entries.get(selected)?;
    let anchor = entries.get(reference)?;
    let attachment = monitor_attachment(target, anchor)?;
    let (side, axis, offset) = match attachment {
        MonitorDirection::Left => ("left of", "vertical", target.y.saturating_sub(anchor.y)),
        MonitorDirection::Right => ("right of", "vertical", target.y.saturating_sub(anchor.y)),
        MonitorDirection::Above => ("above", "horizontal", target.x.saturating_sub(anchor.x)),
        MonitorDirection::Below => ("below", "horizontal", target.x.saturating_sub(anchor.x)),
    };
    Some(format!(
        "{} {side} {}; {axis} offset {offset:+} px",
        target.name, anchor.name
    ))
}

fn monitor_layout_overlay(
    entries: &[MonitorLayoutEntry],
    selected: usize,
    reference: usize,
    message: &str,
) -> String {
    let mut out = String::from("\u{f108}  DISPLAY LAYOUT\n\n");
    out.push_str(&monitor_layout_preview(entries, selected, reference));
    out.push('\n');
    if let Some(summary) = monitor_attachment_summary(entries, selected, reference) {
        writeln!(out, "\nLock: {summary}").expect("writing to a String cannot fail");
    }
    for (index, entry) in entries.iter().enumerate() {
        let target = if index == selected { '>' } else { ' ' };
        let anchor = if index == reference { '*' } else { ' ' };
        writeln!(
            out,
            "{target}{anchor} {}  {}x{}  @ {},{}",
            entry.name, entry.width, entry.height, entry.x, entry.y
        )
        .expect("writing to a String cannot fail");
    }
    if !message.is_empty() {
        writeln!(out, "\n! {message}").expect("writing to a String cannot fail");
    }
    out.push_str(
        "\nTab  target    [ / ]  reference    Arrow  attach side\nShift+Arrow  10px adjust    Ctrl+Arrow  1px adjust\nS / C / E  align start / center / end\nEnter  apply with xrandr    Esc  cancel",
    );
    out
}

fn monitor_layout_preview(
    entries: &[MonitorLayoutEntry],
    selected: usize,
    reference: usize,
) -> String {
    const WIDTH: usize = 52;
    const HEIGHT: usize = 10;
    let max_x = entries
        .iter()
        .map(|entry| entry.x.saturating_add(entry.width.max(1)))
        .max()
        .unwrap_or(1)
        .max(1);
    let max_y = entries
        .iter()
        .map(|entry| entry.y.saturating_add(entry.height.max(1)))
        .max()
        .unwrap_or(1)
        .max(1);
    let mut canvas = vec![vec![' '; WIDTH]; HEIGHT];

    // Draw the selected output last so its outline remains visible when the
    // current layout contains mirrored/overlapping outputs.
    let order = (0..entries.len())
        .filter(|&i| i != selected)
        .chain((selected < entries.len()).then_some(selected));
    for index in order {
        let entry = &entries[index];
        let x0 = scale_preview(entry.x, max_x, WIDTH);
        let y0 = scale_preview(entry.y, max_y, HEIGHT);
        let mut x1 = scale_preview(entry.x.saturating_add(entry.width), max_x, WIDTH);
        let mut y1 = scale_preview(entry.y.saturating_add(entry.height), max_y, HEIGHT);
        x1 = x1.max((x0 + 5).min(WIDTH - 1)).min(WIDTH - 1);
        y1 = y1.max((y0 + 2).min(HEIGHT - 1)).min(HEIGHT - 1);
        let horizontal = if index == selected { '=' } else { '-' };
        let vertical = if index == selected { '#' } else { '|' };
        canvas[y0][x0..=x1].fill(horizontal);
        canvas[y1][x0..=x1].fill(horizontal);
        for row in canvas.iter_mut().take(y1 + 1).skip(y0) {
            row[x0] = vertical;
            row[x1] = vertical;
        }
        for &(x, y) in &[(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
            canvas[y][x] = '+';
        }
        let marker = if index == selected {
            '>'
        } else if index == reference {
            '*'
        } else {
            char::from_digit(u32::try_from((index + 1).min(9)).unwrap_or(9), 10).unwrap_or('?')
        };
        let label = format!("{marker}{}", entry.name);
        for (offset, ch) in label.chars().take(x1.saturating_sub(x0 + 1)).enumerate() {
            canvas[(y0 + 1).min(y1)][x0 + 1 + offset] = ch;
        }
    }

    canvas
        .into_iter()
        .map(|row| row.into_iter().collect::<String>().trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn scale_preview(value: i32, max: i32, extent: usize) -> usize {
    let extent_max = extent.saturating_sub(1);
    let value = u64::try_from(value.max(0)).unwrap_or(0);
    let max = u64::try_from(max.max(1)).unwrap_or(1);
    let extent_max_u64 = u64::try_from(extent_max).unwrap_or(u64::MAX);
    usize::try_from(value.saturating_mul(extent_max_u64) / max).unwrap_or(extent_max)
}

fn fuzzy_score(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if let Some(pos) = haystack.find(needle) {
        return Some(10_000 - pos);
    }
    let mut at = 0;
    let mut score = 0;
    for ch in needle.chars() {
        let rel = haystack[at..].find(ch)?;
        at += rel + ch.len_utf8();
        score += 100usize.saturating_sub(rel);
    }
    Some(score)
}

fn discover_applications() -> Vec<LaunchEntry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::data_dir())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let mut roots = vec![data_home.join("applications")];
    let data_dirs =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    roots.extend(
        data_dirs
            .split(':')
            .map(|p| Path::new(p).join("applications")),
    );
    for root in roots {
        scan_desktop_dir(&root, &mut entries, &mut seen);
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let Ok(items) = fs::read_dir(dir) else {
                continue;
            };
            for item in items.flatten() {
                let name = item.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || !seen.insert(name.clone()) {
                    continue;
                }
                let Ok(meta) = item.metadata() else { continue };
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
                        continue;
                    }
                }
                entries.push(LaunchEntry {
                    search: name.to_lowercase(),
                    name: name.clone(),
                    command: vec![name],
                });
            }
        }
    }
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    entries
}

fn scan_desktop_dir(root: &Path, entries: &mut Vec<LaunchEntry>, seen: &mut HashSet<String>) {
    let Ok(items) = fs::read_dir(root) else {
        return;
    };
    for item in items.flatten() {
        let path = item.path();
        if path.is_dir() {
            scan_desktop_dir(&path, entries, seen);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("desktop") {
            continue;
        }
        let Ok(body) = fs::read_to_string(path) else {
            continue;
        };
        let mut in_entry = false;
        let mut name = None;
        let mut exec = None;
        let mut hidden = false;
        for line in body.lines() {
            if line.starts_with('[') {
                in_entry = line == "[Desktop Entry]";
                continue;
            }
            if !in_entry {
                continue;
            }
            if let Some(v) = line.strip_prefix("Name=") {
                name.get_or_insert_with(|| v.to_string());
            }
            if let Some(v) = line.strip_prefix("Exec=") {
                exec = Some(v.to_string());
            }
            if matches!(line, "Hidden=true" | "NoDisplay=true") {
                hidden = true;
            }
        }
        let (Some(name), Some(exec)) = (name, exec) else {
            continue;
        };
        if hidden || !seen.insert(name.clone()) {
            continue;
        }
        let command = parse_exec(&exec);
        if command.is_empty() {
            continue;
        }
        entries.push(LaunchEntry {
            search: format!("{} {}", name.to_lowercase(), exec.to_lowercase()),
            name,
            command,
        });
    }
}

fn parse_exec(exec: &str) -> Vec<String> {
    // Desktop Exec quoting is deliberately small but handles the common quoted
    // argv form. Field codes are omitted because no files/URLs were supplied.
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for ch in exec.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None
            } else {
                current.push(ch)
            };
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            continue;
        }
        if ch.is_whitespace() {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args.into_iter()
        .filter(|arg| !arg.starts_with('%'))
        .collect()
}

// Minimal dynamically-loaded PAM client. dlopen keeps builds working on
// machines that have the PAM runtime (needed to log in) but not libpam headers.
pub fn authenticate_current_user(password: &str) -> bool {
    unsafe { authenticate_pam(password).unwrap_or(false) }
}

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}
#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}
#[repr(C)]
struct PamConv {
    conv: Option<
        unsafe extern "C" fn(
            c_int,
            *mut *const PamMessage,
            *mut *mut PamResponse,
            *mut c_void,
        ) -> c_int,
    >,
    appdata_ptr: *mut c_void,
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe extern "C" fn pam_conversation(
    n: c_int,
    messages: *mut *const PamMessage,
    responses: *mut *mut PamResponse,
    data: *mut c_void,
) -> c_int {
    if n <= 0 || messages.is_null() || responses.is_null() {
        return 19;
    }
    let password = &*(data as *const CString);
    let out = libc::calloc(n as usize, std::mem::size_of::<PamResponse>()) as *mut PamResponse;
    if out.is_null() {
        return 5;
    }
    for i in 0..n as isize {
        let message = *messages.offset(i);
        if message.is_null() {
            libc::free(out.cast());
            return 19;
        }
        let value = match (*message).msg_style {
            1 => password.as_ptr(),
            2 => b"\0".as_ptr().cast(),
            3 | 4 => b"\0".as_ptr().cast(),
            _ => {
                libc::free(out.cast());
                return 19;
            }
        };
        (*out.offset(i)).resp = libc::strdup(value);
    }
    *responses = out;
    0
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn authenticate_pam(password: &str) -> Result<bool, ()> {
    let lib = libc::dlopen(c"libpam.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
    if lib.is_null() {
        return Err(());
    }
    macro_rules! sym {
        ($name:literal, $ty:ty) => {{
            let p = libc::dlsym(lib, concat!($name, "\0").as_ptr().cast());
            if p.is_null() {
                libc::dlclose(lib);
                return Err(());
            }
            std::mem::transmute::<*mut c_void, $ty>(p)
        }};
    }
    type Start = unsafe extern "C" fn(
        *const c_char,
        *const c_char,
        *const PamConv,
        *mut *mut c_void,
    ) -> c_int;
    type Auth = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
    type End = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
    let start: Start = sym!("pam_start", Start);
    let auth: Auth = sym!("pam_authenticate", Auth);
    let end: End = sym!("pam_end", End);
    let pw = libc::getpwuid(libc::getuid());
    if pw.is_null() {
        libc::dlclose(lib);
        return Err(());
    }
    let user = CStr::from_ptr((*pw).pw_name);
    let password = CString::new(password).map_err(|_| ())?;
    let conv = PamConv {
        conv: Some(pam_conversation),
        appdata_ptr: (&password as *const CString).cast_mut().cast(),
    };
    let mut handle = std::ptr::null_mut();
    let mut result = start(c"login".as_ptr(), user.as_ptr(), &conv, &mut handle);
    if result == 0 {
        result = auth(handle, 0);
    }
    if !handle.is_null() {
        end(handle, result);
    }
    libc::dlclose(lib);
    Ok(result == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_center_builds_rows_for_available_controls() {
        // Volume and brightness present, DND on, night light off.
        let state = SystemUiState::control_center(&ControlCenterInputs {
            volume: Some((45, false)),
            brightness: Some(60),
            do_not_disturb: true,
            ..Default::default()
        });
        assert_eq!(state.selected_control(), Some(ControlKind::Volume));
        let parts = state.overlay_parts();
        // volume, brightness, night light, DND, lock, session
        assert_eq!(parts.items.len(), 6);
        assert!(parts.items[0].contains("45%"));
        assert!(parts.items[1].contains("60%"));
        assert!(parts.items[2].contains("[ off ]"), "night light off");
        assert!(parts.items[3].contains("[ on ]"), "DND on");

        // No audio, no backlight: only the toggles and actions remain.
        let state = SystemUiState::control_center(&ControlCenterInputs::default());
        assert_eq!(state.selected_control(), Some(ControlKind::NightLight));
        assert_eq!(state.overlay_parts().items.len(), 4);
    }

    #[test]
    fn a_destructive_session_row_runs_only_on_the_second_enter() {
        use crate::jwm::features::SessionAction;

        let mut menu = SystemUiState::SessionMenu {
            entries: vec![SessionAction::Lock, SessionAction::Shutdown],
            selected: 1,
            armed: false,
        };

        // First Enter arms and says so; nothing runs yet.
        assert_eq!(menu.activate_session_entry(), None);
        assert!(menu.overlay_parts().items[1].contains("Enter to confirm"));
        assert!(menu.overlay_parts().hint.contains("confirm"));

        // Second Enter runs it and disarms.
        assert_eq!(menu.activate_session_entry(), Some(SessionAction::Shutdown));
        assert!(!menu.overlay_parts().items[1].contains("confirm"));
    }

    #[test]
    fn a_recoverable_session_row_runs_immediately() {
        use crate::jwm::features::SessionAction;

        let mut menu = SystemUiState::SessionMenu {
            entries: vec![SessionAction::Lock, SessionAction::Suspend],
            selected: 1,
            armed: false,
        };
        assert_eq!(menu.activate_session_entry(), Some(SessionAction::Suspend));
    }

    #[test]
    fn moving_off_an_armed_session_row_cancels_the_confirmation() {
        use crate::jwm::features::SessionAction;

        let mut menu = SystemUiState::SessionMenu {
            entries: vec![SessionAction::Reboot, SessionAction::Shutdown],
            selected: 0,
            armed: false,
        };
        assert_eq!(menu.activate_session_entry(), None);
        menu.move_selection(1);
        menu.move_selection(-1);

        // Back on the same row, but disarmed: it must arm again, not run.
        assert!(!menu.overlay_parts().items[0].contains("confirm"));
        assert_eq!(menu.activate_session_entry(), None);
    }

    #[test]
    fn an_empty_session_menu_activates_nothing() {
        let mut menu = SystemUiState::SessionMenu {
            entries: Vec::new(),
            selected: 0,
            armed: false,
        };
        assert_eq!(menu.activate_session_entry(), None);
        menu.move_selection(1);
        assert!(menu.is_session_menu());
    }

    fn wifi(ssid: &str, open: bool) -> crate::jwm::features::WifiNetwork {
        crate::jwm::features::WifiNetwork {
            ssid: ssid.to_string(),
            signal: 70,
            security: if open { String::new() } else { "WPA2".into() },
            in_use: false,
        }
    }

    #[test]
    fn a_refresh_holds_the_selection_on_the_same_row() {
        let mut panel = SystemUiState::wifi_picker("Scanning");
        panel.set_wifi_networks(&[wifi("Alpha", false), wifi("Beta", false)]);
        panel.move_selection(1);
        assert_eq!(
            panel.selected_wifi().map(|(ssid, _)| ssid).as_deref(),
            Some("Beta")
        );

        // A rescan that reorders the list must not move the user's selection.
        panel.set_wifi_networks(&[
            wifi("Gamma", false),
            wifi("Beta", false),
            wifi("Alpha", false),
        ]);
        assert_eq!(
            panel.selected_wifi().map(|(ssid, _)| ssid).as_deref(),
            Some("Beta")
        );
    }

    #[test]
    fn a_refresh_that_drops_the_selected_row_falls_back_to_the_top() {
        let mut panel = SystemUiState::wifi_picker("Scanning");
        panel.set_wifi_networks(&[wifi("Alpha", false), wifi("Beta", false)]);
        panel.move_selection(1);

        panel.set_wifi_networks(&[wifi("Gamma", false)]);
        assert_eq!(
            panel.selected_wifi().map(|(ssid, _)| ssid).as_deref(),
            Some("Gamma")
        );
    }

    #[test]
    fn the_shared_renderer_scrolls_to_keep_the_selection_visible() {
        let mut panel = SystemUiState::wifi_picker("Scanning");
        let networks: Vec<_> = (0..30)
            .map(|i| wifi(&format!("net{i:02}"), false))
            .collect();
        panel.set_wifi_networks(&networks);
        for _ in 0..20 {
            panel.move_selection(1);
        }
        let parts = panel.overlay_parts();

        // The window follows the selection rather than showing the top.
        assert!(parts.items.iter().any(|row| row.contains("net20")));
        assert!(!parts.items.iter().any(|row| row.contains("net00")));
        // And the highlight points inside the window that was drawn.
        assert!(
            parts
                .selected
                .is_some_and(|index| index < parts.items.len())
        );
    }

    #[test]
    fn each_list_kind_keeps_its_own_title_and_hint() {
        assert!(
            SystemUiState::wifi_picker("")
                .overlay_parts()
                .title
                .contains("WI-FI")
        );
        assert!(
            SystemUiState::bluetooth_picker("")
                .overlay_parts()
                .hint
                .contains("connect/disconnect")
        );
        assert!(
            SystemUiState::wallpaper_picker(&[], "", "/walls")
                .overlay_parts()
                .items[0]
                .contains("/walls")
        );
    }

    #[test]
    fn a_prompt_hides_the_row_highlight_and_masks_what_is_typed() {
        let mut panel = SystemUiState::wifi_picker("");
        panel.set_wifi_networks(&[wifi("Alpha", false)]);
        panel.prompt_wifi_passphrase();
        // Characters that cannot appear in the surrounding label, so the
        // assertion below really is about the mask.
        panel.push_char('x');
        panel.push_char('q');
        let parts = panel.overlay_parts();

        assert!(parts.selected.is_none(), "no row highlight while prompting");
        let prompt = parts.items.last().expect("prompt row");
        assert!(prompt.contains("Alpha"), "the prompt names the network");
        assert!(prompt.contains("**"));
        assert!(
            !prompt.contains('x') && !prompt.contains('q'),
            "the passphrase itself is never drawn"
        );

        assert_eq!(panel.take_wifi_passphrase().as_deref(), Some("xq"));
    }

    #[test]
    fn cancelling_a_prompt_keeps_the_list() {
        let mut panel = SystemUiState::wifi_picker("");
        panel.set_wifi_networks(&[wifi("Alpha", false)]);
        panel.prompt_wifi_passphrase();

        assert!(panel.cancel_wifi_passphrase());
        assert!(!panel.is_prompting_wifi_passphrase());
        assert!(panel.is_wifi_picker(), "the picker stays open");
        // Nothing to cancel the second time.
        assert!(!panel.cancel_wifi_passphrase());
    }

    #[test]
    fn a_list_panel_answers_only_to_its_own_kind() {
        let panel = SystemUiState::wifi_picker("");
        assert!(panel.is_wifi_picker());
        assert!(!panel.is_bluetooth_picker());
        assert!(!panel.is_notification_center());
        assert!(!panel.is_wallpaper_picker());
        assert!(panel.selected_bluetooth().is_none());
        assert!(panel.selected_wallpaper().is_none());
        assert!(panel.selected_notification().is_none());
    }

    #[test]
    fn switching_bluetooth_off_needs_a_second_enter() {
        let powered = crate::jwm::features::BluetoothState {
            present: true,
            powered: true,
        };
        let mut panel = SystemUiState::control_center(&ControlCenterInputs {
            bluetooth: Some(&powered),
            ..Default::default()
        });
        assert_eq!(panel.selected_control(), Some(ControlKind::Bluetooth));

        // First Enter arms and says so; nothing is switched.
        assert_eq!(panel.activate_control(), None);
        assert!(panel.control_is_armed());
        assert!(panel.overlay_parts().items[0].contains("Enter to confirm"));
        assert!(panel.overlay_parts().hint.contains("confirm"));

        // Second Enter goes through.
        assert_eq!(panel.activate_control(), Some(ControlKind::Bluetooth));
        assert!(!panel.control_is_armed());
    }

    #[test]
    fn switching_bluetooth_on_is_immediate() {
        let off = crate::jwm::features::BluetoothState {
            present: true,
            powered: false,
        };
        let mut panel = SystemUiState::control_center(&ControlCenterInputs {
            bluetooth: Some(&off),
            ..Default::default()
        });
        // Nothing to strand: turning the radio on cannot cost the user keys.
        assert_eq!(panel.activate_control(), Some(ControlKind::Bluetooth));
    }

    #[test]
    fn moving_off_an_armed_control_cancels_it() {
        let powered = crate::jwm::features::BluetoothState {
            present: true,
            powered: true,
        };
        let mut panel = SystemUiState::control_center(&ControlCenterInputs {
            bluetooth: Some(&powered),
            ..Default::default()
        });
        assert_eq!(panel.activate_control(), None);
        panel.move_selection(1);
        panel.move_selection(-1);

        assert!(!panel.control_is_armed());
        assert_eq!(
            panel.activate_control(),
            None,
            "it must arm again, not fire"
        );
    }

    #[test]
    fn ordinary_toggles_never_ask_for_confirmation() {
        for (kind, enabled) in [
            (ControlKind::NightLight, true),
            (ControlKind::DoNotDisturb, true),
            (ControlKind::LockScreen, false),
            (ControlKind::Session, false),
            (ControlKind::Volume, true),
            (ControlKind::Network, true),
        ] {
            assert!(
                !needs_confirmation(kind, enabled),
                "{kind:?} must not need confirming"
            );
        }
    }

    #[test]
    fn connectivity_rows_appear_only_with_the_hardware() {
        let network = crate::jwm::features::NetworkState {
            wifi_enabled: true,
            connection: Some("ENGINEAI".to_string()),
            kind: crate::jwm::features::LinkKind::Wireless,
            signal: Some(72),
        };
        let bluetooth = crate::jwm::features::BluetoothState {
            present: true,
            powered: true,
        };
        let state = SystemUiState::control_center(&ControlCenterInputs {
            network: Some(&network),
            bluetooth: Some(&bluetooth),
            ..Default::default()
        });
        let parts = state.overlay_parts();

        assert_eq!(state.selected_control(), Some(ControlKind::Network));
        assert!(parts.items[0].contains("ENGINEAI"));
        assert!(parts.items[0].contains("72%"));
        assert!(parts.items[1].contains("Bluetooth"));

        // A controller-less machine hides the Bluetooth row entirely.
        let no_controller = SystemUiState::control_center(&ControlCenterInputs {
            network: Some(&network),
            bluetooth: Some(&crate::jwm::features::BluetoothState::default()),
            ..Default::default()
        });
        assert!(
            !no_controller
                .overlay_parts()
                .items
                .iter()
                .any(|row| row.contains("Bluetooth"))
        );
    }

    #[test]
    fn battery_and_profile_rows_appear_only_with_the_hardware() {
        let battery = crate::jwm::features::BatteryState {
            percent: 64,
            status: crate::jwm::features::ChargeStatus::Discharging,
            time_remaining_mins: Some(95),
        };
        let state = SystemUiState::control_center(&ControlCenterInputs {
            battery: Some(&battery),
            power_profile: Some("balanced"),
            ..Default::default()
        });
        let parts = state.overlay_parts();

        assert_eq!(state.selected_control(), Some(ControlKind::Battery));
        assert!(parts.items[0].contains("64%"));
        assert!(parts.items[0].contains("1h 35m left"));
        assert!(parts.items[1].contains("balanced"));

        // A desktop with neither shows neither row.
        let bare = SystemUiState::control_center(&ControlCenterInputs::default());
        let items = bare.overlay_parts().items;
        assert!(!items.iter().any(|row| row.contains("Battery")));
        assert!(!items.iter().any(|row| row.contains("Power Profile")));
    }

    #[test]
    fn the_night_light_row_reflects_the_live_state() {
        let state = SystemUiState::control_center(&ControlCenterInputs {
            night_light: true,
            ..Default::default()
        });
        assert!(state.overlay_parts().items[0].contains("[ on ]"));
    }

    #[test]
    fn a_running_player_adds_the_media_row_on_top() {
        let media = crate::jwm::features::MediaState {
            player: "spotify".into(),
            identity: "Spotify".into(),
            status: crate::jwm::features::PlaybackStatus::Playing,
            title: "Blue in Green".into(),
            artist: "Miles Davis".into(),
            can_go_next: true,
            can_go_previous: true,
        };
        let state = SystemUiState::control_center(&ControlCenterInputs {
            media: Some(&media),
            volume: Some((45, false)),
            ..Default::default()
        });

        assert_eq!(state.selected_control(), Some(ControlKind::Media));
        let parts = state.overlay_parts();
        assert!(parts.items[0].contains("Blue in Green"));
        assert!(parts.items[1].contains("45%"));
    }

    #[test]
    fn restoring_the_selection_clamps_when_rows_disappear() {
        // Selection sat past the end of a shorter, rebuilt panel; it must land
        // on the last row instead of pointing nowhere.
        let mut rebuilt = SystemUiState::control_center(&ControlCenterInputs::default());
        rebuilt.restore_control_selection(9);
        assert_eq!(rebuilt.selected_control(), Some(ControlKind::Session));
    }

    #[test]
    fn control_center_selection_wraps_and_updates_write_back() {
        let mut state = SystemUiState::control_center(&ControlCenterInputs {
            volume: Some((45, false)),
            brightness: Some(60),
            ..Default::default()
        });
        state.move_selection(-1);
        assert_eq!(state.selected_control(), Some(ControlKind::Session));
        state.move_selection(1);
        assert_eq!(state.selected_control(), Some(ControlKind::Volume));

        state.update_control(ControlKind::Volume, 50, true);
        let parts = state.overlay_parts();
        assert!(parts.items[0].contains("mute"));
        // A muted slider renders an empty bar.
        assert!(!parts.items[0].contains('\u{2588}'));
    }

    #[test]
    fn slider_bar_is_twenty_cells() {
        for percent in [0u8, 45, 100] {
            assert_eq!(slider_bar(percent).chars().count(), 20);
        }
        assert_eq!(slider_bar(0).matches('\u{2588}').count(), 0);
        assert_eq!(slider_bar(100).matches('\u{2588}').count(), 20);
        assert_eq!(slider_bar(50).matches('\u{2588}').count(), 10);
    }

    #[test]
    fn control_center_ignores_text_input() {
        let mut state = SystemUiState::control_center(&ControlCenterInputs {
            volume: Some((45, false)),
            ..Default::default()
        });
        state.push_char('x');
        state.backspace();
        assert_eq!(state.selected_control(), Some(ControlKind::Volume));
        assert!(state.is_active());
        state.cancel();
        assert!(!state.is_active());
    }

    #[test]
    fn parses_desktop_exec() {
        assert_eq!(
            parse_exec("foo --name 'two words' %U"),
            ["foo", "--name", "two words"]
        );
    }
    #[test]
    fn fuzzy_matching() {
        assert!(fuzzy_score("visual studio code", "vsc").is_some());
        assert!(fuzzy_score("firefox", "zzz").is_none());
    }

    #[test]
    fn launcher_overlay_parts_window_the_list_and_track_selection() {
        let entries: Vec<LaunchEntry> = (0..20)
            .map(|i| LaunchEntry {
                name: format!("app{i:02}"),
                command: vec![format!("app{i:02}")],
                search: format!("app{i:02}"),
            })
            .collect();
        let matches = (0..entries.len()).collect();
        let mut state = SystemUiState::Launcher {
            query: String::new(),
            entries,
            matches,
            selected: 0,
        };

        let parts = state.overlay_parts();
        assert_eq!(parts.title, "\u{f135}  APPLICATIONS");
        assert_eq!(parts.query.as_deref(), Some(""));
        assert_eq!(parts.items.len(), 12);
        assert_eq!(parts.items[0], "app00");
        assert_eq!(parts.selected, Some(0));
        assert!(parts.hint.contains("Enter"));

        // Move past the visible window: the list scrolls and the highlighted
        // row stays inside the visible slice.
        for _ in 0..14 {
            state.move_selection(1);
        }
        let parts = state.overlay_parts();
        assert_eq!(parts.items.len(), 12);
        assert_eq!(parts.items[0], "app03");
        assert_eq!(parts.selected, Some(11));
        assert_eq!(parts.items[11], "app14");
    }

    #[test]
    fn locked_overlay_parts_mask_the_password() {
        let mut state = SystemUiState::lock();
        for ch in "hunter2".chars() {
            state.push_char(ch);
        }
        let parts = state.overlay_parts();
        assert!(state.is_locked());
        assert!(parts.items.iter().any(|line| line.contains(&"*".repeat(7))));
        assert!(!parts.items.iter().any(|line| line.contains("hunter2")));
    }

    #[test]
    fn info_search_filters_shortcut_and_description() {
        let mut state = SystemUiState::info(
            "KEYS",
            vec![
                "Mod1+j  focus next".into(),
                "Mod1+Return  terminal".into(),
                "Mod1+b  toggle bar".into(),
            ],
        );
        for ch in "term".chars() {
            state.push_char(ch);
        }
        let text = state.overlay_text();
        assert!(text.contains("Mod1+Return  terminal"));
        assert!(!text.contains("focus next"));
        state.backspace();
        assert!(state.overlay_text().contains("ter_"));
    }

    fn monitor(name: &str, x: i32, y: i32, width: i32, height: i32) -> MonitorLayoutEntry {
        MonitorLayoutEntry {
            name: name.into(),
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn monitor_layout_places_target_relative_to_reference() {
        let mut state = SystemUiState::monitor_layout(vec![
            monitor("eDP-1", 0, 0, 1920, 1080),
            monitor("HDMI-1", 0, 0, 2560, 1440),
        ]);

        state.place_monitor(MonitorDirection::Left);

        assert_eq!(
            state.monitor_layout_xrandr_args().unwrap(),
            [
                "--output", "eDP-1", "--pos", "0x0", "--output", "HDMI-1", "--pos", "1920x0",
            ]
        );
    }

    #[test]
    fn monitor_layout_cycles_target_without_using_it_as_reference() {
        let mut state = SystemUiState::monitor_layout(vec![
            monitor("one", 0, 0, 100, 100),
            monitor("two", 100, 0, 100, 100),
            monitor("three", 200, 0, 100, 100),
        ]);

        state.cycle_monitor(1);
        state.place_monitor(MonitorDirection::Below);

        let text = state.overlay_text();
        assert!(text.contains(" * one  100x100  @ 0,0"));
        assert!(text.contains(">  two  100x100  @ 0,100"));
    }

    #[test]
    fn monitor_layout_keeps_horizontal_attachment_while_adjusting_vertical_offset() {
        let mut state = SystemUiState::monitor_layout(vec![
            monitor("eDP-1", 0, 0, 1920, 1080),
            monitor("HDMI-1", 0, 0, 2560, 1440),
        ]);

        state.place_monitor(MonitorDirection::Right);
        state.fine_tune_monitor(MonitorDirection::Below, 10);
        state.fine_tune_monitor(MonitorDirection::Below, 1);

        assert_eq!(
            state.monitor_layout_xrandr_args().unwrap(),
            [
                "--output", "eDP-1", "--pos", "2560x11", "--output", "HDMI-1", "--pos", "0x0",
            ]
        );
        assert!(state.overlay_text().contains("vertical offset +11 px"));
    }

    #[test]
    fn monitor_layout_centers_different_height_outputs_on_cross_axis() {
        let mut state = SystemUiState::monitor_layout(vec![
            monitor("eDP-1", 0, 0, 1920, 1080),
            monitor("HDMI-1", 0, 0, 2560, 1440),
        ]);

        state.place_monitor(MonitorDirection::Right);
        state.align_monitor_center();

        assert_eq!(
            state.monitor_layout_xrandr_args().unwrap(),
            [
                "--output", "eDP-1", "--pos", "2560x180", "--output", "HDMI-1", "--pos", "0x0",
            ]
        );
    }

    #[test]
    fn monitor_layout_rejects_adjustment_that_breaks_locked_axis() {
        let mut state = SystemUiState::monitor_layout(vec![
            monitor("one", 0, 0, 100, 100),
            monitor("two", 100, 0, 100, 100),
        ]);

        state.place_monitor(MonitorDirection::Left);
        let before = state.monitor_layout_xrandr_args();
        state.fine_tune_monitor(MonitorDirection::Left, 10);

        assert_eq!(state.monitor_layout_xrandr_args(), before);
        assert!(state.overlay_text().contains("fine-tune with Up/Down"));
    }

    #[test]
    fn monitor_layout_preview_marks_target_and_reference() {
        let state = SystemUiState::monitor_layout(vec![
            monitor("eDP-1", 0, 0, 1920, 1080),
            monitor("HDMI-1", 1920, 0, 2560, 1440),
        ]);
        let text = state.overlay_text();

        assert!(text.contains("DISPLAY LAYOUT"));
        assert!(text.contains(">  eDP-1"));
        assert!(text.contains(" * HDMI-1"));
        assert!(text.contains("apply with xrandr"));
    }
}
