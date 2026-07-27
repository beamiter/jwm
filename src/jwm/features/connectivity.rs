//! Wi-Fi and Bluetooth state for the control center.
//!
//! Same shape as [`system_controls`](super::system_controls) and
//! [`power`](super::power): shell out to whatever the session actually runs —
//! `nmcli` then `rfkill` for the wireless radio, `bluetoothctl` then `rfkill`
//! for Bluetooth — cache the tool that answered, and keep every parser pure so
//! the output formats are pinned by tests on machines that have neither.

use std::process::Command;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkKind {
    Wireless,
    Wired,
    #[default]
    None,
}

impl LinkKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wireless => "wireless",
            Self::Wired => "wired",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkState {
    /// Whether the Wi-Fi radio is switched on.
    pub wifi_enabled: bool,
    /// Name of the active connection, when there is one.
    pub connection: Option<String>,
    pub kind: LinkKind,
    /// Signal strength 0..=100 for a wireless link.
    pub signal: Option<u8>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BluetoothState {
    /// Whether a controller exists at all; without one the row is hidden.
    pub present: bool,
    pub powered: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectivityState {
    /// `None` when this machine has no wireless radio.
    pub network: Option<NetworkState>,
    pub bluetooth: BluetoothState,
}

// ---------------------------------------------------------------------------
// Parsers (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Split one `nmcli -t` record. Fields are colon-separated, and nmcli escapes
/// a literal colon or backslash inside a field with a backslash — an SSID
/// containing `:` would otherwise split into two fields.
#[must_use]
pub fn split_nmcli_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            ':' => fields.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    fields.push(current);
    fields
}

/// `nmcli radio wifi` → `enabled` / `disabled`.
#[must_use]
pub fn parse_radio(output: &str) -> Option<bool> {
    match output.trim() {
        "enabled" => Some(true),
        "disabled" => Some(false),
        _ => None,
    }
}

/// Pick the interesting active connection from
/// `nmcli -t -f NAME,TYPE,DEVICE connection show --active`.
///
/// A wireless link wins over a wired one; virtual plumbing (bridges, tunnels,
/// loopback) is never what the user means by "connected".
#[must_use]
pub fn parse_active_connection(output: &str) -> Option<(String, LinkKind)> {
    let mut wired = None;
    for line in output.lines() {
        let fields = split_nmcli_fields(line);
        let (Some(name), Some(kind)) = (fields.first(), fields.get(1)) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        if kind.contains("wireless") {
            return Some((name.clone(), LinkKind::Wireless));
        }
        if kind == "802-3-ethernet" && wired.is_none() {
            wired = Some((name.clone(), LinkKind::Wired));
        }
    }
    wired
}

/// Signal strength of the connected network from
/// `nmcli -t -f active,ssid,signal dev wifi`.
#[must_use]
pub fn parse_active_signal(output: &str) -> Option<u8> {
    output.lines().find_map(|line| {
        let fields = split_nmcli_fields(line);
        (fields.first().map(String::as_str) == Some("yes"))
            .then(|| fields.get(2)?.parse::<u8>().ok().map(|s| s.min(100)))
            .flatten()
    })
}

/// `bluetoothctl show` → whether the controller is powered. Returns `None`
/// when the output describes no controller at all.
#[must_use]
pub fn parse_bluetooth_show(output: &str) -> Option<bool> {
    if output.trim().is_empty() || output.contains("No default controller") {
        return None;
    }
    output.lines().find_map(|line| match line.trim() {
        "Powered: yes" => Some(true),
        "Powered: no" => Some(false),
        _ => None,
    })
}

/// `rfkill list <type>` → whether the radio is soft- or hard-blocked.
/// `None` means no device of that type exists.
#[must_use]
pub fn parse_rfkill_blocked(output: &str) -> Option<bool> {
    let mut seen_device = false;
    let mut blocked = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.ends_with("Bluetooth") || trimmed.ends_with("Wireless LAN") {
            seen_device = true;
        } else if trimmed == "Soft blocked: yes" || trimmed == "Hard blocked: yes" {
            blocked = true;
        }
    }
    seen_device.then_some(blocked)
}

