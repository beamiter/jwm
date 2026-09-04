//! Bluetooth pairing session state for the control center.
//!
//! Pairing cannot be driven by `bluetoothctl` the way connect/disconnect is:
//! BlueZ wants an interactive agent answering PIN and passkey callbacks on the
//! system bus. That work lives in the one-shot helpers `jwm-bridge pair
//! <address>` and `jwm-bridge accept`; this module is the compositor's side of
//! the conversation — a pure state machine plus the IPC wire mapping, so every
//! rule (who may prompt, what a PIN may look like, when a session is stale) is
//! unit tested without a bus, a helper, or a panel.
//!
//! A session runs in one of two directions, and the direction is the only
//! thing that differs:
//!
//! - **Outbound** — the user picked a device and pressed Enter. The address is
//!   known before the helper starts and every callback is checked against it.
//! - **Inbound** — the user armed a bounded window and something out there
//!   asked to bond, or a bonded device asked for a service. Nobody can know
//!   the address in advance, so it is pinned by the first callback and
//!   enforced from then on. Arming is always an explicit gesture: without one
//!   there is no agent, and BlueZ answers such a request by refusing it.
//!
//! Wire contract, both directions scoped by a per-session cookie jwm mints and
//! hands to the helper through its environment:
//!
//! - helper → jwm commands: `bluetooth_pairing_prompt` (bluez asked
//!   something; only updates the panel) and `bluetooth_pairing_done`
//!   (terminal outcome).
//! - jwm → helper event on the `bluetooth` topic: `bluetooth/pairing_response`
//!   carrying the user's answer, or a cancellation. jwm broadcasts responses
//!   only while a session is active; a late answer for a dead session never
//!   leaves the compositor, and the helper ignores anything whose cookie is
//!   not its own.

use std::time::{Duration, Instant};

use serde_json::Value;

/// How long a prompt may sit unanswered before jwm withdraws it and cancels
/// the request. The helper waits a little longer, so jwm's cancel is what
/// normally ends a stale prompt.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(25);
/// Hard bound on one session. The helper has its own ~90s wall clock; this
/// catches a helper that died before it could report `done`.
pub const SESSION_TIMEOUT: Duration = Duration::from_secs(95);
/// BlueZ PINs are 1..=16 characters (the spec bounds them at 16 octets).
pub const MAX_PIN_CHARS: usize = 16;
/// Displayed codes (PIN display, passkey) never need more room than a PIN.
pub const MAX_CODE_CHARS: usize = 16;
/// Passkeys are numeric comparisons in 0..=999999, rendered six digits.
pub const MAX_PASSKEY: u64 = 999_999;
/// Cookies are 16 lowercase hex chars; reject anything wildly different
/// rather than growing a session record from attacker-controlled length.
const MAX_COOKIE_CHARS: usize = 64;
/// One failure line on the picker; keep it single-line and short.
const MAX_ERROR_CHARS: usize = 96;
/// A device name a remote peer chose, rendered on a modal panel. The same
/// bound the picker's own device list uses.
const MAX_DEVICE_NAME_CHARS: usize = 248;
/// A canonical Bluetooth service UUID is 36 characters. The bound is a little
/// wider so a profile *name* fits too, and no wider: the string comes from a
/// remote peer and is rendered on a modal panel.
pub const MAX_SERVICE_CHARS: usize = 64;
/// An inbound window is shorter than a pairing session's 95s: it is armed
/// speculatively, so it holds the controller discoverable and the panel's
/// prompt slot for as long as it lives.
pub const INBOUND_WINDOW: Duration = Duration::from_secs(60);

/// The event name jwm broadcasts pairing answers on.
pub const RESPONSE_EVENT: &str = "bluetooth/pairing_response";

/// What bluez is asking for, as named on the `bluetooth_pairing_prompt` wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingPrompt {
    /// The user types a PIN (or numeric passkey) shown on the other device.
    Pin,
    /// Numeric comparison: confirm both sides show this passkey.
    Confirm { passkey: u32 },
    /// The user types this code on the *device*; the panel only displays it.
    Display { code: String },
    /// Something asked for something, and only the user can say yes.
    ///
    /// `service` distinguishes bluez's two inbound callbacks:
    /// `None` is `RequestAuthorization` — an unbonded device wants to pair
    /// with this machine; `Some(name)` is `AuthorizeService` — a bonded
    /// device wants to use a profile. Only ever reachable while an inbound
    /// window is armed.
    Authorize { service: Option<String> },
}

/// Which way a session runs. See the module docs: the direction decides
/// whether the address is known before the helper starts or pinned by the
/// first callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingKind {
    Outbound,
    Inbound,
}

impl PairingKind {
    /// Stable name for the `get_bluetooth_pairing` query. The helper reads it
    /// to confirm jwm is expecting the direction it is about to serve.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Outbound => "outbound",
            Self::Inbound => "inbound",
        }
    }
}

/// Where one pairing session stands. There is no persistent Done/Failed
/// state: the session record is dropped the moment `done` lands, and the
/// outcome lives on as the picker's status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingPhase {
    /// The helper is running and bluez has not asked anything (anymore).
    /// For an inbound session this is the armed window: nothing has asked.
    Working,
    AwaitingPin,
    AwaitingConfirm {
        passkey: u32,
    },
    Displaying {
        code: String,
    },
    AwaitingAuthorization {
        /// The profile a bonded device wants, or `None` for a bond request.
        service: Option<String>,
    },
}

