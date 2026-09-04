//! Wi-Fi and Bluetooth state for the control center.
//!
//! Same shape as [`system_controls`](super::system_controls) and
//! [`power`](super::power): shell out to whatever the session actually runs —
//! `nmcli` then `rfkill` for the wireless radio, `bluetoothctl` then `rfkill`
//! for Bluetooth — cache the tool that answered, and keep every parser pure so
//! the output formats are pinned by tests on machines that have neither.

use std::sync::OnceLock;
use std::time::Duration;

const CONNECTIVITY_QUERY_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECTIVITY_ACTION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONNECTIVITY_OUTPUT_BYTES: usize = 1024 * 1024;

fn connectivity_output(
    cmd: &str,
    args: &[&str],
    timeout: Duration,
    output_limit: usize,
) -> std::io::Result<std::process::Output> {
    super::external_command::output_with_limits(cmd, args, timeout, output_limit)
}

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
        // fa-sitemap: fa-network-wired is an f6xx codepoint many fonts lack.
        LinkKind::Wired => "\u{f0e8}",
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
    let output = connectivity_output(
        cmd,
        args,
        CONNECTIVITY_QUERY_TIMEOUT,
        MAX_CONNECTIVITY_OUTPUT_BYTES,
    )
    .ok()?;
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

// ---------------------------------------------------------------------------
// Scanning and joining
// ---------------------------------------------------------------------------

/// One access point from a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiNetwork {
    pub ssid: String,
    pub signal: u8,
    /// Empty for an open network; otherwise the flags nmcli reports, e.g.
    /// `WPA2` or `WPA1 WPA2`.
    pub security: String,
    /// Whether this is the network currently in use.
    pub in_use: bool,
}

const MAX_WIFI_SCAN_LINES: usize = 1024;
const MAX_WIFI_SCAN_LINE_BYTES: usize = 1024;
const MAX_WIFI_NETWORKS: usize = 256;
// IEEE 802.11 SSIDs contain at most 32 octets.
const MAX_WIFI_SSID_BYTES: usize = 32;
const MAX_WIFI_SECURITY_BYTES: usize = 128;

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

impl WifiNetwork {
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.security.trim().is_empty()
    }
}

/// Work handed to a worker thread, because doing it inline would freeze the
/// compositor: nmcli's first `dev wifi list` after boot triggers a scan and
/// takes seconds, and joining a network takes seconds more.
///
/// The result is picked up by polling from the frame tick — jwm's event loop
/// owns all state, so nothing but the finished value crosses the boundary.
#[derive(Debug)]
pub struct BackgroundJob<T> {
    slot: std::sync::Arc<std::sync::Mutex<BackgroundJobState<T>>>,
}

#[derive(Debug)]
struct BackgroundJobState<T> {
    result: Option<T>,
    notifier: Option<crate::backend::update_notifier::AsyncUpdateNotifier>,
}

impl<T: Send + 'static> BackgroundJob<T> {
    pub fn spawn(work: impl FnOnce() -> T + Send + 'static) -> Self {
        let slot = std::sync::Arc::new(std::sync::Mutex::new(BackgroundJobState {
            result: None,
            notifier: None,
        }));
        let handle = std::sync::Arc::clone(&slot);
        std::thread::spawn(move || {
            let value = work();
            let notifier = {
                let mut guard = handle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Publish the result while holding the mutex, then clone the
                // notifier and release the mutex before signalling. A handler
                // woken by the eventfd can therefore take the value at once.
                guard.result = Some(value);
                guard.notifier.clone()
            };
            if let Some(notifier) = notifier {
                notifier.notify();
            }
        });
        Self { slot }
    }

    /// Attach this job to one JWM event loop.
    ///
    /// Attachment is race-safe when a very short job finishes first: seeing
    /// an already-published result emits the wake here instead of waiting for
    /// the old 20 ms polling cadence.
    #[must_use]
    pub(crate) fn with_notifier(
        self,
        notifier: Option<crate::backend::update_notifier::AsyncUpdateNotifier>,
    ) -> Self {
        let notify_now = {
            let mut guard = self
                .slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.notifier = notifier;
            guard
                .result
                .is_some()
                .then(|| guard.notifier.clone())
                .flatten()
        };
        if let Some(notifier) = notify_now {
            notifier.notify();
        }
        self
    }

    /// The finished value, once. Returns `None` while the work is still
    /// running, and never blocks the caller: a contended lock simply reads as
    /// "not ready yet" and is retried on the next frame.
    #[must_use]
    pub fn take(&self) -> Option<T> {
        self.slot.try_lock().ok()?.result.take()
    }

    /// Whether dropping the idle poll is safe while this job is in flight.
    /// Lock contention is treated conservatively as uncovered for one
    /// scheduling decision.
    #[must_use]
    pub(crate) fn readiness_is_covered(&self) -> bool {
        self.slot.try_lock().ok().is_some_and(|guard| {
            guard
                .notifier
                .as_ref()
                .is_some_and(crate::backend::update_notifier::AsyncUpdateNotifier::is_healthy)
        })
    }
}

impl<T> Drop for BackgroundJob<T> {
    fn drop(&mut self) {
        // A closed picker deliberately detaches its in-flight worker. Avoid a
        // needless event-loop wake when that discarded result later arrives.
        if let Ok(mut guard) = self.slot.try_lock() {
            guard.notifier = None;
        }
    }
}

/// Parse `nmcli -t -f IN-USE,SSID,SIGNAL,SECURITY dev wifi list`.
///
/// The same SSID appears once per access point and band; the strongest
/// reading wins so the list is one row per network. Hidden networks report an
/// empty SSID and are dropped — there is nothing to show or select.
#[must_use]
pub fn parse_networks(output: &str) -> Vec<WifiNetwork> {
    let mut networks: Vec<WifiNetwork> = Vec::new();
    for line in output.lines().take(MAX_WIFI_SCAN_LINES) {
        if line.len() > MAX_WIFI_SCAN_LINE_BYTES {
            continue;
        }
        let fields = split_nmcli_fields(line);
        let (Some(in_use), Some(ssid), Some(signal)) =
            (fields.first(), fields.get(1), fields.get(2))
        else {
            continue;
        };
        let ssid = ssid.trim();
        if ssid.is_empty() || ssid.len() > MAX_WIFI_SSID_BYTES {
            continue;
        }
        let signal = signal.trim().parse::<u8>().unwrap_or(0).min(100);
        let security = fields.get(3).map_or("", String::as_str).trim();
        let network = WifiNetwork {
            ssid: ssid.to_string(),
            signal,
            security: utf8_prefix(security, MAX_WIFI_SECURITY_BYTES).to_string(),
            in_use: in_use.trim() == "*",
        };
        if let Some(existing) = networks.iter_mut().find(|other| other.ssid == network.ssid) {
            // Keep the strongest reading, but never lose the fact that one of
            // this network's access points is the one in use.
            existing.in_use |= network.in_use;
            if network.signal > existing.signal {
                existing.signal = network.signal;
                existing.security = network.security;
            }
            continue;
        }
        if networks.len() < MAX_WIFI_NETWORKS {
            networks.push(network);
        }
    }
    networks.sort_by(|a, b| b.signal.cmp(&a.signal).then_with(|| a.ssid.cmp(&b.ssid)));
    networks
}