/// Icon for the link, so the row reads at a glance.
///
/// A wired link is checked first: an Ethernet cable is still the connection
/// even when the wireless radio is switched off.
#[must_use]
pub fn wifi_icon(state: &NetworkState) -> &'static str {
    match state.kind {
        LinkKind::Wired => "\u{f6ff}",          // fa-network-wired
        _ if !state.wifi_enabled => "\u{f05e}", // fa-ban
        _ => "\u{f1eb}",                        // fa-wifi
    }
}

/// The Wi-Fi control-center row.
#[must_use]
pub fn network_row(state: &NetworkState) -> String {
    let detail = if !state.wifi_enabled && state.kind != LinkKind::Wired {
        "[ off ]".to_string()
    } else {
        match (&state.connection, state.signal) {
            (Some(name), Some(signal)) => format!("{name}  {signal}%"),
            (Some(name), None) => name.clone(),
            (None, _) => "not connected".to_string(),
        }
    };
    format!("{}  Network{:>24}", wifi_icon(state), detail)
}

/// The Bluetooth control-center row.
#[must_use]
pub fn bluetooth_row(state: &BluetoothState) -> String {
    format!(
        "\u{f293}  Bluetooth{:>24}",
        if state.powered { "[ on ]" } else { "[ off ]" }
    )
}

// ---------------------------------------------------------------------------
// Tool detection and queries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WifiTool {
    Nmcli,
    Rfkill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BluetoothTool {
    Bluetoothctl,
    Rfkill,
}

static WIFI_TOOL: OnceLock<Option<WifiTool>> = OnceLock::new();
static BLUETOOTH_TOOL: OnceLock<Option<BluetoothTool>> = OnceLock::new();

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn wifi_tool() -> Option<WifiTool> {
    *WIFI_TOOL.get_or_init(|| {
        if run("nmcli", &["radio", "wifi"])
            .and_then(|o| parse_radio(&o))
            .is_some()
        {
            return Some(WifiTool::Nmcli);
        }
        run("rfkill", &["list", "wifi"])
            .and_then(|o| parse_rfkill_blocked(&o))
            .map(|_| WifiTool::Rfkill)
    })
}

fn bluetooth_tool() -> Option<BluetoothTool> {
    *BLUETOOTH_TOOL.get_or_init(|| {
        if run("bluetoothctl", &["show"])
            .and_then(|o| parse_bluetooth_show(&o))
            .is_some()
        {
            return Some(BluetoothTool::Bluetoothctl);
        }
        run("rfkill", &["list", "bluetooth"])
            .and_then(|o| parse_rfkill_blocked(&o))
            .map(|_| BluetoothTool::Rfkill)
    })
}

/// Read the current network state, or `None` when this machine has no
/// wireless radio to report on.
#[must_use]
pub fn network_state() -> Option<NetworkState> {
    match wifi_tool()? {
        WifiTool::Nmcli => {
            let wifi_enabled = run("nmcli", &["radio", "wifi"]).and_then(|o| parse_radio(&o))?;
            let active = run(
                "nmcli",
                &[
                    "-t",
                    "-f",
                    "NAME,TYPE,DEVICE",
                    "connection",
                    "show",
                    "--active",
                ],
            )
            .and_then(|o| parse_active_connection(&o));
            let (connection, kind) = match active {
                Some((name, kind)) => (Some(name), kind),
                None => (None, LinkKind::None),
            };
            let signal = (kind == LinkKind::Wireless)
                .then(|| run("nmcli", &["-t", "-f", "active,ssid,signal", "dev", "wifi"]))
                .flatten()
                .and_then(|o| parse_active_signal(&o));
            Some(NetworkState {
                wifi_enabled,
                connection,
                kind,
                signal,
            })
        }
        // Without NetworkManager only the radio switch is visible; the row
        // then reports on/off without claiming to know the network.
        WifiTool::Rfkill => {
            let blocked =
                run("rfkill", &["list", "wifi"]).and_then(|o| parse_rfkill_blocked(&o))?;
            Some(NetworkState {
                wifi_enabled: !blocked,
                connection: None,
                kind: LinkKind::None,
                signal: None,
            })
        }
    }
}