impl PairingPhase {
    /// Stable name for the `get_bluetooth_pairing` query.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::AwaitingPin => "awaiting_pin",
            Self::AwaitingConfirm { .. } => "awaiting_confirm",
            Self::Displaying { .. } => "displaying",
            Self::AwaitingAuthorization { .. } => "awaiting_authorization",
        }
    }

    /// Whether the user is being asked something right now.
    #[must_use]
    pub fn is_prompting(&self) -> bool {
        !matches!(self, Self::Working)
    }
}

/// One live pairing session. At most one exists at a time, in either
/// direction: an inbound window can never displace or race an outbound
/// pairing, and vice versa.
#[derive(Debug, Clone)]
pub struct PairingSession {
    kind: PairingKind,
    /// `None` only for an inbound window nothing has called into yet. Once
    /// a callback names a device it is pinned, and every later message must
    /// name the same one.
    address: Option<String>,
    /// Display name for prompts; bluez callbacks carry only the address.
    device_name: String,
    cookie: String,
    phase: PairingPhase,
    started: Instant,
    prompt_started: Option<Instant>,
}

impl PairingSession {
    /// `None` for a malformed address — the session must never anchor on
    /// something that cannot name a bluez device.
    #[must_use]
    pub fn new(
        address: &str,
        device_name: &str,
        cookie: String,
        now: Instant,
    ) -> Option<PairingSession> {
        if !super::connectivity::is_bluetooth_address(address) {
            return None;
        }
        Some(PairingSession {
            kind: PairingKind::Outbound,
            address: Some(address.to_uppercase()),
            device_name: device_name.to_string(),
            cookie,
            phase: PairingPhase::Working,
            started: now,
            prompt_started: None,
        })
    }

    /// An armed inbound window. There is no address yet — that is the whole
    /// point — and no way to guess one, so it stays `None` until a callback
    /// names a device.
    #[must_use]
    pub fn inbound(cookie: String, now: Instant) -> PairingSession {
        PairingSession {
            kind: PairingKind::Inbound,
            address: None,
            device_name: String::new(),
            cookie,
            phase: PairingPhase::Working,
            started: now,
            prompt_started: None,
        }
    }

    #[must_use]
    pub fn kind(&self) -> PairingKind {
        self.kind
    }

    /// The device this session is bound to, once there is one.
    #[must_use]
    pub fn address(&self) -> Option<&str> {
        self.address.as_deref()
    }

    #[must_use]
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    #[must_use]
    pub fn cookie(&self) -> &str {
        &self.cookie
    }

    #[must_use]
    pub fn phase(&self) -> &PairingPhase {
        &self.phase
    }

    /// Whether this session is the one a helper message claims to serve.
    ///
    /// The cookie is a shared secret and compares exactly; the address is
    /// case-folded because bluez and bluetoothctl disagree on casing. An
    /// inbound window that has not been called into yet accepts any *valid*
    /// address — it cannot know which device will ring — and [`pin_address`]
    /// then fixes it, so the foreign-device protection is late-bound rather
    /// than absent.
    ///
    /// [`pin_address`]: PairingSession::pin_address
    #[must_use]
    pub fn matches(&self, address: &str, cookie: &str) -> bool {
        if self.cookie != cookie {
            return false;
        }
        match &self.address {
            Some(bound) => bound.eq_ignore_ascii_case(address),
            None => {
                self.kind == PairingKind::Inbound
                    && super::connectivity::is_bluetooth_address(address)
            }
        }
    }

    /// Bind an inbound window to the device that called into it, and give the
    /// panel something to name. Returns false when the session is already
    /// bound to a different device — the caller must refuse that message.
    pub fn pin_address(&mut self, address: &str, device_name: &str) -> bool {
        match &self.address {
            Some(bound) => bound.eq_ignore_ascii_case(address),
            None => {
                if !super::connectivity::is_bluetooth_address(address) {
                    return false;
                }
                self.address = Some(address.to_uppercase());
                self.device_name = if device_name.is_empty() {
                    address.to_uppercase()
                } else {
                    device_name.to_string()
                };
                true
            }
        }
    }

    /// bluez asked something: move into the matching prompt phase. A second
    /// prompt replaces the first — bluez serializes agent requests per Pair
    /// call, so a fresh prompt means the previous one was withdrawn.
    pub fn apply_prompt(&mut self, prompt: PairingPrompt, now: Instant) {
        self.phase = match prompt {
            PairingPrompt::Pin => PairingPhase::AwaitingPin,
            PairingPrompt::Confirm { passkey } => PairingPhase::AwaitingConfirm { passkey },
            PairingPrompt::Display { code } => PairingPhase::Displaying { code },
            PairingPrompt::Authorize { service } => PairingPhase::AwaitingAuthorization { service },
        };
        self.prompt_started = Some(now);
    }

    /// The prompt was answered (or locally timed out): back to waiting on the
    /// helper for whatever bluez does next.
    pub fn clear_prompt(&mut self) {
        self.phase = PairingPhase::Working;
        self.prompt_started = None;
    }

