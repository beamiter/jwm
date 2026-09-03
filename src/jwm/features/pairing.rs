//! Bluetooth pairing session state for the control center.
//!
//! Pairing cannot be driven by `bluetoothctl` the way connect/disconnect is:
//! BlueZ wants an interactive agent answering PIN and passkey callbacks on the
//! system bus. That work lives in the one-shot helper `jwm-bridge pair
//! <address>`; this module is the compositor's side of the conversation — a
//! pure state machine plus the IPC wire mapping, so every rule (who may
//! prompt, what a PIN may look like, when a session is stale) is unit tested
//! without a bus, a helper, or a panel.
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
}

/// Where one pairing session stands. There is no persistent Done/Failed
/// state: the session record is dropped the moment `done` lands, and the
/// outcome lives on as the picker's status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingPhase {
    /// The helper is running and bluez has not asked anything (anymore).
    Working,
    AwaitingPin,
    AwaitingConfirm {
        passkey: u32,
    },
    Displaying {
        code: String,
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
        }
    }

    /// Whether the user is being asked something right now.
    #[must_use]
    pub fn is_prompting(&self) -> bool {
        !matches!(self, Self::Working)
    }
}

/// One live pairing session. At most one exists at a time.
#[derive(Debug, Clone)]
pub struct PairingSession {
    address: String,
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
            address: address.to_uppercase(),
            device_name: device_name.to_string(),
            cookie,
            phase: PairingPhase::Working,
            started: now,
            prompt_started: None,
        })
    }

    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
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
    /// The cookie is a shared secret and compares exactly; the address is
    /// case-folded because bluez and bluetoothctl disagree on casing.
    #[must_use]
    pub fn matches(&self, address: &str, cookie: &str) -> bool {
        self.address.eq_ignore_ascii_case(address) && self.cookie == cookie
    }

    /// bluez asked something: move into the matching prompt phase. A second
    /// prompt replaces the first — bluez serializes agent requests per Pair
    /// call, so a fresh prompt means the previous one was withdrawn.
    pub fn apply_prompt(&mut self, prompt: PairingPrompt, now: Instant) {
        self.phase = match prompt {
            PairingPrompt::Pin => PairingPhase::AwaitingPin,
            PairingPrompt::Confirm { passkey } => PairingPhase::AwaitingConfirm { passkey },
            PairingPrompt::Display { code } => PairingPhase::Displaying { code },
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

    /// Whether the whole session has outlived [`SESSION_TIMEOUT`].
    #[must_use]
    pub fn session_timed_out(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= SESSION_TIMEOUT
    }

    /// Time until the next deadline this session cares about, so the event
    /// loop can wake exactly when a timeout must be enforced. `None` when no
    /// session is running.
    #[must_use]
    pub fn next_timeout_in(&self, now: Instant) -> Option<Duration> {
        let session_deadline =
            SESSION_TIMEOUT.saturating_sub(now.saturating_duration_since(self.started));
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
}

/// A validated `bluetooth_pairing_done` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoneCommand {
    pub address: String,
    pub cookie: String,
    pub ok: bool,
    pub error: Option<String>,
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
        kind => return Err(format!("unknown prompt kind {kind:?}")),
    };
    Ok(PromptCommand {
        address: address_field(args)?,
        cookie: cookie_field(args)?,
        prompt,
    })
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
    })
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
            "address": session.address(),
            "cookie": session.cookie(),
            "state": session.phase().as_str(),
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