#[must_use]
pub fn bluetooth_state() -> BluetoothState {
    match bluetooth_tool() {
        Some(BluetoothTool::Bluetoothctl) => {
            let powered = run("bluetoothctl", &["show"])
                .and_then(|o| parse_bluetooth_show(&o))
                .unwrap_or(false);
            BluetoothState {
                present: true,
                powered,
            }
        }
        Some(BluetoothTool::Rfkill) => {
            let blocked = run("rfkill", &["list", "bluetooth"])
                .and_then(|o| parse_rfkill_blocked(&o))
                .unwrap_or(true);
            BluetoothState {
                present: true,
                powered: !blocked,
            }
        }
        None => BluetoothState::default(),
    }
}

#[must_use]
pub fn read_state() -> ConnectivityState {
    ConnectivityState {
        network: network_state(),
        bluetooth: bluetooth_state(),
    }
}

/// Switch the Wi-Fi radio. Returns false when no tool would do it.
#[must_use]
pub fn set_wifi(enabled: bool) -> bool {
    match wifi_tool() {
        Some(WifiTool::Nmcli) => run(
            "nmcli",
            &["radio", "wifi", if enabled { "on" } else { "off" }],
        )
        .is_some(),
        Some(WifiTool::Rfkill) => run(
            "rfkill",
            &[if enabled { "unblock" } else { "block" }, "wifi"],
        )
        .is_some(),
        None => false,
    }
}

/// Switch the Bluetooth controller. Returns false when no tool would do it.
#[must_use]
pub fn set_bluetooth(enabled: bool) -> bool {
    match bluetooth_tool() {
        Some(BluetoothTool::Bluetoothctl) => run(
            "bluetoothctl",
            &["power", if enabled { "on" } else { "off" }],
        )
        .is_some(),
        Some(BluetoothTool::Rfkill) => run(
            "rfkill",
            &[if enabled { "unblock" } else { "block" }, "bluetooth"],
        )
        .is_some(),
        None => false,
    }
}

impl crate::jwm::Jwm {
    /// Re-read Wi-Fi and Bluetooth and refresh an open control center.
    /// Called after a toggle and on the hardware poll.
    pub(crate) fn refresh_connectivity(&mut self) {
        let state = read_state();
        if state == self.features.connectivity {
            return;
        }
        self.features.connectivity = state;
        self.refresh_open_control_center();
        self.broadcast_ipc_event("network/status", self.connectivity_json());
    }