/// Signal glyph.
///
/// Two tiers, not four: the per-level `fa-signal-N` glyphs are
/// FontAwesome-5-era `f6xx` codepoints that common Nerd Font builds do not
/// carry, and a missing glyph renders as a hollow box. Everything the shell
/// draws sticks to the FontAwesome-4 range for that reason; the exact
/// strength is on the row as a percentage anyway.
#[must_use]
pub fn signal_icon(signal: u8) -> &'static str {
    if signal >= 50 {
        "\u{f1eb}" // fa-wifi
    } else {
        "\u{f012}" // fa-signal
    }
}

/// One picker row: signal, SSID, a lock for secured networks, and a marker
/// for the network already in use.
#[must_use]
pub fn picker_row(network: &WifiNetwork) -> String {
    let lock = if network.is_open() {
        " "
    } else {
        "\u{f023}" // fa-lock
    };
    let marker = if network.in_use { "\u{f00c}" } else { " " };
    format!(
        "{} {marker} {:<32} {lock}  {:>3}%",
        signal_icon(network.signal),
        network.ssid,
        network.signal
    )
}

/// What joining a network requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectPlan {
    /// `NetworkManager` already has a profile: bring it up, no passphrase.
    UseSaved,
    /// Open network, nothing to ask for.
    Open,
    /// Secured and unknown: the picker must prompt before it can proceed.
    NeedsPassphrase,
    /// Secured, with the passphrase the user just typed.
    WithPassphrase,
}

/// Decide how to join `network`. Pure so the branch that decides whether to
/// prompt is testable without `NetworkManager`.
#[must_use]
pub fn plan_connect(network: &WifiNetwork, saved: bool, passphrase: Option<&str>) -> ConnectPlan {
    if let Some(passphrase) = passphrase
        && !passphrase.is_empty()
    {
        return ConnectPlan::WithPassphrase;
    }
    if saved {
        return ConnectPlan::UseSaved;
    }
    if network.is_open() {
        return ConnectPlan::Open;
    }
    ConnectPlan::NeedsPassphrase
}

// ---------------------------------------------------------------------------
// Bluetooth devices
// ---------------------------------------------------------------------------

/// One known Bluetooth device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BluetoothDevice {
    /// Controller address, `AA:BB:CC:DD:EE:FF`, used for every command.
    pub address: String,
    pub name: String,
    pub connected: bool,
    pub paired: bool,
    /// Signal strength in dBm, when the controller is hearing the device
    /// right now. `None` for a remembered device out of range, and for every
    /// device on the `bluetoothctl` path, which never reported it.
    ///
    /// A discovery scan in a populated room returns dozens of nameless
    /// beacons; sorting those by name is sorting by MAC address. Proximity is
    /// the only ordering that helps someone find the headset in their hand.
    pub rssi: Option<i16>,
}

/// A remembered-device list should be tiny. These bounds keep malformed or
/// unexpected bluetoothctl output from turning one picker refresh into an
/// arbitrary number of `bluetoothctl info` child processes.
const MAX_BLUETOOTH_DEVICES: usize = 64;
const MAX_BLUETOOTH_DEVICE_LINES: usize = 1024;
const MAX_BLUETOOTH_DEVICE_NAME_CHARS: usize = 248;

/// Bound and sanitize a device name for the picker.
///
/// A name is chosen by the device, i.e. by a stranger in radio range. The
/// picker draws its rows by joining them with newlines and splitting them
/// back into lines, while the selection pill and the pointer hit-map are
/// computed from the *item* count — so one embedded newline draws an extra
/// line, shifts every row below it off its own highlight, and makes `Enter`
/// act on a device other than the one that looks selected. Control
/// characters are dropped rather than escaped: there is nothing a device
/// needs them for. `parse_prompt_command` has applied the same rule to the
/// name on the pairing wire from the start.
fn sanitize_device_name(name: &str) -> String {
    name.trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_BLUETOOTH_DEVICE_NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

pub(crate) fn is_bluetooth_address(address: &str) -> bool {
    let mut count = 0;
    for octet in address.split(':') {
        if octet.len() != 2 || !octet.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return false;
        }
        count += 1;
    }
    count == 6
}

/// Parse `bluetoothctl devices`: one `Device <address> <name>` line each.
///
/// Names may contain spaces, so only the first two fields are split off. A
/// device whose name is unknown reports its address again, which is left as
/// is — it is still the only handle the user has.
#[must_use]
pub fn parse_devices(output: &str) -> Vec<BluetoothDevice> {
    let mut devices: Vec<BluetoothDevice> = Vec::new();
    for line in output.lines().take(MAX_BLUETOOTH_DEVICE_LINES) {
        if devices.len() >= MAX_BLUETOOTH_DEVICES {
            break;
        }
        let mut parts = line.trim().splitn(3, ' ');
        if parts.next() != Some("Device") {
            continue;
        }
        let Some(address) = parts.next().filter(|address| is_bluetooth_address(address)) else {
            continue;
        };
        if devices
            .iter()
            .any(|device| device.address.eq_ignore_ascii_case(address))
        {
            continue;
        }
        let name = sanitize_device_name(parts.next().unwrap_or(address));
        devices.push(BluetoothDevice {
            address: address.to_string(),
            name: if name.is_empty() {
                address.to_string()
            } else {
                name
            },
            connected: false,
            paired: false,
            // The text path never reported it; only the D-Bus helper does.
            rssi: None,
        });
    }
    devices
}

/// Parse `bluetoothctl --timeout N scan on` output into newly heard devices.
///
/// Discovery prints `[NEW] Device <address> <name>` on first sighting and
/// `[CHG] Device <address> <property>: <value>` as advertisements update. Only
/// `Name:` changes carry something worth listing; RSSI/TxPower churn is
/// ignored. A device heard about only through property lines still gets a row
/// under its address.
#[must_use]
pub fn parse_scan_output(output: &str) -> Vec<BluetoothDevice> {
    let mut devices: Vec<BluetoothDevice> = Vec::new();
    for line in output.lines().take(MAX_BLUETOOTH_DEVICE_LINES) {
        let line = line.trim();
        let (changed, rest) = if let Some(rest) = line.strip_prefix("[NEW] Device ") {
            (false, rest)
        } else if let Some(rest) = line.strip_prefix("[CHG] Device ") {
            (true, rest)
        } else {
            continue;
        };
        let mut parts = rest.splitn(2, ' ');
        let Some(address) = parts.next().filter(|address| is_bluetooth_address(address)) else {
            continue;
        };
        let remainder = parts.next().unwrap_or("").trim();
        // [CHG] lines are property updates; a plain remainder only names a
        // device on the [NEW] line. `Alias:` mirrors `Name:` and is skipped.
        let name = if changed {
            match remainder.strip_prefix("Name:") {
                Some(name) => name.trim(),
                None => continue,
            }
        } else {
            remainder
        };
        if let Some(existing) = devices
            .iter_mut()
            .find(|device| device.address.eq_ignore_ascii_case(address))
        {
            // A later Name: change can fill in a device first heard unnamed.
            let name = sanitize_device_name(name);
            if !name.is_empty() && existing.name == existing.address {
                existing.name = name;
            }
            continue;
        }
        if devices.len() >= MAX_BLUETOOTH_DEVICES {
            break;
        }
        let name = sanitize_device_name(name);
        devices.push(BluetoothDevice {
            address: address.to_string(),
            name: if name.is_empty() {
                address.to_string()
            } else {
                name
            },
            connected: false,
            paired: false,
            // The text path never reported it; only the D-Bus helper does.
            rssi: None,
        });
    }
    devices
}