    /// Whether the current prompt has outlived [`PROMPT_TIMEOUT`].
    #[must_use]
    pub fn prompt_timed_out(&self, now: Instant) -> bool {
        self.phase.is_prompting()
            && self
                .prompt_started
                .is_some_and(|started| now.saturating_duration_since(started) >= PROMPT_TIMEOUT)
    }

    /// How long this session may live before jwm gives up on it. An inbound
    /// window is armed speculatively and holds the controller discoverable,
    /// so it expires sooner than a pairing the user is watching.
    #[must_use]
    pub fn lifetime(&self) -> Duration {
        match self.kind {
            PairingKind::Outbound => SESSION_TIMEOUT,
            PairingKind::Inbound => INBOUND_WINDOW,
        }
    }

    /// Whether the whole session has outlived [`PairingSession::lifetime`].
    #[must_use]
    pub fn session_timed_out(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= self.lifetime()
    }

    /// Time until the next deadline this session cares about, so the event
    /// loop can wake exactly when a timeout must be enforced. `None` when no
    /// session is running.
    #[must_use]
    pub fn next_timeout_in(&self, now: Instant) -> Option<Duration> {
        let session_deadline = self
            .lifetime()
            .saturating_sub(now.saturating_duration_since(self.started));
        let prompt_deadline = if self.phase.is_prompting() {
            self.prompt_started.map(|started| {
                PROMPT_TIMEOUT.saturating_sub(now.saturating_duration_since(started))
            })
        } else {
            None
        };
        Some(match prompt_deadline {
            Some(prompt) => prompt.min(session_deadline),
            None => session_deadline,
        })
    }
}

// ---------------------------------------------------------------------------
// Validation (pure)
// ---------------------------------------------------------------------------

/// What the user may submit as a PIN: 1..=16 non-control characters.
#[must_use]
pub fn valid_pin(pin: &str) -> bool {
    let length = pin.chars().count();
    (1..=MAX_PIN_CHARS).contains(&length) && !pin.chars().any(char::is_control)
}

/// A passkey is a numeric comparison in `0..=999999`.
#[must_use]
pub fn valid_passkey(passkey: u64) -> bool {
    passkey <= MAX_PASSKEY
}

/// Six-digit rendering, the form both sides of a numeric comparison show.
#[must_use]
pub fn format_passkey(passkey: u32) -> String {
    format!("{passkey:06}")
}

/// Mint a one-shot session cookie. The helper gets it through its
/// environment (never argv, which `ps` exposes) and echoes it back on every
/// message, which is how both sides drop stale traffic from a dead session.
#[must_use]
pub fn new_cookie() -> String {
    use std::io::Read as _;
    let entropy = std::fs::File::open("/dev/urandom")
        .and_then(|mut file| {
            let mut bytes = [0_u8; 8];
            file.read_exact(&mut bytes)
                .map(|()| u64::from_ne_bytes(bytes))
        })
        .unwrap_or_else(|_| fallback_entropy());
    format!("{entropy:016x}")
}

/// Time/pid/counter mix for machines without a readable urandom. Not
/// cryptographic — it still uniquely names a session, which is all the
/// cookie needs on a single-user socket.
fn fallback_entropy() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.subsec_nanos() as u64 ^ since.as_secs().rotate_left(32))
        .unwrap_or(0);
    nanos
        ^ (std::process::id() as u64).rotate_left(20)
        ^ COUNTER.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// IPC wire mapping (pure)
// ---------------------------------------------------------------------------

/// A validated `bluetooth_pairing_prompt` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCommand {
    pub address: String,
    pub cookie: String,
    pub prompt: PairingPrompt,
    /// What to call the device on screen. An outbound session already has a
    /// name from the picker row and ignores this; an inbound window has
    /// nothing but an address until the helper supplies one. Empty when the
    /// helper could not learn a name, in which case the address is the name.
    pub device_name: String,
}

/// A validated `bluetooth_pairing_done` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoneCommand {
    pub address: String,
    pub cookie: String,
    pub ok: bool,
    pub error: Option<String>,
    /// Whether the helper also brought the device's profiles up after the
    /// bond landed. `None` when there was nothing to connect — a failed
    /// pairing, or a helper too old to report it — which is why this is not
    /// a plain `bool`: "did not connect" and "was never asked to" put
    /// different words on the picker's status line.
    pub connected: Option<bool>,
}

fn required_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, String> {
    args.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("expected non-empty string field '{field}'"))
}

fn address_field(args: &Value) -> Result<String, String> {
    let address = required_str(args, "address")?;
    if !super::connectivity::is_bluetooth_address(address) {
        return Err(format!(
            "field 'address' is not a Bluetooth address: {address:?}"
        ));
    }
    Ok(address.to_uppercase())
}

fn cookie_field(args: &Value) -> Result<String, String> {
    let cookie = required_str(args, "cookie")?;
    if cookie.chars().count() > MAX_COOKIE_CHARS {
        return Err("field 'cookie' is unreasonably long".to_string());
    }
    Ok(cookie.to_string())
}