    /// JSON snapshot for the `get_connectivity` query and the event payload.
    pub(crate) fn connectivity_json(&self) -> serde_json::Value {
        let network = match &self.features.connectivity.network {
            Some(state) => serde_json::json!({
                "present": true,
                "wifi_enabled": state.wifi_enabled,
                "connection": state.connection,
                "kind": state.kind.as_str(),
                "signal": state.signal,
            }),
            None => serde_json::json!({ "present": false }),
        };
        let bluetooth = self.features.connectivity.bluetooth;
        serde_json::json!({
            "network": network,
            "bluetooth": {
                "present": bluetooth.present,
                "powered": bluetooth.powered,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radio_state_parses_both_words() {
        assert_eq!(parse_radio("enabled\n"), Some(true));
        assert_eq!(parse_radio("disabled"), Some(false));
        assert_eq!(parse_radio("missing"), None);
    }

    #[test]
    fn nmcli_records_split_on_unescaped_colons() {
        assert_eq!(
            split_nmcli_fields("ENGINEAI 1:802-11-wireless:wlp3s0"),
            ["ENGINEAI 1", "802-11-wireless", "wlp3s0"]
        );
    }

    #[test]
    fn an_escaped_colon_stays_inside_the_field() {
        // An SSID containing a colon must not split the record.
        assert_eq!(
            split_nmcli_fields(r"net\:work:802-11-wireless:wlp3s0"),
            ["net:work", "802-11-wireless", "wlp3s0"]
        );
        assert_eq!(split_nmcli_fields(r"back\\slash:x"), [r"back\slash", "x"]);
    }

    #[test]
    fn the_wireless_connection_wins_over_wired_and_virtual() {
        // Real `nmcli connection show --active` output from a laptop.
        let output = "ENGINEAI 1:802-11-wireless:wlp3s0\ntun0:tun:tun0\ndocker0:bridge:docker0\n";
        assert_eq!(
            parse_active_connection(output),
            Some(("ENGINEAI 1".to_string(), LinkKind::Wireless))
        );
    }

    #[test]
    fn a_wired_link_is_reported_when_there_is_no_wireless_one() {
        let output = "docker0:bridge:docker0\nWired connection 1:802-3-ethernet:enp0s31f6\n";
        assert_eq!(
            parse_active_connection(output),
            Some(("Wired connection 1".to_string(), LinkKind::Wired))
        );
    }

    #[test]
    fn virtual_plumbing_alone_counts_as_not_connected() {
        let output = "tun0:tun:tun0\ndocker0:bridge:docker0\nlo:loopback:lo\n";
        assert_eq!(parse_active_connection(output), None);
        assert_eq!(parse_active_connection(""), None);
    }

    #[test]
    fn the_signal_comes_from_the_active_network() {
        let output = "no:ENGINEAI-Guest:85\nyes:ENGINEAI:72\nno:IO_2.4G:70\n";
        assert_eq!(parse_active_signal(output), Some(72));
    }

    #[test]
    fn no_active_network_reports_no_signal() {
        assert_eq!(parse_active_signal("no:A:85\nno:B:70\n"), None);
        assert_eq!(parse_active_signal(""), None);
    }

    #[test]
    fn bluetooth_power_is_read_from_the_controller_block() {
        // Real `bluetoothctl show` output.
        let output = "Controller F0:68:E3:7B:9E:05 (public)\n\tName: host\n\tPowered: yes\n\tDiscoverable: no\n";
        assert_eq!(parse_bluetooth_show(output), Some(true));

        let off = "Controller F0:68:E3:7B:9E:05 (public)\n\tPowered: no\n";
        assert_eq!(parse_bluetooth_show(off), Some(false));
    }

    #[test]
    fn no_controller_means_no_bluetooth_row() {
        assert_eq!(parse_bluetooth_show(""), None);
        assert_eq!(
            parse_bluetooth_show("No default controller available\n"),
            None
        );
    }

    #[test]
    fn rfkill_reports_blocked_state_per_device() {
        let unblocked = "1: phy0: Wireless LAN\n\tSoft blocked: no\n\tHard blocked: no\n";
        assert_eq!(parse_rfkill_blocked(unblocked), Some(false));

        let soft = "1: phy0: Wireless LAN\n\tSoft blocked: yes\n\tHard blocked: no\n";
        assert_eq!(parse_rfkill_blocked(soft), Some(true));

        let hard = "0: hci0: Bluetooth\n\tSoft blocked: no\n\tHard blocked: yes\n";
        assert_eq!(parse_rfkill_blocked(hard), Some(true));

        // No device of that type at all.
        assert_eq!(parse_rfkill_blocked(""), None);
    }

    #[test]
    fn the_network_row_shows_the_connection_and_signal() {
        let state = NetworkState {
            wifi_enabled: true,
            connection: Some("ENGINEAI".to_string()),
            kind: LinkKind::Wireless,
            signal: Some(72),
        };
        let row = network_row(&state);
        assert!(row.contains("ENGINEAI"));
        assert!(row.contains("72%"));
        assert!(row.contains('\u{f1eb}'));
    }

    #[test]
    fn a_disabled_radio_reads_as_off() {
        let state = NetworkState::default();
        let row = network_row(&state);
        assert!(row.contains("[ off ]"));
        assert!(row.contains('\u{f05e}'));
    }

    #[test]
    fn an_enabled_but_unconnected_radio_says_so() {
        let state = NetworkState {
            wifi_enabled: true,
            ..Default::default()
        };
        assert!(network_row(&state).contains("not connected"));
    }

    #[test]
    fn a_wired_link_shows_even_with_the_radio_off() {
        let state = NetworkState {
            wifi_enabled: false,
            connection: Some("Wired connection 1".to_string()),
            kind: LinkKind::Wired,
            signal: None,
        };
        let row = network_row(&state);
        assert!(row.contains("Wired connection 1"));
        assert!(row.contains('\u{f6ff}'));
    }

    #[test]
    fn the_bluetooth_row_reflects_power() {
        assert!(
            bluetooth_row(&BluetoothState {
                present: true,
                powered: true
            })
            .contains("[ on ]")
        );
        assert!(bluetooth_row(&BluetoothState::default()).contains("[ off ]"));
    }
}