/// Read `Connected:`/`Paired:` out of `bluetoothctl info <address>`.
#[must_use]
pub fn parse_device_info(output: &str) -> (bool, bool) {
    let mut connected = false;
    let mut paired = false;
    for line in output.lines() {
        match line.trim() {
            "Connected: yes" => connected = true,
            "Paired: yes" | "Bonded: yes" => paired = true,
            _ => {}
        }
    }
    (connected, paired)
}

/// Parse `jwm-bridge discover`: a JSON array of
/// `{address, name, paired, connected, rssi}`.
///
/// One `GetManagedObjects` round trip answers what the text path needed one
/// `bluetoothctl info` child per device to learn, so a list parsed here needs
/// no follow-up sweep. The payload comes from a helper reading remote-peer
/// advertisements, so it is bounded exactly like the text path: the same
/// device cap, the same name cap, addresses validated before they can become
/// a command argument.
#[must_use]
pub fn parse_bridge_devices(output: &str) -> Vec<BluetoothDevice> {
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str(output.trim()) else {
        return Vec::new();
    };
    let mut devices: Vec<BluetoothDevice> = Vec::new();
    for entry in entries.iter().take(MAX_BLUETOOTH_DEVICE_LINES) {
        let Some(address) = entry.get("address").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let address = address.to_uppercase();
        if !is_bluetooth_address(&address) {
            continue;
        }
        if devices
            .iter()
            .any(|device| device.address.eq_ignore_ascii_case(&address))
        {
            continue;
        }
        let name = entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(sanitize_device_name)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| address.clone());
        devices.push(BluetoothDevice {
            address,
            name,
            connected: entry
                .get("connected")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            paired: entry
                .get("paired")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            rssi: entry
                .get("rssi")
                .and_then(serde_json::Value::as_i64)
                .and_then(|rssi| i16::try_from(rssi).ok()),
        });
    }
    // Sort *before* the cap, so what survives it is what the user most likely
    // wants. Truncating in wire order — which is object-path, i.e. MAC order
    // — would drop a connected headset at `F0:…` behind sixty-four nameless
    // beacons in a busy room, which is exactly the room the cap exists for.
    // The `bluetoothctl` path could not do that: it listed remembered
    // devices first and merged sightings into whatever room was left.
    sort_devices(&mut devices);
    devices.truncate(MAX_BLUETOOTH_DEVICES);
    devices
}

/// Sort devices the way the picker lists them: connected first, then paired,
/// then — among devices in the same state — the strongest signal, then by
/// name.
///
/// Signal comes before name because a discovery scan in a populated room
/// returns dozens of nameless beacons whose "name" is their MAC address;
/// alphabetical order over those is noise, while the device in the user's
/// hand is reliably the loudest. Devices with no reading (everything on the
/// `bluetoothctl` path, and remembered devices out of range) sort after the
/// ones that were heard, and fall back to name among themselves — which is
/// exactly the old ordering.
pub fn sort_devices(devices: &mut [BluetoothDevice]) {
    devices.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then_with(|| b.paired.cmp(&a.paired))
            .then_with(|| b.rssi.is_some().cmp(&a.rssi.is_some()))
            .then_with(|| b.rssi.cmp(&a.rssi))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// One Bluetooth picker row.
#[must_use]
pub fn device_row(device: &BluetoothDevice) -> String {
    let marker = if device.connected {
        "\u{f00c}" // fa-check
    } else {
        " "
    };
    // The trailing column says what the row is: its bond state, or — for a
    // device that has neither, which is every row a discovery scan adds —
    // how strongly the controller is hearing it. dBm is a negative number
    // where closer to zero is nearer; it is printed raw because there is no
    // honest way to turn one advertisement's RSSI into a percentage.
    let state = if device.connected {
        "connected".to_string()
    } else if device.paired {
        "paired".to_string()
    } else {
        device
            .rssi
            .map(|rssi| format!("{rssi} dBm"))
            .unwrap_or_default()
    };
    format!(
        "\u{f293} {marker} {:<34} {state}",
        device.name.chars().take(34).collect::<String>()
    )
}

/// Whether activating a device pairs, connects, or disconnects it. A device
/// the controller has never bonded must be paired before connecting is even
/// meaningful, and pairing goes through the `jwm-bridge pair` helper because
/// bluez wants an interactive agent.
#[must_use]
pub fn device_action(device: &BluetoothDevice) -> &'static str {
    if device.connected {
        "disconnect"
    } else if device.paired {
        "connect"
    } else {
        "pair"
    }
}

/// Read the live `Connected:`/`Paired:` flags into every listed device.
/// `bluetoothctl info` is one process per device, which is why callers run on
/// a worker thread: a handful of devices would otherwise stall a frame.
fn read_device_states(devices: &mut [BluetoothDevice]) {
    for device in devices {
        if let Some(info) = run("bluetoothctl", &["info", &device.address]) {
            let (connected, paired) = parse_device_info(&info);
            device.connected = connected;
            device.paired = paired;
        }
    }
}

/// Fold freshly discovered devices into the remembered list, keeping the
/// remembered entry's live flags. Bounded like the remembered list itself.
fn merge_discovered(devices: &mut Vec<BluetoothDevice>, discovered: Vec<BluetoothDevice>) {
    for device in discovered {
        if devices.len() >= MAX_BLUETOOTH_DEVICES {
            break;
        }
        if devices
            .iter()
            .any(|known| known.address.eq_ignore_ascii_case(&device.address))
        {
            continue;
        }
        devices.push(device);
    }
}

/// How long `jwm-bridge discover` scans for a picker refresh. The helper
/// clamps anything larger; this is the number jwm asks for.
const BRIDGE_DISCOVERY_SECONDS: u64 = 10;
/// Deadline for the helper process, leaving room above its own scan window
/// for process startup and the two bus round trips around it.
const BRIDGE_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(25);

/// Whether `jwm-bridge` is installed at all.
///
/// A `PATH` walk, not an exec. `start_device_scan` and `start_discovery_scan`
/// have to answer this *before* they spawn their worker — that is what
/// decides whether there is a job to run — so it happens on the frame
/// thread. Running the helper there would block the compositor for up to the
/// query timeout on any session that has jwm-bridge but not `bluetoothctl`,
/// which is precisely the stall the "jwm stays D-Bus-free so a wedged bus
/// cannot hold a frame" rule exists to prevent. Whether the helper can
/// actually answer is decided inside the worker, where it costs nothing.
fn bridge_discovery_installed() -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| {
        let Some(path) = std::env::var_os("PATH") else {
            return false;
        };
        std::env::split_paths(&path).any(|dir| dir.join("jwm-bridge").is_file())
    })
}