/// Parse and validate a `bluetooth_pairing_prompt` command. Unknown kinds and
/// malformed passkeys/codes are rejected here so the panel never renders
/// attacker-shaped strings.
pub fn parse_prompt_command(args: &Value) -> Result<PromptCommand, String> {
    let prompt = match required_str(args, "kind")? {
        "pin" => PairingPrompt::Pin,
        "confirm" => {
            let passkey = args
                .get("passkey")
                .and_then(Value::as_u64)
                .filter(|passkey| valid_passkey(*passkey))
                .ok_or_else(|| {
                    "kind 'confirm' expects integer field 'passkey' in 0..=999999".to_string()
                })?;
            PairingPrompt::Confirm {
                passkey: passkey as u32,
            }
        }
        "display" => {
            let code = required_str(args, "code")?;
            if code.chars().count() > MAX_CODE_CHARS || code.chars().any(char::is_control) {
                return Err(format!(
                    "field 'code' must be 1..={MAX_CODE_CHARS} printable characters"
                ));
            }
            PairingPrompt::Display {
                code: code.to_string(),
            }
        }
        "authorize" => {
            // Absent field = a bond request; present = a service request.
            // The value is remote-controlled text on a modal panel, so it is
            // bounded and stripped of control characters like every other
            // string that reaches the picker.
            let service = match args.get("service").and_then(Value::as_str) {
                None => None,
                Some(service) => {
                    let service = service.trim();
                    if service.is_empty()
                        || service.chars().count() > MAX_SERVICE_CHARS
                        || service.chars().any(char::is_control)
                    {
                        return Err(format!(
                            "field 'service' must be 1..={MAX_SERVICE_CHARS} printable characters"
                        ));
                    }
                    Some(service_label(service))
                }
            };
            PairingPrompt::Authorize { service }
        }
        kind => return Err(format!("unknown prompt kind {kind:?}")),
    };
    let device_name = args
        .get("device_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
        .map(|name| name.chars().take(MAX_DEVICE_NAME_CHARS).collect::<String>())
        .unwrap_or_default();
    Ok(PromptCommand {
        address: address_field(args)?,
        cookie: cookie_field(args)?,
        prompt,
        device_name,
    })
}

/// Turn a Bluetooth service UUID into something a person can decide about.
///
/// The wire carries whatever bluez passed to `AuthorizeService`, which for a
/// standard profile is a 128-bit UUID built on the SDP base — `0000110B-…`
/// is A2DP audio. A bare UUID is not a question anyone can answer, so the
/// well-known short forms get their profile name and everything else is
/// passed through unchanged (already bounded by the caller): an unrecognized
/// UUID shown verbatim is still better than a name invented for it.
#[must_use]
pub fn service_label(uuid: &str) -> String {
    const BASE_SUFFIX: &str = "-0000-1000-8000-00805F9B34FB";
    let upper = uuid.to_uppercase();
    let Some(short) = upper
        .strip_suffix(BASE_SUFFIX)
        .and_then(|head| head.strip_prefix("0000"))
    else {
        return uuid.to_string();
    };
    match short {
        "1108" => "Headset",
        "110A" => "Audio source",
        "110B" => "Audio sink",
        "110D" => "Audio (A2DP)",
        "110E" | "110C" => "Remote control",
        "111E" | "111F" => "Hands-free",
        "1112" | "1115" | "1116" => "Networking",
        "1124" => "Input device",
        "1105" => "File transfer",
        "1106" => "File access",
        "112F" | "1130" => "Phone book access",
        "1132" | "1133" => "Messaging",
        "1200" => "Device identification",
        _ => return uuid.to_string(),
    }
    .to_string()
}

/// Parse and validate a `bluetooth_pairing_done` command. The error text is
/// condensed to one bounded line: it lands on the picker's status row.
pub fn parse_done_command(args: &Value) -> Result<DoneCommand, String> {
    let ok = args
        .get("ok")
        .and_then(Value::as_bool)
        .ok_or_else(|| "expected boolean field 'ok'".to_string())?;
    let error = args
        .get("error")
        .and_then(Value::as_str)
        .map(|error| {
            error
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .chars()
                .take(MAX_ERROR_CHARS)
                .collect::<String>()
        })
        .filter(|error| !error.is_empty());
    Ok(DoneCommand {
        address: address_field(args)?,
        cookie: cookie_field(args)?,
        ok,
        error,
        // A pairing that failed has nothing to connect; a helper that does
        // not send the field leaves the verdict unknown rather than false.
        connected: ok
            .then(|| args.get("connected").and_then(Value::as_bool))
            .flatten(),
    })
}

/// The picker's status line after a pairing succeeded.
///
/// The bond is what the command asked for and it landed either way, so this
/// never reads as a failure. It does distinguish the three outcomes, because
/// "Paired" on a headset that is not playing anything is the report that
/// sends a user back to the panel to find out why.
#[must_use]
pub fn paired_message(device_name: &str, connected: Option<bool>) -> String {
    match connected {
        Some(true) => format!("Connected {device_name}"),
        Some(false) => format!("Paired {device_name} \u{2014} not connected"),
        None => format!("Paired {device_name}"),
    }
}

/// The user's answer to a prompt, broadcast as [`RESPONSE_EVENT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingAnswer<'a> {
    /// A typed PIN/passkey. The string goes on the wire; it is never logged.
    Pin(&'a str),
    /// Confirm prompt accepted.
    Confirmed,
    /// Confirm prompt actively refused (`n`).
    Rejected,
    /// Esc, panel close, or prompt timeout.
    Cancelled,
}

/// Build the `bluetooth/pairing_response` event payload.
#[must_use]
pub fn response_payload(cookie: &str, answer: PairingAnswer<'_>) -> Value {
    match answer {
        PairingAnswer::Pin(pin) => {
            serde_json::json!({ "cookie": cookie, "accepted": true, "pin": pin })
        }
        PairingAnswer::Confirmed => {
            serde_json::json!({ "cookie": cookie, "accepted": true })
        }
        PairingAnswer::Rejected => {
            serde_json::json!({ "cookie": cookie, "accepted": false, "reason": "rejected" })
        }
        PairingAnswer::Cancelled => {
            serde_json::json!({ "cookie": cookie, "accepted": false, "reason": "cancelled" })
        }
    }
}

/// The `get_bluetooth_pairing` query answer, which is also the helper's
/// startup self-heal: a session jwm no longer holds means the helper should
/// exit without ever touching bluez.
#[must_use]
pub fn session_json(session: Option<&PairingSession>) -> Value {
    match session {
        Some(session) => serde_json::json!({
            "active": true,
            // Null while an armed inbound window is waiting for its first
            // caller; the helper's liveness check tolerates that only for
            // the direction it is serving.
            "address": session.address(),
            "cookie": session.cookie(),
            "state": session.phase().as_str(),
            "kind": session.kind().as_str(),
        }),
        None => serde_json::json!({ "active": false }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: &str = "5C:FB:7C:1A:2B:3C";

    fn session(now: Instant) -> PairingSession {
        PairingSession::new(ADDR, "WH-1000XM4", "0123456789abcdef".to_string(), now)
            .expect("valid session")
    }

    // --- Session construction and identity ---

    #[test]
    fn a_session_requires_a_real_bluetooth_address() {
        let now = Instant::now();
        assert!(session(now).matches(ADDR, "0123456789abcdef"));
        assert!(PairingSession::new("not-an-address", "x", "c".to_string(), now).is_none());
        assert!(PairingSession::new("", "x", "c".to_string(), now).is_none());
    }

    #[test]
    fn identity_matches_case_folded_address_but_exact_cookie() {
        let session = session(Instant::now());
        assert!(session.matches(&ADDR.to_lowercase(), "0123456789abcdef"));
        assert!(!session.matches(ADDR, "0123456789abcdee"));
        assert!(!session.matches("11:22:33:44:55:66", "0123456789abcdef"));
    }

    // --- Transition matrix ---

    #[test]
    fn prompts_move_working_into_each_prompt_phase() {
        let now = Instant::now();
        let mut session = session(now);
        assert_eq!(session.phase(), &PairingPhase::Working);

        session.apply_prompt(PairingPrompt::Pin, now);
        assert_eq!(session.phase(), &PairingPhase::AwaitingPin);
        assert!(session.phase().is_prompting());

        session.clear_prompt();
        assert_eq!(session.phase(), &PairingPhase::Working);
        assert!(!session.phase().is_prompting());

        session.apply_prompt(PairingPrompt::Confirm { passkey: 42 }, now);
        assert_eq!(
            session.phase(),
            &PairingPhase::AwaitingConfirm { passkey: 42 }
        );

        // A fresh prompt replaces an unanswered one: bluez withdrew the first.
        session.apply_prompt(
            PairingPrompt::Display {
                code: "1234".into(),
            },
            now,
        );
        assert_eq!(
            session.phase(),
            &PairingPhase::Displaying {
                code: "1234".into()
            }
        );
    }

    #[test]
    fn prompt_deadline_applies_only_while_prompting() {
        let now = Instant::now();
        let mut session = session(now);
        assert!(!session.prompt_timed_out(now + PROMPT_TIMEOUT));

        session.apply_prompt(PairingPrompt::Pin, now);
        assert!(!session.prompt_timed_out(now + PROMPT_TIMEOUT - Duration::from_secs(1)));
        assert!(session.prompt_timed_out(now + PROMPT_TIMEOUT));
        assert!(!session.session_timed_out(now + PROMPT_TIMEOUT));

        session.clear_prompt();
        assert!(!session.prompt_timed_out(now + PROMPT_TIMEOUT + Duration::from_secs(1)));
    }

    #[test]
    fn session_deadline_bounds_the_whole_exchange() {
        let now = Instant::now();
        let session = session(now);
        assert!(!session.session_timed_out(now + SESSION_TIMEOUT - Duration::from_secs(1)));
        assert!(session.session_timed_out(now + SESSION_TIMEOUT));
    }

    #[test]
    fn next_timeout_tracks_the_earlier_deadline() {
        let now = Instant::now();
        let mut session = session(now);
        assert_eq!(session.next_timeout_in(now), Some(SESSION_TIMEOUT));

        session.apply_prompt(PairingPrompt::Pin, now + Duration::from_secs(3));
        assert_eq!(
            session.next_timeout_in(now + Duration::from_secs(3)),
            Some(PROMPT_TIMEOUT)
        );
        // Half the prompt budget spent: the prompt deadline still leads.
        assert_eq!(
            session.next_timeout_in(now + Duration::from_secs(3) + PROMPT_TIMEOUT / 2),
            Some(PROMPT_TIMEOUT / 2)
        );
    }

    // --- Validation ---

    #[test]
    fn pin_validation_bounds_length_and_controls() {
        assert!(valid_pin("0"));
        assert!(valid_pin("123456"));
        assert!(valid_pin("a".repeat(MAX_PIN_CHARS).as_str()));
        assert!(!valid_pin(""));
        assert!(!valid_pin("a".repeat(MAX_PIN_CHARS + 1).as_str()));
        assert!(!valid_pin("12\n34"));
        assert!(!valid_pin("12\0 34"));
    }

    #[test]
    fn passkeys_are_six_digit_numeric_comparisons() {
        assert!(valid_passkey(0));
        assert!(valid_passkey(MAX_PASSKEY));
        assert!(!valid_passkey(MAX_PASSKEY + 1));
        assert_eq!(format_passkey(42), "000042");
        assert_eq!(format_passkey(999_999), "999999");
    }

    #[test]
    fn cookies_are_unique_sixteen_lowercase_hex_chars() {
        let one = new_cookie();
        let other = new_cookie();
        assert_eq!(one.len(), 16);
        assert!(one.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(
            other
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        );
        assert_ne!(one, other);
    }

    // --- Wire mapping ---

    #[test]
    fn prompt_commands_parse_each_kind() {
        let pin = parse_prompt_command(&serde_json::json!({
            "address": ADDR, "cookie": "c", "kind": "pin",
        }))
        .unwrap();
        assert_eq!(pin.prompt, PairingPrompt::Pin);
        assert_eq!(pin.address, ADDR);

        let confirm = parse_prompt_command(&serde_json::json!({
            "address": ADDR, "cookie": "c", "kind": "confirm", "passkey": 123_456,
        }))
        .unwrap();
        assert_eq!(confirm.prompt, PairingPrompt::Confirm { passkey: 123_456 });

        let display = parse_prompt_command(&serde_json::json!({
            "address": ADDR, "cookie": "c", "kind": "display", "code": "987654",
        }))
        .unwrap();
        assert_eq!(
            display.prompt,
            PairingPrompt::Display {
                code: "987654".into()
            }
        );
    }

    #[test]
    fn prompt_commands_reject_malformed_input() {
        for args in [
            serde_json::json!({}),
            serde_json::json!({"address": ADDR, "cookie": "c"}),
            serde_json::json!({"address": "nope", "cookie": "c", "kind": "pin"}),
            serde_json::json!({"address": ADDR, "kind": "pin"}),
            serde_json::json!({"address": ADDR, "cookie": "c", "kind": "nope"}),
            serde_json::json!({"address": ADDR, "cookie": "c", "kind": "confirm"}),
            serde_json::json!({"address": ADDR, "cookie": "c", "kind": "confirm", "passkey": 1_000_000}),
            serde_json::json!({"address": ADDR, "cookie": "c", "kind": "confirm", "passkey": -1}),
            serde_json::json!({"address": ADDR, "cookie": "c", "kind": "confirm", "passkey": "123456"}),
            serde_json::json!({"address": ADDR, "cookie": "c", "kind": "display"}),
            serde_json::json!({"address": ADDR, "cookie": "c", "kind": "display", "code": ""}),
            serde_json::json!({"address": ADDR, "cookie": "c", "kind": "display", "code": "a\u{7}"}),
            serde_json::json!({"address": ADDR, "cookie": "c", "kind": "display", "code": "x".repeat(MAX_CODE_CHARS + 1)}),
            serde_json::json!({"address": ADDR, "cookie": "x".repeat(MAX_COOKIE_CHARS + 1), "kind": "pin"}),
        ] {
            assert!(
                parse_prompt_command(&args).is_err(),
                "accepted malformed prompt command: {args}"
            );
        }
    }

    #[test]
    fn lowercase_addresses_normalize_for_matching() {
        let command = parse_prompt_command(&serde_json::json!({
            "address": ADDR.to_lowercase(), "cookie": "c", "kind": "pin",
        }))
        .unwrap();
        assert_eq!(command.address, ADDR);
        let session =
            PairingSession::new(ADDR, "x", command.cookie.clone(), Instant::now()).unwrap();
        assert!(session.matches(&command.address, &command.cookie));
    }

    #[test]
    fn done_commands_parse_and_condense_the_error() {
        let done = parse_done_command(&serde_json::json!({
            "address": ADDR, "cookie": "c", "ok": false,
            "error": "first line\nsecond line",
        }))
        .unwrap();
        assert!(!done.ok);
        assert_eq!(done.error.as_deref(), Some("first line"));

        let ok = parse_done_command(&serde_json::json!({
            "address": ADDR, "cookie": "c", "ok": true,
        }))
        .unwrap();
        assert!(ok.ok);
        assert_eq!(ok.error, None);

        // Empty and over-long errors never reach the panel.
        let done = parse_done_command(&serde_json::json!({
            "address": ADDR, "cookie": "c", "ok": false, "error": "  \n".to_string(),
        }))
        .unwrap();
        assert_eq!(done.error, None);
        let done = parse_done_command(&serde_json::json!({
            "address": ADDR, "cookie": "c", "ok": false, "error": "x".repeat(400),
        }))
        .unwrap();
        assert_eq!(done.error.as_ref().map(String::len), Some(MAX_ERROR_CHARS));

        for args in [
            serde_json::json!({"address": ADDR, "cookie": "c"}),
            serde_json::json!({"address": ADDR, "cookie": "c", "ok": "yes"}),
            serde_json::json!({"address": "nope", "cookie": "c", "ok": true}),
        ] {
            assert!(parse_done_command(&args).is_err(), "accepted {args}");
        }
    }

    #[test]
    fn an_armed_window_binds_to_the_first_device_that_rings_and_then_only_that_one() {
        let now = Instant::now();
        let mut window = PairingSession::inbound("0123456789abcdef".to_string(), now);
        assert_eq!(window.kind(), PairingKind::Inbound);
        assert_eq!(window.address(), None, "nothing has rung yet");

        // Before it binds, the window accepts any valid address — it cannot
        // know which device will ring — but never a foreign cookie, and
        // never something that is not an address at all.
        assert!(window.matches(ADDR, "0123456789abcdef"));
        assert!(window.matches("11:22:33:44:55:66", "0123456789abcdef"));
        assert!(!window.matches(ADDR, "someone-elses-cookie"));
        assert!(!window.matches("not-an-address", "0123456789abcdef"));

        assert!(window.pin_address("5c:fb:7c:1a:2b:3c", "MX Master 3S"));
        assert_eq!(window.address(), Some(ADDR));
        assert_eq!(window.device_name(), "MX Master 3S");

        // From here it is exactly as narrow as an outbound session.
        assert!(window.matches(ADDR, "0123456789abcdef"));
        assert!(!window.matches("11:22:33:44:55:66", "0123456789abcdef"));
        assert!(
            !window.pin_address("11:22:33:44:55:66", "Somebody else"),
            "a second device must not take over a bound window"
        );

        // A nameless caller is named after itself rather than left blank.
        let mut nameless = PairingSession::inbound("cafe".to_string(), now);
        assert!(nameless.pin_address(ADDR, ""));
        assert_eq!(nameless.device_name(), ADDR);
        // And an address that could not name a bluez device binds nothing.
        let mut junk = PairingSession::inbound("cafe".to_string(), now);
        assert!(!junk.pin_address("not-an-address", "junk"));
        assert_eq!(junk.address(), None);
    }

    #[test]
    fn an_outbound_session_is_bound_before_it_starts_and_never_rebinds() {
        let mut outbound = session(Instant::now());
        assert_eq!(outbound.kind(), PairingKind::Outbound);
        assert_eq!(outbound.address(), Some(ADDR));
        // `pin_address` is a no-op that agrees, so one call site serves both
        // directions without having to ask which one it is holding.
        assert!(outbound.pin_address(ADDR, "renamed"));
        assert_eq!(
            outbound.device_name(),
            "WH-1000XM4",
            "the name is not rewritten"
        );
        assert!(!outbound.pin_address("11:22:33:44:55:66", "elsewhere"));
    }

    #[test]
    fn an_armed_window_expires_sooner_than_a_pairing_the_user_is_watching() {
        let start = Instant::now();
        let window = PairingSession::inbound("cafe".to_string(), start);
        assert_eq!(window.lifetime(), INBOUND_WINDOW);
        assert!(!window.session_timed_out(start + INBOUND_WINDOW - Duration::from_millis(1)));
        assert!(window.session_timed_out(start + INBOUND_WINDOW));
        // It holds the controller discoverable for its whole life, so it must
        // close well before a pairing session would.
        assert!(INBOUND_WINDOW < SESSION_TIMEOUT);
        assert_eq!(session(start).lifetime(), SESSION_TIMEOUT);
    }

    #[test]
    fn an_authorization_prompt_names_what_it_is_granting() {
        let bond = parse_prompt_command(&serde_json::json!({
            "address": ADDR, "cookie": "c", "kind": "authorize",
            "device_name": "MX Master 3S",
        }))
        .unwrap();
        assert_eq!(bond.prompt, PairingPrompt::Authorize { service: None });
        assert_eq!(bond.device_name, "MX Master 3S");

        // A well-known UUID becomes a profile a person can decide about.
        let service = parse_prompt_command(&serde_json::json!({
            "address": ADDR, "cookie": "c", "kind": "authorize",
            "service": "0000110B-0000-1000-8000-00805F9B34FB",
        }))
        .unwrap();
        assert_eq!(
            service.prompt,
            PairingPrompt::Authorize {
                service: Some("Audio sink".to_string())
            }
        );
        // Anything else is shown verbatim: an unrecognized UUID is a worse
        // answer than a name, and an invented name is worse than both.
        assert_eq!(
            service_label("0000FFF0-0000-1000-8000-00805F9B34FB"),
            "0000FFF0-0000-1000-8000-00805F9B34FB"
        );
        assert_eq!(service_label("a private uuid"), "a private uuid");
        // Short forms are matched case-insensitively.
        assert_eq!(
            service_label("0000110b-0000-1000-8000-00805f9b34fb"),
            "Audio sink"
        );

        // The panel never renders attacker-shaped strings: the service field
        // is bounded and stripped of control characters like every other
        // string on this wire.
        for bad in [
            serde_json::json!({"address": ADDR, "cookie": "c", "kind": "authorize", "service": ""}),
            serde_json::json!({"address": ADDR, "cookie": "c", "kind": "authorize",
                               "service": "a
b"}),
            serde_json::json!({"address": ADDR, "cookie": "c", "kind": "authorize",
                               "service": "x".repeat(MAX_SERVICE_CHARS + 1)}),
        ] {
            assert!(parse_prompt_command(&bad).is_err(), "accepted {bad}");
        }

        // A device name is bounded and control-stripped too, and an
        // unusable one leaves the session to fall back to the address.
        let unnamed = parse_prompt_command(&serde_json::json!({
            "address": ADDR, "cookie": "c", "kind": "authorize", "device_name": "  \n ",
        }))
        .unwrap();
        assert_eq!(unnamed.device_name, "");
        let long = parse_prompt_command(&serde_json::json!({
            "address": ADDR, "cookie": "c", "kind": "authorize",
            "device_name": "x".repeat(MAX_DEVICE_NAME_CHARS * 3),
        }))
        .unwrap();
        assert_eq!(long.device_name.chars().count(), MAX_DEVICE_NAME_CHARS);
    }

    #[test]
    fn the_query_says_which_direction_a_session_runs() {
        let now = Instant::now();
        let outbound = session_json(Some(&session(now)));
        assert_eq!(outbound["kind"], "outbound");
        assert_eq!(outbound["address"], ADDR);

        // An armed window reports a null address, which is what tells the
        // helper it is serving a window rather than a pairing.
        let window = session_json(Some(&PairingSession::inbound("cafe".to_string(), now)));
        assert_eq!(window["kind"], "inbound");
        assert!(window["address"].is_null());
        assert_eq!(window["state"], "working");

        let mut prompting = PairingSession::inbound("cafe".to_string(), now);
        prompting.apply_prompt(PairingPrompt::Authorize { service: None }, now);
        assert_eq!(
            session_json(Some(&prompting))["state"],
            "awaiting_authorization"
        );
        assert!(prompting.phase().is_prompting());
    }

    #[test]
    fn the_auto_connect_verdict_is_only_meaningful_on_a_successful_pairing() {
        let connected = parse_done_command(&serde_json::json!({
            "address": ADDR, "cookie": "c", "ok": true, "connected": true,
        }))
        .unwrap();
        assert_eq!(connected.connected, Some(true));

        let bonded_only = parse_done_command(&serde_json::json!({
            "address": ADDR, "cookie": "c", "ok": true, "connected": false,
        }))
        .unwrap();
        assert_eq!(bonded_only.connected, Some(false));

        // A helper too old to report it leaves the verdict unknown rather
        // than claiming the device did not connect.
        let silent = parse_done_command(&serde_json::json!({
            "address": ADDR, "cookie": "c", "ok": true,
        }))
        .unwrap();
        assert_eq!(silent.connected, None);

        // A failed pairing had nothing to connect, so a stray field on that
        // frame is ignored instead of putting a connect verdict on a failure.
        let failed = parse_done_command(&serde_json::json!({
            "address": ADDR, "cookie": "c", "ok": false, "connected": true,
        }))
        .unwrap();
        assert_eq!(failed.connected, None);
    }

    #[test]
    fn the_success_line_says_whether_the_device_is_actually_usable() {
        assert_eq!(
            paired_message("WH-1000XM4", Some(true)),
            "Connected WH-1000XM4"
        );
        assert_eq!(
            paired_message("WH-1000XM4", Some(false)),
            "Paired WH-1000XM4 \u{2014} not connected"
        );
        assert_eq!(paired_message("WH-1000XM4", None), "Paired WH-1000XM4");
    }

    #[test]
    fn response_payloads_carry_exactly_one_answer_shape() {
        let pin = response_payload("c", PairingAnswer::Pin("1234"));
        assert_eq!(pin["accepted"], true);
        assert_eq!(pin["pin"], "1234");
        assert!(pin.get("reason").is_none());

        let confirmed = response_payload("c", PairingAnswer::Confirmed);
        assert_eq!(confirmed["accepted"], true);
        assert!(confirmed.get("pin").is_none());

        let rejected = response_payload("c", PairingAnswer::Rejected);
        assert_eq!(rejected["accepted"], false);
        assert_eq!(rejected["reason"], "rejected");
        assert!(rejected.get("pin").is_none());

        let cancelled = response_payload("c", PairingAnswer::Cancelled);
        assert_eq!(cancelled["accepted"], false);
        assert_eq!(cancelled["reason"], "cancelled");
    }

    #[test]
    fn the_query_reports_liveness_and_phase() {
        assert_eq!(session_json(None), serde_json::json!({ "active": false }));

        let mut session = session(Instant::now());
        let json = session_json(Some(&session));
        assert_eq!(json["active"], true);
        assert_eq!(json["address"], ADDR);
        assert_eq!(json["cookie"], "0123456789abcdef");
        assert_eq!(json["state"], "working");

        session.apply_prompt(PairingPrompt::Confirm { passkey: 7 }, Instant::now());
        assert_eq!(session_json(Some(&session))["state"], "awaiting_confirm");
    }
}