/// Ask `jwm-bridge` for the device list, scanning for `seconds` first.
/// `None` when the helper is absent or failed, so the caller falls back.
fn bridge_devices(seconds: u64) -> Option<Vec<BluetoothDevice>> {
    if !bridge_discovery_installed() {
        return None;
    }
    let seconds = seconds.to_string();
    let output = connectivity_output(
        "jwm-bridge",
        &["discover", &seconds],
        BRIDGE_DISCOVERY_TIMEOUT,
        MAX_CONNECTIVITY_OUTPUT_BYTES,
    )
    .ok()
    .filter(|output| output.status.success())?;
    Some(parse_bridge_devices(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// List known devices with their live state, on a worker thread.
///
/// The helper answers the whole list — names and live Paired/Connected — in
/// one bus round trip. The `bluetoothctl` fallback needs `devices` plus one
/// `info` child per device, which is why this has always run off the frame
/// thread.
#[must_use]
pub fn start_device_scan() -> Option<BackgroundJob<Vec<BluetoothDevice>>> {
    if bluetooth_tool() != Some(BluetoothTool::Bluetoothctl) && !bridge_discovery_installed() {
        return None;
    }
    Some(BackgroundJob::spawn(|| {
        // Zero seconds: list what bluez remembers without powering up a scan.
        if let Some(mut devices) = bridge_devices(0) {
            sort_devices(&mut devices);
            return devices;
        }
        let mut devices = run("bluetoothctl", &["devices"])
            .map(|output| parse_devices(&output))
            .unwrap_or_default();
        read_device_states(&mut devices);
        sort_devices(&mut devices);
        devices
    }))
}

/// Run a bounded discovery and merge what it hears into the remembered list,
/// on a worker thread.
///
/// `jwm-bridge discover` drives `Adapter1.StartDiscovery` and reads the whole
/// object tree back — one list, with live Paired/Connected and the signal
/// strength the text path parsed and threw away, and no per-device `info`
/// children. Without the helper this falls back to
/// `bluetoothctl --timeout 15 scan on`, whose sightings are merged into the
/// remembered list and then swept for state.
///
/// Either way the scan window is fixed and the worker deadline leaves
/// headroom for process startup and the follow-up work.
#[must_use]
pub fn start_discovery_scan() -> Option<BackgroundJob<Vec<BluetoothDevice>>> {
    if bluetooth_tool() != Some(BluetoothTool::Bluetoothctl) && !bridge_discovery_installed() {
        return None;
    }
    Some(BackgroundJob::spawn(|| {
        if let Some(mut devices) = bridge_devices(BRIDGE_DISCOVERY_SECONDS) {
            sort_devices(&mut devices);
            return devices;
        }
        let discovered = connectivity_output(
            "bluetoothctl",
            &["--timeout", "15", "scan", "on"],
            Duration::from_secs(25),
            MAX_CONNECTIVITY_OUTPUT_BYTES,
        )
        .map(|output| parse_scan_output(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default();
        let mut devices = run("bluetoothctl", &["devices"])
            .map(|output| parse_devices(&output))
            .unwrap_or_default();
        merge_discovered(&mut devices, discovered);
        read_device_states(&mut devices);
        sort_devices(&mut devices);
        devices
    }))
}

/// Connect or disconnect one device on a worker thread.
#[must_use]
pub fn start_device_action(
    address: &str,
    action: &'static str,
) -> BackgroundJob<Result<String, String>> {
    let address = address.to_string();
    BackgroundJob::spawn(move || {
        match connectivity_output(
            "bluetoothctl",
            &[action, &address],
            CONNECTIVITY_ACTION_TIMEOUT,
            MAX_CONNECTIVITY_OUTPUT_BYTES,
        ) {
            // bluetoothctl exits 0 even when the attempt failed, so the
            // outcome has to be read out of what it printed.
            Ok(output) => {
                let text = String::from_utf8_lossy(&output.stdout);
                if text.contains("Failed to") || text.contains("not available") {
                    Err(summarize_bluetoothctl_error(&text))
                } else {
                    Ok(address)
                }
            }
            Err(error) => Err(format!("could not run bluetoothctl: {error}")),
        }
    })
}

/// Condense bluetoothctl's chatter into the one line that matters.
#[must_use]
pub fn summarize_bluetoothctl_error(output: &str) -> String {
    let line = output
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("Failed to") || line.contains("not available"))
        .unwrap_or("the device did not respond");
    line.chars().take(72).collect()
}

/// What activating the control center's Network row should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkRowAction {
    /// Show the list of networks in range.
    OpenPicker,
    /// Switch the radio on; there is nothing to pick while it is off.
    EnableRadio,
    /// Switch the radio the other way (explicit Left/Right).
    SetRadio(bool),
    Nothing,
}

/// Decide what the Network row does, given the radio state and which key was
/// pressed.
///
/// `Enter` must never switch a working radio *off*: the row is the way into
/// the picker, and an accidental keypress that drops the user's network — and
/// with it anything running over it — is not an acceptable cost for a
/// shortcut. Turning the radio off is only ever explicit, via Left/Right or
/// the `toggle_wifi` action.
#[must_use]
pub fn plan_network_row(radio_on: bool, activate: bool, adjust: bool) -> NetworkRowAction {
    if adjust {
        return NetworkRowAction::SetRadio(!radio_on);
    }
    if !activate {
        return NetworkRowAction::Nothing;
    }
    if radio_on {
        NetworkRowAction::OpenPicker
    } else {
        NetworkRowAction::EnableRadio
    }
}

/// What activating the control center's Bluetooth row should do. Mirrors
/// [`NetworkRowAction`] so the two connectivity rows behave the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BluetoothRowAction {
    /// Show the remembered devices.
    OpenPicker,
    /// Power the controller on; there is nothing to list while it is off.
    PowerOn,
    /// Power it the other way (explicit Left/Right).
    SetPower(bool),
    Nothing,
}

/// Decide what the Bluetooth row does. As with Wi-Fi, `Enter` never switches a
/// working controller *off* — that can take a Bluetooth keyboard with it —
/// so powering down is explicit via Left/Right, and confirmed separately.
#[must_use]
pub fn plan_bluetooth_row(powered: bool, activate: bool, adjust: bool) -> BluetoothRowAction {
    if adjust {
        return BluetoothRowAction::SetPower(!powered);
    }
    if !activate {
        return BluetoothRowAction::Nothing;
    }
    if powered {
        BluetoothRowAction::OpenPicker
    } else {
        BluetoothRowAction::PowerOn
    }
}

/// Whether `NetworkManager` already stores a profile named `ssid`.
#[must_use]
pub fn has_saved_profile(ssid: &str) -> bool {
    let Some(output) = run("nmcli", &["-t", "-f", "NAME", "connection", "show"]) else {
        return false;
    };
    output.lines().any(|line| {
        split_nmcli_fields(line)
            .first()
            .is_some_and(|name| name == ssid)
    })
}

/// Start a scan on a worker thread. `None` when there is no nmcli to scan
/// with — the picker then reports that instead of showing an empty list.
#[must_use]
pub fn start_scan() -> Option<BackgroundJob<Vec<WifiNetwork>>> {
    if wifi_tool() != Some(WifiTool::Nmcli) {
        return None;
    }
    Some(BackgroundJob::spawn(|| {
        run(
            "nmcli",
            &[
                "-t",
                "-f",
                "IN-USE,SSID,SIGNAL,SECURITY",
                "dev",
                "wifi",
                "list",
            ],
        )
        .map(|output| parse_networks(&output))
        .unwrap_or_default()
    }))
}

/// Join a network on a worker thread, reporting what happened.
///
/// The passphrase is moved into the thread and dropped there; it is never
/// stored in the panel once this is called.
#[must_use]
pub fn start_connect(
    ssid: &str,
    plan: &ConnectPlan,
    passphrase: Option<String>,
) -> BackgroundJob<Result<String, String>> {
    let ssid = ssid.to_string();
    let plan = plan.clone();
    BackgroundJob::spawn(move || {
        let output = match plan {
            ConnectPlan::UseSaved => connectivity_output(
                "nmcli",
                &["connection", "up", "id", &ssid],
                CONNECTIVITY_ACTION_TIMEOUT,
                MAX_CONNECTIVITY_OUTPUT_BYTES,
            ),
            ConnectPlan::Open | ConnectPlan::NeedsPassphrase => connectivity_output(
                "nmcli",
                &["device", "wifi", "connect", &ssid],
                CONNECTIVITY_ACTION_TIMEOUT,
                MAX_CONNECTIVITY_OUTPUT_BYTES,
            ),
            ConnectPlan::WithPassphrase => connectivity_output(
                "nmcli",
                &[
                    "device",
                    "wifi",
                    "connect",
                    &ssid,
                    "password",
                    passphrase.as_deref().unwrap_or(""),
                ],
                CONNECTIVITY_ACTION_TIMEOUT,
                MAX_CONNECTIVITY_OUTPUT_BYTES,
            ),
        };
        match output {
            Ok(output) if output.status.success() => Ok(ssid),
            Ok(output) => Err(summarize_nmcli_error(&String::from_utf8_lossy(
                &output.stderr,
            ))),
            Err(error) => Err(format!("could not run nmcli: {error}")),
        }
    })
}

/// Condense nmcli's stderr into one line the panel can show.
#[must_use]
pub fn summarize_nmcli_error(stderr: &str) -> String {
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("connection failed");
    let line = line.strip_prefix("Error:").map_or(line, str::trim);
    line.chars().take(72).collect()
}

/// Read Wi-Fi and Bluetooth state on a worker thread.
///
/// `read_state` shells out to nmcli/bluetoothctl, and nmcli can block for
/// seconds while NetworkManager is mid-scan. Run on the event loop that wait
/// froze the entire desktop (compositing and input both stop; only the
/// hardware cursor keeps moving), so the read must never happen inline.
#[must_use]
pub fn start_state_read() -> BackgroundJob<ConnectivityState> {
    BackgroundJob::spawn(read_state)
}

impl crate::jwm::Jwm {
    /// Bind a worker's completion to this handler's readiness fd. When the fd
    /// could not be created or aggregated, `None` leaves the job on the
    /// conservative timer fallback.
    pub(crate) fn track_background_job<T: Send + 'static>(
        &self,
        job: BackgroundJob<T>,
    ) -> BackgroundJob<T> {
        job.with_notifier(self.async_update_notifier.clone())
    }

    /// Start a read only when none is already in flight. Passive UI openings
    /// coalesce here so rapid close/reopen cycles do not detach a trail of
    /// still-running nmcli workers.
    pub(crate) fn ensure_connectivity_refresh(&mut self) {
        if self.features.connectivity_poll.is_none() {
            self.features.connectivity_poll = Some(self.track_background_job(start_state_read()));
        }
    }

    /// Kick off a background re-read of Wi-Fi and Bluetooth. The result is
    /// adopted from the frame tick by [`Self::poll_connectivity_job`].
    /// Called after a toggle and on the hardware poll.
    ///
    /// A toggle wants the post-toggle state, so a still-running read is
    /// replaced; its thread finishes on its own and the stale result is
    /// dropped with the handle.
    pub(crate) fn refresh_connectivity(&mut self) {
        self.features.connectivity_poll = Some(self.track_background_job(start_state_read()));
    }

    /// Adopt a finished background connectivity read and refresh an open
    /// control center. Called from the frame tick; does nothing while the
    /// read is still running.
    pub(crate) fn poll_connectivity_job(&mut self) {
        let Some(state) = self
            .features
            .connectivity_poll
            .as_ref()
            .and_then(BackgroundJob::take)
        else {
            return;
        };
        self.features.connectivity_poll = None;
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
        assert!(row.contains('\u{f0e8}'));
    }

    fn network(ssid: &str, signal: u8, security: &str) -> WifiNetwork {
        WifiNetwork {
            ssid: ssid.to_string(),
            signal,
            security: security.to_string(),
            in_use: false,
        }
    }

    #[test]
    fn a_scan_collapses_access_points_to_one_row_per_network() {
        // Real output: the same SSID appears once per AP and band.
        let output = " :ENGINEAI:80:WPA2\n :ENGINEAI-Guest:79:WPA2\n*:ENGINEAI:72:WPA2\n :ENGINEAI:64:WPA2\n";
        let networks = parse_networks(output);

        assert_eq!(networks.len(), 2);
        assert_eq!(networks[0].ssid, "ENGINEAI");
        assert_eq!(networks[0].signal, 80, "strongest reading wins");
        assert!(
            networks[0].in_use,
            "a weaker access point being in use must not be lost"
        );
        assert_eq!(networks[1].ssid, "ENGINEAI-Guest");
    }

    #[test]
    fn scans_sort_by_signal_then_name() {
        let output = " :Bravo:40:WPA2\n :Alpha:90:WPA2\n :Charlie:40:WPA2\n";
        let names: Vec<String> = parse_networks(output)
            .into_iter()
            .map(|network| network.ssid)
            .collect();
        assert_eq!(names, ["Alpha", "Bravo", "Charlie"]);
    }

    #[test]
    fn hidden_networks_are_dropped() {
        // A hidden AP reports an empty SSID; there is nothing to select.
        assert!(parse_networks(" ::70:WPA2\n").is_empty());
        assert!(parse_networks("").is_empty());
    }

    #[test]
    fn open_networks_are_recognized() {
        let output = " :FreeWifi:55:\n :Secured:60:WPA2\n";
        let networks = parse_networks(output);
        assert!(
            networks
                .iter()
                .find(|n| n.ssid == "FreeWifi")
                .unwrap()
                .is_open()
        );
        assert!(
            !networks
                .iter()
                .find(|n| n.ssid == "Secured")
                .unwrap()
                .is_open()
        );
    }

    #[test]
    fn multi_word_security_flags_survive() {
        let networks = parse_networks(" :Mixed:70:WPA1 WPA2\n");
        assert_eq!(networks[0].security, "WPA1 WPA2");
        assert!(!networks[0].is_open());
    }

    #[test]
    fn scans_bound_unique_networks_and_input_lines() {
        let output = (0..MAX_WIFI_NETWORKS + 20)
            .map(|index| format!(" :net{index}:50:WPA2\n"))
            .collect::<String>();
        assert_eq!(parse_networks(&output).len(), MAX_WIFI_NETWORKS);

        let chatter = "not a network\n".repeat(MAX_WIFI_SCAN_LINES);
        let after_budget = format!("{chatter} :TooLate:80:WPA2\n");
        assert!(parse_networks(&after_budget).is_empty());
    }

    #[test]
    fn scans_reject_impossible_ssids_and_bound_retained_fields() {
        let valid_ssid = "s".repeat(MAX_WIFI_SSID_BYTES);
        let invalid_ssid = "s".repeat(MAX_WIFI_SSID_BYTES + 1);
        let security = "W".repeat(MAX_WIFI_SECURITY_BYTES + 20);
        let oversized_line = format!(" :oversized:50:{}\n", "W".repeat(MAX_WIFI_SCAN_LINE_BYTES));
        let output =
            format!(" :{valid_ssid}:70:{security}\n :{invalid_ssid}:80:WPA2\n{oversized_line}");
        let networks = parse_networks(&output);

        assert_eq!(networks.len(), 1);
        assert_eq!(networks[0].ssid, valid_ssid);
        assert_eq!(networks[0].security.len(), MAX_WIFI_SECURITY_BYTES);
    }

    #[test]
    fn picker_rows_carry_signal_lock_and_in_use_markers() {
        let mut connected = network("ENGINEAI", 72, "WPA2");
        connected.in_use = true;
        let row = picker_row(&connected);
        assert!(row.contains("ENGINEAI"));
        assert!(row.contains("72%"));
        assert!(row.contains('\u{f023}'), "secured networks show a lock");
        assert!(row.contains('\u{f00c}'), "the network in use is marked");

        let open = picker_row(&network("FreeWifi", 30, ""));
        assert!(!open.contains('\u{f023}'));
        assert!(!open.contains('\u{f00c}'));
    }

    #[test]
    fn signal_icons_distinguish_weak_from_strong() {
        assert_eq!(signal_icon(10), signal_icon(49));
        assert_eq!(signal_icon(50), signal_icon(90));
        assert_ne!(signal_icon(49), signal_icon(50));
    }

    #[test]
    fn every_glyph_stays_in_the_widely_available_range() {
        // FontAwesome-5-era f6xx codepoints are absent from common Nerd Font
        // builds and render as a hollow box; the shell sticks to FA4.
        let rows = [
            picker_row(&network("Net", 80, "WPA2")),
            picker_row(&network("Open", 20, "")),
            network_row(&NetworkState {
                wifi_enabled: false,
                connection: Some("Wired".into()),
                kind: LinkKind::Wired,
                signal: None,
            }),
            network_row(&NetworkState::default()),
            bluetooth_row(&BluetoothState {
                present: true,
                powered: true,
            }),
        ];
        for row in rows {
            for ch in row
                .chars()
                .filter(|ch| ('\u{f000}'..'\u{f900}').contains(ch))
            {
                assert!(
                    (ch as u32) < 0xf600,
                    "{ch:?} (U+{:04X}) is outside the FontAwesome-4 range",
                    ch as u32
                );
            }
        }
    }

    fn device(name: &str, connected: bool, paired: bool) -> BluetoothDevice {
        BluetoothDevice {
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            name: name.to_string(),
            connected,
            paired,
            rssi: None,
        }
    }

    #[test]
    fn device_lists_keep_names_containing_spaces() {
        let output =
            "Device AA:BB:CC:DD:EE:FF Ada's WH-1000XM4\nDevice 11:22:33:44:55:66 Magic Keyboard\n";
        let devices = parse_devices(output);

        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].address, "AA:BB:CC:DD:EE:FF");
        assert_eq!(devices[0].name, "Ada's WH-1000XM4");
        assert_eq!(devices[1].name, "Magic Keyboard");
    }

    #[test]
    fn a_nameless_device_falls_back_to_its_address() {
        let devices = parse_devices("Device AA:BB:CC:DD:EE:FF\n");
        assert_eq!(devices[0].name, "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn non_device_chatter_is_ignored() {
        // bluetoothctl prints banners and prompts around the list.
        let output = "Agent registered\nDevice AA:BB:CC:DD:EE:FF Speaker\n[bluetooth]# \n";
        assert_eq!(parse_devices(output).len(), 1);
        assert!(parse_devices("").is_empty());
    }

    #[test]
    fn device_list_bounds_the_follow_up_info_fanout() {
        let output = (0..=MAX_BLUETOOTH_DEVICES)
            .map(|index| {
                format!(
                    "Device 00:00:00:00:{:02X}:{:02X} Device {index}\n",
                    index / 256,
                    index % 256
                )
            })
            .collect::<String>();
        assert_eq!(parse_devices(&output).len(), MAX_BLUETOOTH_DEVICES);

        let chatter = "not a device\n".repeat(MAX_BLUETOOTH_DEVICE_LINES);
        let after_budget = format!("{chatter}Device AA:BB:CC:DD:EE:FF Too Late\n");
        assert!(parse_devices(&after_budget).is_empty());
    }

    #[test]
    fn device_list_validates_deduplicates_and_bounds_names() {
        let long_name = "x".repeat(MAX_BLUETOOTH_DEVICE_NAME_CHARS + 20);
        let output = format!(
            "Device AA:BB:CC:DD:EE:FF {long_name}\n\
             Device aa:bb:cc:dd:ee:ff duplicate\n\
             Device not-an-address invalid\n"
        );
        let devices = parse_devices(&output);
        assert_eq!(devices.len(), 1);
        assert_eq!(
            devices[0].name.chars().count(),
            MAX_BLUETOOTH_DEVICE_NAME_CHARS
        );
    }

    #[test]
    fn device_info_reports_connection_and_pairing() {
        let output =
            "Device AA:BB:CC:DD:EE:FF (public)\n\tName: Speaker\n\tPaired: yes\n\tConnected: yes\n";
        assert_eq!(parse_device_info(output), (true, true));

        let idle = "\tPaired: yes\n\tConnected: no\n";
        assert_eq!(parse_device_info(idle), (false, true));
        assert_eq!(parse_device_info(""), (false, false));
    }

    #[test]
    fn bonded_counts_as_paired() {
        // Newer bluetoothctl reports Bonded rather than Paired.
        assert_eq!(parse_device_info("\tBonded: yes\n"), (false, true));
    }

    #[test]
    fn the_list_puts_connected_devices_first() {
        let mut devices = vec![
            device("Zeta", false, false),
            device("Alpha", false, true),
            device("Beta", true, true),
        ];
        sort_devices(&mut devices);
        let names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Beta", "Alpha", "Zeta"]);
    }

    #[test]
    fn among_equals_the_loudest_device_is_the_one_in_your_hand() {
        // A discovery scan in a populated room returns mostly nameless
        // beacons, so the tie-break that matters is proximity, not the MAC
        // address masquerading as a name.
        let heard = |name: &str, rssi: i16| BluetoothDevice {
            rssi: Some(rssi),
            ..device(name, false, false)
        };
        let mut devices = vec![
            heard("AA-BB-CC-00-00-01", -90),
            heard("AA-BB-CC-00-00-02", -41),
            heard("AA-BB-CC-00-00-03", -67),
        ];
        sort_devices(&mut devices);
        let names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "AA-BB-CC-00-00-02",
                "AA-BB-CC-00-00-03",
                "AA-BB-CC-00-00-01"
            ]
        );

        // State still wins over signal: a connected device stays on top even
        // when something unpaired is shouting next to it.
        let mut mixed = vec![heard("Loud beacon", -30), device("Speaker", true, true)];
        sort_devices(&mut mixed);
        assert_eq!(mixed[0].name, "Speaker");

        // The row shows the reading where a bond state would otherwise be,
        // so a scan's worth of nameless devices is at least orderable by eye.
        assert!(device_row(&heard("Beacon", -41)).contains("-41 dBm"));
        assert!(device_row(&device("Speaker", true, true)).contains("connected"));
        assert!(!device_row(&device("Keyboard", false, false)).contains("dBm"));

        // Devices nobody heard sort after the ones that were, and keep the
        // old alphabetical order among themselves.
        let mut silent = vec![
            device("Zeta", false, false),
            heard("Quiet", -95),
            device("Alpha", false, false),
        ];
        sort_devices(&mut silent);
        let names: Vec<&str> = silent.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Quiet", "Alpha", "Zeta"]);
    }

    #[test]
    fn the_helper_device_list_parses_into_rows_without_a_follow_up_sweep() {
        let devices = parse_bridge_devices(
            r#"[
              {"address":"5c:fb:7c:1a:2b:3c","name":"WH-1000XM4",
               "paired":true,"connected":true,"rssi":-52},
              {"address":"7C:10:C9:AA:BB:CC","name":"  ","paired":false,
               "connected":false,"rssi":null}
            ]"#,
        );
        assert_eq!(devices.len(), 2);
        // Addresses are upper-cased so they match every other wire form.
        assert_eq!(devices[0].address, "5C:FB:7C:1A:2B:3C");
        assert_eq!(devices[0].name, "WH-1000XM4");
        // Paired/Connected arrive with the list; no `bluetoothctl info` run.
        assert!(devices[0].paired);
        assert!(devices[0].connected);
        assert_eq!(devices[0].rssi, Some(-52));
        // A blank name is not a row the user can read: fall back to the
        // address, which is at least the handle they have.
        assert_eq!(devices[1].name, "7C:10:C9:AA:BB:CC");
        assert_eq!(devices[1].rssi, None);
    }

    #[test]
    fn a_device_name_can_never_add_a_line_to_the_picker() {
        // Rows are joined with newlines and split back into lines, while the
        // selection pill and the pointer hit-map count *items* — so one
        // embedded newline in a name chosen by a stranger in radio range
        // draws an extra line and shifts every row below it off its own
        // highlight, making Enter act on a device other than the one that
        // looks selected.
        let hostile = "Speaker\n\u{f293}   Office Speaker\u{1b}[2J";
        let devices = parse_bridge_devices(&format!(
            r#"[{{"address":"5C:FB:7C:1A:2B:3C","name":{}}}]"#,
            serde_json::Value::from(hostile)
        ));
        assert_eq!(devices.len(), 1);
        assert!(
            !devices[0].name.chars().any(char::is_control),
            "{:?}",
            devices[0].name
        );
        assert!(!device_row(&devices[0]).contains('\n'));

        // The text path gets the same treatment: `lines()` already stops a
        // newline, but nothing stopped the rest.
        let devices = parse_devices("Device 5C:FB:7C:1A:2B:3C Speaker\u{1b}[2Jx\n");
        assert!(!devices[0].name.chars().any(char::is_control));
        let devices = parse_scan_output("[NEW] Device 5C:FB:7C:1A:2B:3C Speaker\u{7}beep\n");
        assert!(!devices[0].name.chars().any(char::is_control));

        // A name that was *only* control characters is not a row the user
        // can read; it falls back to the address like an empty one.
        let devices = parse_bridge_devices(r#"[{"address":"5C:FB:7C:1A:2B:3C","name":""}]"#);
        assert_eq!(devices[0].name, "5C:FB:7C:1A:2B:3C");
    }

    #[test]
    fn the_device_cap_keeps_what_the_user_can_act_on() {
        // The cap exists for a crowded room, which is exactly the room where
        // truncating in wire order — object-path order, i.e. MAC order —
        // would drop a connected headset at `F0:…` behind sixty-four
        // nameless beacons.
        let mut entries: Vec<String> = (0..(MAX_BLUETOOTH_DEVICES + 20))
            .map(|index| {
                format!(
                    r#"{{"address":"00:BB:CC:{:02X}:{:02X}:{:02X}","rssi":-90}}"#,
                    index / 256,
                    index % 256,
                    index % 7
                )
            })
            .collect();
        entries.push(
            r#"{"address":"F0:FF:FF:FF:FF:FF","name":"WH-1000XM4","paired":true,"connected":true}"#
                .to_string(),
        );
        let devices = parse_bridge_devices(&format!("[{}]", entries.join(",")));
        assert_eq!(devices.len(), MAX_BLUETOOTH_DEVICES);
        assert_eq!(
            devices[0].name, "WH-1000XM4",
            "the connected device survives the cap and leads the list"
        );
    }

    #[test]
    fn helper_output_is_validated_the_way_bluetoothctl_output_is() {
        // Not JSON, not an array, and an empty payload: all empty lists, not
        // panics — the helper is a child process like any other.
        assert!(parse_bridge_devices("bluetoothd crashed").is_empty());
        assert!(parse_bridge_devices("{}").is_empty());
        assert!(parse_bridge_devices("").is_empty());

        // Entries that could not name a device are dropped rather than
        // reaching the panel or, worse, a command argument.
        let devices = parse_bridge_devices(
            r#"[{"name":"no address"},
                {"address":"not-an-address","name":"junk"},
                {"address":"5C:FB:7C:1A:2B:3C","name":"Real"},
                {"address":"5c:fb:7c:1a:2b:3c","name":"Duplicate"}]"#,
        );
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "Real");

        // The device and name caps hold against a hostile helper payload.
        let long = "x".repeat(MAX_BLUETOOTH_DEVICE_NAME_CHARS * 4);
        let flood: Vec<String> = (0..(MAX_BLUETOOTH_DEVICES + 40))
            .map(|index| {
                format!(
                    r#"{{"address":"AA:BB:CC:{:02X}:{:02X}:{:02X}","name":"{long}"}}"#,
                    index / 256,
                    index % 256,
                    index % 11
                )
            })
            .collect();
        let devices = parse_bridge_devices(&format!("[{}]", flood.join(",")));
        assert_eq!(devices.len(), MAX_BLUETOOTH_DEVICES);
        assert!(
            devices
                .iter()
                .all(|d| d.name.chars().count() <= MAX_BLUETOOTH_DEVICE_NAME_CHARS)
        );

        // An out-of-range RSSI is dropped rather than saturated: a wrong
        // number would reorder the list.
        let devices = parse_bridge_devices(r#"[{"address":"5C:FB:7C:1A:2B:3C","rssi":-99999}]"#);
        assert_eq!(devices[0].rssi, None);
    }

    #[test]
    fn activating_a_device_toggles_its_connection() {
        assert_eq!(device_action(&device("Speaker", false, true)), "connect");
        assert_eq!(device_action(&device("Speaker", true, true)), "disconnect");
        // Never bonded: pairing must come before connecting is meaningful.
        assert_eq!(device_action(&device("Keyboard", false, false)), "pair");
    }

    #[test]
    fn scan_output_collects_new_devices_and_name_changes() {
        // Real `bluetoothctl --timeout 15 scan on` output: [NEW] on first
        // sighting, then property churn as advertisements arrive.
        let output = "[NEW] Device 5C:FB:7C:1A:2B:3C WH-1000XM4\n\
                      [CHG] Device 5C:FB:7C:1A:2B:3C RSSI: -61\n\
                      [CHG] Device 5C:FB:7C:1A:2B:3C TxPower: 4\n\
                      [NEW] Device 7C:10:C9:AA:BB:CC\n\
                      [CHG] Device 7C:10:C9:AA:BB:CC Name: MX Master 3S\n\
                      [CHG] Device 7C:10:C9:AA:BB:CC Alias: MX Master 3S\n\
                      [NEW] Device not-an-address junk\n\
                      [DEL] Device 5C:FB:7C:1A:2B:3C WH-1000XM4\n";
        let devices = parse_scan_output(output);

        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name, "WH-1000XM4");
        assert!(!devices[0].paired);
        assert!(!devices[0].connected);
        // Heard unnamed, named by a later Name: change.
        assert_eq!(devices[1].name, "MX Master 3S");
    }

    #[test]
    fn scan_output_deduplicates_and_bounds_the_catch() {
        let repeated = "[NEW] Device 5C:FB:7C:1A:2B:3C Speaker\n\
                        [CHG] Device 5C:fb:7c:1a:2b:3c Name: Speaker\n";
        assert_eq!(parse_scan_output(repeated).len(), 1);
        assert!(parse_scan_output("").is_empty());
        assert!(parse_scan_output("[bluetooth]# scan on\n").is_empty());

        let crowd = (0..MAX_BLUETOOTH_DEVICES + 20)
            .map(|index| {
                format!(
                    "[NEW] Device 00:00:00:{:02X}:{:02X}:{:02X} D{index}\n",
                    index / 65536,
                    (index / 256) % 256,
                    index % 256
                )
            })
            .collect::<String>();
        assert_eq!(parse_scan_output(&crowd).len(), MAX_BLUETOOTH_DEVICES);
    }

    #[test]
    fn discovered_devices_merge_under_the_remembered_list() {
        let mut remembered = vec![BluetoothDevice {
            address: "5C:FB:7C:1A:2B:3C".to_string(),
            name: "Speaker".to_string(),
            connected: false,
            paired: true,
            rssi: None,
        }];
        let discovered = parse_scan_output(
            "[NEW] Device 5c:fb:7c:1a:2b:3c Speaker\n[NEW] Device 7C:10:C9:AA:BB:CC Keyboard\n",
        );
        merge_discovered(&mut remembered, discovered);

        assert_eq!(remembered.len(), 2);
        // The remembered entry wins: its live flags survive the merge.
        assert!(remembered[0].paired);
        assert_eq!(remembered[1].name, "Keyboard");
        assert!(!remembered[1].paired);
    }

    #[test]
    fn device_rows_mark_the_connected_one() {
        let row = device_row(&device("Speaker", true, true));
        assert!(row.contains("Speaker"));
        assert!(row.contains("connected"));
        assert!(row.contains('\u{f00c}'));
        assert!(!device_row(&device("Speaker", false, true)).contains('\u{f00c}'));
        assert!(device_row(&device("Speaker", false, true)).contains("paired"));
    }

    #[test]
    fn bluetoothctl_failures_are_condensed() {
        let output = "Attempting to connect to AA:BB\nFailed to connect: org.bluez.Error.Failed br-connection-profile-unavailable\n";
        let message = summarize_bluetoothctl_error(output);
        assert!(message.starts_with("Failed to connect"));
        assert!(message.chars().count() <= 72);
        assert_eq!(
            summarize_bluetoothctl_error("Attempting to connect\n"),
            "the device did not respond"
        );
    }

    #[test]
    fn enter_on_a_working_radio_opens_the_picker_and_never_switches_it_off() {
        // The regression this guards: Enter used to toggle, so selecting the
        // row on a connected machine dropped the network.
        assert_eq!(
            plan_network_row(true, true, false),
            NetworkRowAction::OpenPicker
        );
        for radio_on in [true, false] {
            let action = plan_network_row(radio_on, true, false);
            assert_ne!(action, NetworkRowAction::SetRadio(false));
        }
    }

    #[test]
    fn the_bluetooth_row_mirrors_the_network_row() {
        // Enter opens the list, never powers a working controller down.
        assert_eq!(
            plan_bluetooth_row(true, true, false),
            BluetoothRowAction::OpenPicker
        );
        assert_eq!(
            plan_bluetooth_row(false, true, false),
            BluetoothRowAction::PowerOn
        );
        assert_eq!(
            plan_bluetooth_row(true, false, true),
            BluetoothRowAction::SetPower(false)
        );
        assert_eq!(
            plan_bluetooth_row(true, false, false),
            BluetoothRowAction::Nothing
        );
    }

    #[test]
    fn enter_switches_a_disabled_radio_on() {
        // With the radio off there is nothing to pick, so Enter enables it.
        assert_eq!(
            plan_network_row(false, true, false),
            NetworkRowAction::EnableRadio
        );
    }

    #[test]
    fn switching_the_radio_off_takes_an_explicit_left_or_right() {
        assert_eq!(
            plan_network_row(true, false, true),
            NetworkRowAction::SetRadio(false)
        );
        assert_eq!(
            plan_network_row(false, false, true),
            NetworkRowAction::SetRadio(true)
        );
    }

    #[test]
    fn other_keys_leave_the_radio_alone() {
        assert_eq!(
            plan_network_row(true, false, false),
            NetworkRowAction::Nothing
        );
    }

    #[test]
    fn a_saved_network_joins_without_asking_for_a_passphrase() {
        let secured = network("ENGINEAI", 72, "WPA2");
        assert_eq!(plan_connect(&secured, true, None), ConnectPlan::UseSaved);
    }

    #[test]
    fn an_unknown_secured_network_must_prompt_first() {
        let secured = network("Neighbour", 60, "WPA2");
        assert_eq!(
            plan_connect(&secured, false, None),
            ConnectPlan::NeedsPassphrase
        );
        assert_eq!(
            plan_connect(&secured, false, Some("hunter2")),
            ConnectPlan::WithPassphrase
        );
        // An empty prompt is not an answer; keep asking.
        assert_eq!(
            plan_connect(&secured, false, Some("")),
            ConnectPlan::NeedsPassphrase
        );
    }

    #[test]
    fn an_open_network_joins_directly() {
        assert_eq!(
            plan_connect(&network("FreeWifi", 40, ""), false, None),
            ConnectPlan::Open
        );
    }

    #[test]
    fn nmcli_errors_are_condensed_to_one_line() {
        let stderr = "Error: Connection activation failed: (7) Secrets were required, but not provided.\nmore noise\n";
        let message = summarize_nmcli_error(stderr);
        assert!(message.starts_with("Connection activation failed"));
        assert!(!message.contains('\n'));
        assert!(message.chars().count() <= 72);

        assert_eq!(summarize_nmcli_error(""), "connection failed");
    }

    #[test]
    fn connectivity_helpers_bound_stderr_and_wait_time() {
        let error = connectivity_output(
            "sh",
            &["-c", "printf 123456789 >&2"],
            Duration::from_secs(1),
            8,
        )
        .expect_err("oversized stderr must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        let started = std::time::Instant::now();
        let error = connectivity_output(
            "sh",
            &["-c", "exec sleep 10"],
            Duration::from_millis(25),
            64,
        )
        .expect_err("sleeping helper must time out");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_background_job_hands_its_result_back_once() {
        let job = BackgroundJob::spawn(|| 42_u32);
        let mut value = None;
        for _ in 0..200 {
            if let Some(result) = job.take() {
                value = Some(result);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(value, Some(42));
        // Taken once: a second poll must not repeat the work's result.
        assert_eq!(job.take(), None);
    }

    #[test]
    fn a_notified_background_job_publishes_before_waking() {
        let notifier = crate::backend::update_notifier::AsyncUpdateNotifier::new().unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        let job = BackgroundJob::spawn(move || {
            wait.recv().unwrap();
            73_u32
        })
        .with_notifier(Some(notifier.clone()));

        release.send(()).unwrap();
        let mut woke = false;
        for _ in 0..200 {
            if notifier.drain().unwrap() > 0 {
                woke = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(woke, "job completion did not signal its owning handler");
        assert_eq!(
            job.take(),
            Some(73),
            "a wake must never precede result publication"
        );
    }

    #[test]
    fn unnotified_jobs_keep_the_idle_poll_fallback() {
        let (release, wait) = std::sync::mpsc::channel();
        let job = BackgroundJob::spawn(move || {
            wait.recv().unwrap();
        });
        assert!(!job.readiness_is_covered());

        let notifier = crate::backend::update_notifier::AsyncUpdateNotifier::new().unwrap();
        let job = job.with_notifier(Some(notifier));
        assert!(job.readiness_is_covered());
        release.send(()).unwrap();
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
