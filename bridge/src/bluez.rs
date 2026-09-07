//! One-shot Bluetooth verbs on the system bus: `jwm-bridge pair <address>`
//! and `jwm-bridge discover [seconds]`.
//!
//! Both live here rather than in the compositor because both need the system
//! bus, and jwm is deliberately D-Bus-free — its event loop stays synchronous
//! and a wedged bus must not be able to stall a frame.
//!
//! **Pairing** needs an `org.bluez.Agent1` answering PIN and passkey
//! callbacks interactively — nothing the long-lived notification bridge does
//! either. This process is the narrow bridge for exactly one session:
//! register an agent with the strongest capability jwm's picker can honor
//! (`KeyboardDisplay`), call `Device1.Pair`, relay every callback to jwm's
//! picker over the fast IPC commands, and turn the user's answer — broadcast
//! back as a `bluetooth/pairing_response` event — into the agent reply. A
//! bond that lands is then trusted and connected, because "paired" is not
//! what the user wanted; "the headset plays" is.
//!
//! **Discovery** replaces scraping `bluetoothctl --timeout N scan on`. One
//! `GetManagedObjects` round trip answers Paired/Connected/Alias/RSSI for
//! every device at once, where the text path needed one `bluetoothctl info`
//! child per device. It prints a bounded JSON array and exits; BlueZ scopes a
//! discovery session to the calling connection, so process exit stops the
//! scan even if `StopDiscovery` never lands.
//!
//! Trust rules, all enforced here and in `jwm::features::pairing`:
//!
//! - The session is bound by a cookie jwm minted into this process's
//!   environment. Callbacks for any other device are rejected, inbound
//!   `RequestAuthorization`/`AuthorizeService` always are (this agent exists
//!   to pair one chosen device, not to bless whatever asks), and jwm-side
//!   answers for a foreign cookie are dropped.
//! - PINs and passkeys are relayed but never logged.
//! - Every wait is bounded: jwm withdraws a stale prompt after 25s, this
//!   process answers no callback later than 35s after it arrived, and the
//!   whole session has a 90s wall clock. Discovery has its own, shorter one.
//! - Everything printed for jwm to parse is bounded here as well as there:
//!   device names come from a remote peer and must never reach the panel at
//!   arbitrary length.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use zbus::names::BusName;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::{Connection, interface};

use crate::jwm_ipc::JwmIpc;

const BLUEZ: &str = "org.bluez";
const AGENT_MANAGER_IFACE: &str = "org.bluez.AgentManager1";
const DEVICE_IFACE: &str = "org.bluez.Device1";
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";

/// Where this process serves its agent. The path dies with the process, so
/// concurrent or successive helpers never fight over it.
pub const AGENT_PATH: &str = "/org/jwm/pairing_agent";

/// Where the inbound window serves its agent. A distinct path from
/// [`AGENT_PATH`] so a log line says which direction is running; bluez keys
/// agents by (sender, path), so two processes could share one path safely,
/// but nothing is gained by making the logs ambiguous.
pub const INBOUND_AGENT_PATH: &str = "/org/jwm/inbound_agent";

/// The strongest capability jwm's picker can honor: it can show a passkey and
/// read a typed PIN back.
pub const CAPABILITY: &str = "KeyboardDisplay";

/// The longest service identifier jwm's prompt will render
/// (`pairing::MAX_SERVICE_CHARS`). Anything longer is a malformed callback.
const MAX_SERVICE_CHARS: usize = 64;

/// How long an armed inbound window waits for something to ring. Shorter than
/// the pairing wall clock: it holds the controller pairable and discoverable
/// for its whole life, and jwm drops its own record at 60s.
const INBOUND_WINDOW: Duration = Duration::from_secs(60);

/// jwm withdraws an unanswered prompt after 25s; answer no callback later
/// than this so jwm's own cancel reliably lands first.
const PROMPT_WAIT: Duration = Duration::from_secs(35);
/// Absolute bound on one pairing session.
const WALL_CLOCK: Duration = Duration::from_secs(90);
/// How long the post-pair `Connect` may take when the whole wall clock is
/// still available. A profile negotiation that has not finished by then is
/// reported as "paired but not connected" rather than held onto: the bond is
/// what the user asked for and it is already durable.
const CONNECT_WAIT: Duration = Duration::from_secs(15);
/// Room reserved at the end of the wall clock for the `done` report, so a
/// long `Connect` can never eat the frame that tells jwm what happened.
const REPORT_SLACK: Duration = Duration::from_secs(3);
/// Below this there is no point starting a `Connect` at all.
const MIN_CONNECT_BUDGET: Duration = Duration::from_secs(1);
/// Pairing traffic is a handful of frames; a small queue is plenty.
const EVENT_QUEUE_CAPACITY: usize = 64;

/// Paired successfully.
pub const EXIT_PAIRED: i32 = 0;
/// Pairing was attempted and failed (device error, bluez error).
pub const EXIT_FAILED: i32 = 1;
/// The user (or jwm, on their behalf) cancelled, or jwm's session was already
/// gone when the helper started.
pub const EXIT_CANCELLED: i32 = 2;
/// Infrastructure failure: no system bus, jwm unreachable, wall clock hit.
pub const EXIT_ERROR: i32 = 3;
/// Bad invocation (usage).
pub const EXIT_USAGE: i32 = 64;
/// The verb did what it said. Shares its value with [`EXIT_PAIRED`]; the
/// pairing name is kept because the picker reads pairing exits by name.
pub const EXIT_OK: i32 = 0;

/// Longest discovery window a caller may ask for. Discovery keeps the radio
/// busy and BlueZ's own `DiscoverableTimeout` is measured in minutes, so a
/// picker refresh has no business running longer than this.
const DISCOVERY_MAX_SECONDS: u64 = 30;
/// What `jwm-bridge discover` scans for when no window is given — the same
/// budget the `bluetoothctl` path used, minus the process-startup slack it
/// needed.
pub const DISCOVERY_DEFAULT_SECONDS: u64 = 10;
/// Bound on one bus round trip. A wedged bluetoothd must not hold the worker
/// thread jwm is waiting on.
const DISCOVERY_CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// As many devices as jwm's picker will keep (`MAX_BLUETOOTH_DEVICES`).
/// Bounded on this side too: a crowded room is exactly when the list is
/// longest, and stdout is the wire.
const MAX_DISCOVERED_DEVICES: usize = 64;
/// As many name characters as jwm's picker will keep
/// (`MAX_BLUETOOTH_DEVICE_NAME_CHARS`). The name is remote-controlled input.
const MAX_DEVICE_NAME_CHARS: usize = 248;

/// Agent replies bluez understands. zbus's stock `Error` collapses to
/// `org.freedesktop.DBus.Error.Failed` at dispatch; deriving `DBusError` keeps
/// the bluez names intact on the wire.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    /// The user refused the request (`n` on a numeric comparison), or the
    /// callback did not belong to this session's device.
    Rejected(String),
    /// The prompt was withdrawn: Esc, panel closed, timeout, or jwm gone.
    Canceled(String),
}

/// What bluez asked, mirrored onto the `bluetooth_pairing_prompt` command.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PromptRequest {
    /// `RequestPinCode`/`RequestPasskey`: the user types the code.
    Pin,
    /// `RequestConfirmation`: numeric comparison.
    Confirm { passkey: u32 },
    /// `DisplayPinCode`/`DisplayPasskey`: shown, typed on the device.
    Display { code: String },
    /// `RequestAuthorization` (`None`) or `AuthorizeService` (the UUID):
    /// something out there is asking, and only the user can answer.
    Authorize { service: Option<String> },
}

/// The user's answer, as parsed from a `bluetooth/pairing_response` event.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UserReply {
    Pin(String),
    Confirmed,
    Rejected,
    /// One prompt withdrawn; the session lives on.
    Cancelled,
    /// The session itself is over. An inbound window puts the controller
    /// back here rather than waiting out its own clock.
    SessionClosed,
}

impl UserReply {
    /// Whether this answer ends the pairing from the user's side — used to
    /// classify the session's exit code after `Pair` unwinds.
    fn is_user_termination(&self) -> bool {
        matches!(self, Self::Rejected | Self::Cancelled | Self::SessionClosed)
    }

    /// Whether jwm has dropped the session, as opposed to withdrawing one
    /// prompt from a session that is still running.
    fn ends_the_session(&self) -> bool {
        matches!(self, Self::SessionClosed)
    }
}

/// State shared between the agent callbacks and the response pump.
struct Shared {
    /// Target device address, uppercase; callbacks for anything else are
    /// rejected before they can touch the session.
    ///
    /// An outbound session knows it before the process starts. An inbound
    /// window cannot — nobody can say in advance which device will ring — so
    /// it starts empty and the first callback pins it. From that moment the
    /// rule is identical: one session, one device.
    target: Mutex<Option<String>>,
    cookie: String,
    /// Whether this process answers inbound authorization at all. False for
    /// `pair`, whose agent exists to bond one chosen device and must not be
    /// talked into blessing whatever else asks while it holds the default
    /// agent registration.
    accepts_inbound: bool,
    /// The one request bluez has outstanding, tagged with the id jwm was
    /// told about. BlueZ serializes agent requests per `Pair` call; a
    /// replacement means the first was withdrawn.
    ///
    /// The id is what stops an answer landing on the wrong question. A
    /// cookie identifies the session, not the request, so without it a `yes`
    /// the user gave to one prompt could resolve whichever request happened
    /// to be pending by the time the broadcast crossed the socket, the mpsc
    /// queue and the scheduler — granting, say, an input-device profile on
    /// the strength of a yes to an audio one, with the real question never
    /// drawn.
    pending: Mutex<Option<(u64, oneshot::Sender<UserReply>)>>,
    /// Source of request ids. Monotonic within one session; jwm echoes it
    /// back and anything else is dropped.
    next_request_id: std::sync::atomic::AtomicU64,
    /// Set when a user-side rejection/cancel was seen, so the exit code says
    /// "cancelled" rather than reporting bluez's resulting error as failure.
    ended_by_user: AtomicBool,
    /// Set when the user actually granted an inbound request (a `Confirmed`
    /// or `Pin` reply resolved a pending one). An inbound window's `done`
    /// report is `ok` only when this is true: closing the window is *not* a
    /// refusal, so `!ended_by_user` — which `SessionClosed` also flips —
    /// would report every allowed bond as "refused".
    authorized: AtomicBool,
    ipc: JwmIpc,
}

impl Shared {
    fn lock_pending(&self) -> std::sync::MutexGuard<'_, Option<(u64, oneshot::Sender<UserReply>)>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Take the outstanding request only when `id` names it. `None` when
    /// nothing is pending or the answer is for a request already gone.
    fn take_pending(&self, id: u64) -> Option<oneshot::Sender<UserReply>> {
        let mut pending = self.lock_pending();
        match pending.as_ref() {
            Some((pending_id, _)) if *pending_id == id => pending.take().map(|(_, reply)| reply),
            _ => None,
        }
    }

    fn mint_request_id(&self) -> u64 {
        self.next_request_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn lock_target(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The device this session has bound to, if it has bound to one.
    fn target(&self) -> Option<String> {
        self.lock_target().clone()
    }

    /// Accept a callback's device: equal to the bound one, or — for an
    /// inbound window that nothing has called into yet — the one that binds
    /// it. Late-bound, never absent.
    fn bind_target(&self, address: &str) -> bool {
        let mut target = self.lock_target();
        match target.as_deref() {
            Some(bound) => bound == address,
            None => {
                *target = Some(address.to_string());
                true
            }
        }
    }
}

struct PairingAgent {
    shared: Arc<Shared>,
    /// Only an inbound window needs this: it has to ask bluez what the
    /// device that just rang is called, because nothing told it in advance.
    /// `None` for an outbound session, whose name came off the picker row.
    name_lookup: Option<Connection>,
}

impl PairingAgent {
    /// Whether a callback's object path names this session's device, binding
    /// it on the first inbound callback. A path that is not a device address
    /// at all never matches.
    fn device_matches(&self, device: &OwnedObjectPath) -> bool {
        address_from_device_path(device.as_str())
            .is_some_and(|address| self.shared.bind_target(&address))
    }

    /// What to call the device on jwm's prompt. Empty unless this is an
    /// inbound window, where the panel would otherwise have to ask the user
    /// to make a security decision about a bare MAC address.
    async fn device_label(&self, address: &str) -> String {
        let Some(connection) = self.name_lookup.as_ref() else {
            return String::new();
        };
        let Some(objects) = managed_objects(connection).await else {
            return String::new();
        };
        device_name_from_managed_objects(&objects, address).unwrap_or_default()
    }

    /// Relay a display-only callback to jwm's picker. Nothing comes back; a
    /// cancel while this is showing arrives as a response event with no
    /// pending request and turns into `CancelPairing` in the pump.
    async fn tell_jwm(&self, prompt: PromptRequest) {
        let Some(target) = self.shared.target() else {
            return;
        };
        let label = self.device_label(&target).await;
        // Display-only prompts expect no answer, so their id can never be
        // matched by one; minting it anyway keeps the wire shape uniform.
        let args = prompt_args(
            &target,
            &self.shared.cookie,
            &label,
            self.shared.mint_request_id(),
            &prompt,
        );
        let ipc = self.shared.ipc.clone();
        let sent =
            tokio::task::spawn_blocking(move || ipc.command("bluetooth_pairing_prompt", args))
                .await;
        match sent {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => log::warn!("jwm rejected the display prompt: {error}"),
            Err(error) => log::warn!("could not forward the display prompt: {error}"),
        }
    }

    /// Answer an inbound request — a device wanting to bond
    /// (`RequestAuthorization`) or a bonded device wanting a profile
    /// (`AuthorizeService`).
    ///
    /// Refused outright unless this process was started as an inbound window
    /// the user armed. A `pair` session holds the default agent registration
    /// for up to 90 seconds precisely so nothing else answers its callbacks;
    /// letting it also bless whatever rings during that window would turn one
    /// chosen device into an open door.
    async fn authorize(
        &self,
        device: OwnedObjectPath,
        uuid: Option<String>,
    ) -> Result<(), AgentError> {
        if !self.shared.accepts_inbound {
            log::warn!(
                "rejecting inbound authorization for {} (this session pairs one chosen device)",
                device.as_str()
            );
            return Err(AgentError::Rejected(
                "this agent only pairs the device it was started for".to_string(),
            ));
        }
        // A UUID longer than jwm will render is a malformed callback, not a
        // string to truncate into something that reads like a real profile.
        if uuid
            .as_ref()
            .is_some_and(|uuid| uuid.is_empty() || uuid.chars().count() > MAX_SERVICE_CHARS)
        {
            log::warn!("rejecting a service authorization with an unusable UUID");
            return Err(AgentError::Rejected(
                "unusable service identifier".to_string(),
            ));
        }
        match self
            .ask(&device, PromptRequest::Authorize { service: uuid })
            .await?
        {
            UserReply::Confirmed => Ok(()),
            UserReply::Pin(_) => Err(AgentError::Rejected(
                "jwm answered an authorization with a PIN; refusing".to_string(),
            )),
            UserReply::Rejected => Err(AgentError::Rejected("refused by the user".to_string())),
            UserReply::Cancelled | UserReply::SessionClosed => Err(AgentError::Canceled(
                "authorization prompt cancelled".to_string(),
            )),
        }
    }

    /// One request/response round trip through jwm's picker: park the reply
    /// channel, show the prompt, wait (bounded) for the user's answer.
    async fn ask(
        &self,
        device: &OwnedObjectPath,
        prompt: PromptRequest,
    ) -> Result<UserReply, AgentError> {
        if !self.device_matches(device) {
            log::warn!("agent callback for a foreign device path; rejecting");
            return Err(AgentError::Rejected(
                "not the device this session pairs".to_string(),
            ));
        }
        let target = self.shared.target().unwrap_or_default();
        let label = self.device_label(&target).await;
        let (tx, rx) = oneshot::channel();
        let request_id = self.shared.mint_request_id();
        *self.shared.lock_pending() = Some((request_id, tx));

        let args = prompt_args(&target, &self.shared.cookie, &label, request_id, &prompt);
        let ipc = self.shared.ipc.clone();
        let sent =
            tokio::task::spawn_blocking(move || ipc.command("bluetooth_pairing_prompt", args))
                .await;
        match sent {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                let _ = self.shared.take_pending(request_id);
                return Err(AgentError::Canceled(format!(
                    "jwm did not accept the pairing prompt: {error}"
                )));
            }
            Err(error) => {
                let _ = self.shared.take_pending(request_id);
                return Err(AgentError::Canceled(format!(
                    "could not reach jwm: {error}"
                )));
            }
        }

        match tokio::time::timeout(PROMPT_WAIT, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            // Sender dropped without an answer: bluez cancelled the request.
            Ok(Err(_)) => Err(AgentError::Canceled("the prompt was withdrawn".to_string())),
            Err(_) => {
                let _ = self.shared.take_pending(request_id);
                Err(AgentError::Canceled(
                    "the pairing prompt went unanswered".to_string(),
                ))
            }
        }
    }
}

#[interface(name = "org.bluez.Agent1")]
impl PairingAgent {
    fn release(&self) {
        log::info!("pairing agent released by bluez");
    }

    async fn request_pin_code(&self, device: OwnedObjectPath) -> Result<String, AgentError> {
        match self.ask(&device, PromptRequest::Pin).await? {
            // The PIN is returned to bluez and never logged here.
            UserReply::Pin(pin) => Ok(pin),
            UserReply::Confirmed => Err(AgentError::Rejected(
                "jwm confirmed a PIN request; refusing to guess".to_string(),
            )),
            UserReply::Rejected => Err(AgentError::Rejected("PIN refused by the user".to_string())),
            UserReply::Cancelled | UserReply::SessionClosed => {
                Err(AgentError::Canceled("PIN prompt cancelled".to_string()))
            }
        }
    }

    async fn display_pin_code(&self, device: OwnedObjectPath, pincode: String) {
        if self.device_matches(&device) {
            self.tell_jwm(PromptRequest::Display { code: pincode })
                .await;
        }
    }

    async fn request_passkey(&self, device: OwnedObjectPath) -> Result<u32, AgentError> {
        match self.ask(&device, PromptRequest::Pin).await? {
            UserReply::Pin(pin) => pin
                .parse::<u32>()
                .ok()
                .filter(|passkey| *passkey <= 999_999)
                .ok_or_else(|| {
                    AgentError::Rejected("passkey must be numeric, 0-999999".to_string())
                }),
            UserReply::Confirmed => Err(AgentError::Rejected(
                "jwm confirmed a passkey request; refusing to guess".to_string(),
            )),
            UserReply::Rejected => Err(AgentError::Rejected(
                "passkey refused by the user".to_string(),
            )),
            UserReply::Cancelled | UserReply::SessionClosed => {
                Err(AgentError::Canceled("passkey prompt cancelled".to_string()))
            }
        }
    }

    async fn display_passkey(&self, device: OwnedObjectPath, passkey: u32, _entered: u16) {
        if self.device_matches(&device) {
            self.tell_jwm(PromptRequest::Display {
                code: format!("{passkey:06}"),
            })
            .await;
        }
    }

    async fn request_confirmation(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
    ) -> Result<(), AgentError> {
        match self
            .ask(&device, PromptRequest::Confirm { passkey })
            .await?
        {
            UserReply::Confirmed => Ok(()),
            UserReply::Pin(_) => Err(AgentError::Rejected(
                "jwm answered a confirmation with a PIN; refusing".to_string(),
            )),
            UserReply::Rejected => Err(AgentError::Rejected(
                "passkeys did not match, says the user".to_string(),
            )),
            UserReply::Cancelled | UserReply::SessionClosed => Err(AgentError::Canceled(
                "confirmation prompt cancelled".to_string(),
            )),
        }
    }

    async fn request_authorization(&self, device: OwnedObjectPath) -> Result<(), AgentError> {
        self.authorize(device, None).await
    }

    async fn authorize_service(
        &self,
        device: OwnedObjectPath,
        uuid: String,
    ) -> Result<(), AgentError> {
        self.authorize(device, Some(uuid)).await
    }

    async fn cancel(&self, device: OwnedObjectPath) {
        if !self.device_matches(&device) {
            return;
        }
        // BlueZ withdrew the outstanding request: the user can no longer
        // answer it, so the pending callback resolves as cancelled and the
        // Pair call unwinds. Whatever is pending is what bluez withdrew, so
        // this takes it by identity rather than by id.
        let pending = self.shared.lock_pending().take();
        let request_id = pending.as_ref().map(|(id, _)| *id);
        // The prompt the cancelled request raised stays on jwm's panel until
        // told otherwise — resolving the callback does not reach the picker.
        // Report the withdrawal even when nothing was pending (a display-only
        // prompt carries no reply channel, and a spurious cancel is possible
        // too): jwm no-ops when it is showing none. Awaited like every other
        // send, so the frame cannot be cut off by the Pair unwind racing the
        // process to its exit, and so it lands before any request bluez
        // raises next. It is also reported BEFORE the callback resolves: the
        // unwind that resolution sets off ends in `report_done`, and the two
        // frames must land in this order — withdraw first, outcome second —
        // or the outcome would be processed against a panel still showing
        // the stale prompt. The device is bound by `device_matches` above,
        // so a missing target is not a real arm.
        if let Some(target) = self.shared.target() {
            report_withdraw(&self.shared.ipc, &target, &self.shared.cookie, request_id).await;
        }
        if let Some((_, reply)) = pending {
            let _ = reply.send(UserReply::Cancelled);
        }
    }
}

// ---------------------------------------------------------------------------
// Pure wire mapping (unit-tested without a bus)
// ---------------------------------------------------------------------------

/// `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF` → `AA:BB:CC:DD:EE:FF`.
fn address_from_device_path(path: &str) -> Option<String> {
    let leaf = path.rsplit('/').next()?;
    let dev = leaf.strip_prefix("dev_")?;
    let address = dev.replace('_', ":").to_uppercase();
    is_valid_address(&address).then_some(address)
}

fn is_valid_address(address: &str) -> bool {
    let mut count = 0;
    for octet in address.split(':') {
        if octet.len() != 2 || !octet.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return false;
        }
        count += 1;
    }
    count == 6
}

/// Pick the `org.bluez.Device1` object whose `Address` property names the
/// target — the only reliable way to learn which controller path hosts the
/// device when the machine has more than `hci0`.
fn device_path_from_managed_objects(
    objects: &zbus::fdo::ManagedObjects,
    address: &str,
) -> Option<OwnedObjectPath> {
    let iface = zbus::names::InterfaceName::try_from(DEVICE_IFACE).ok()?;
    objects.iter().find_map(|(path, interfaces)| {
        let properties = interfaces.get(&iface)?;
        let device_address = properties
            .get("Address")
            .and_then(|value| String::try_from(value.clone()).ok())?;
        device_address
            .eq_ignore_ascii_case(address)
            .then(|| path.clone())
    })
}

/// The name bluez knows the device at `address` by, sanitized for the panel.
///
/// Looked up straight from the `ManagedObjects` map by address, *not* through
/// [`devices_from_managed_objects`], whose sort-and-cap-to-64 exists for the
/// picker and would drop the very device that just rang an inbound window —
/// an unpaired device with no RSSI sorts into the tail — leaving the prompt
/// to name a bare MAC in a crowded room. `None` when bluez has no better
/// name than the address itself.
fn device_name_from_managed_objects(
    objects: &zbus::fdo::ManagedObjects,
    address: &str,
) -> Option<String> {
    let iface = zbus::names::InterfaceName::try_from(DEVICE_IFACE).ok()?;
    objects.iter().find_map(|(_, interfaces)| {
        let properties = interfaces.get(&iface)?;
        let device_address = managed_string(properties, "Address")?;
        if !device_address.eq_ignore_ascii_case(address) {
            return None;
        }
        managed_string(properties, "Alias")
            .or_else(|| managed_string(properties, "Name"))
            .map(|name| {
                name.trim()
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .take(MAX_DEVICE_NAME_CHARS)
                    .collect::<String>()
                    .trim()
                    .to_string()
            })
            // The address is not a name worth sending: jwm falls back to it.
            .filter(|name| !name.is_empty() && !name.eq_ignore_ascii_case(address))
    })
}

/// Build the `bluetooth_pairing_prompt` command arguments. The passkey/code
/// a prompt carries is display material, never a secret to keep off the wire
/// — but it is never logged either.
fn prompt_args(
    address: &str,
    cookie: &str,
    device_name: &str,
    request_id: u64,
    prompt: &PromptRequest,
) -> Value {
    let mut args = match prompt {
        PromptRequest::Pin => serde_json::json!({ "kind": "pin" }),
        PromptRequest::Confirm { passkey } => serde_json::json!({
            "kind": "confirm",
            "passkey": passkey,
        }),
        PromptRequest::Display { code } => serde_json::json!({
            "kind": "display",
            "code": code,
        }),
        PromptRequest::Authorize { service } => serde_json::json!({
            "kind": "authorize",
            // Absent rather than null for a bond request: jwm reads the
            // field's presence, not its value.
            "service": service,
        }),
    };
    let object = args.as_object_mut().expect("prompt args are an object");
    object.insert("address".to_string(), Value::from(address));
    object.insert("cookie".to_string(), Value::from(cookie));
    object.insert("request_id".to_string(), Value::from(request_id));
    if !device_name.is_empty() {
        object.insert("device_name".to_string(), Value::from(device_name));
    }
    args
}

/// Build the `bluetooth_pairing_done` command arguments.
///
/// `connected` is the post-pair auto-connect verdict and is meaningful only
/// when `ok`: `Some(true)` the device is in use now, `Some(false)` the bond
/// exists but the profile connection did not come up, `None` no connect was
/// attempted (the pairing failed, so there was nothing to connect).
fn done_args(
    address: &str,
    cookie: &str,
    ok: bool,
    error: Option<&str>,
    connected: Option<bool>,
) -> Value {
    serde_json::json!({
        "address": address,
        "cookie": cookie,
        "ok": ok,
        "error": error,
        "connected": connected,
    })
}

/// Build the `bluetooth_pairing_withdraw` command arguments.
///
/// BlueZ cancelling the outstanding agent request resolves the helper's side
/// of it, but the prompt that request raised stays on jwm's panel until it is
/// told — the resolved callback never reaches the picker. `request_id` names
/// the cancelled request when one was pending; a cancel can also arrive with
/// nothing outstanding (a display-only prompt carries no reply channel, or
/// jwm's own timeout answered first), and then the field is null — like
/// `done`'s nulls — and jwm drops whatever prompt it is showing, the same
/// by-identity reading the cancel itself uses.
fn withdraw_args(address: &str, cookie: &str, request_id: Option<u64>) -> Value {
    serde_json::json!({
        "address": address,
        "cookie": cookie,
        "request_id": request_id,
    })
}

/// How long the post-pair `Connect` may run, given how much of the session
/// wall clock the pairing itself already spent.
///
/// The connect is a convenience on top of a bond that is already durable, so
/// it borrows leftover time instead of extending the session: jwm drops its
/// own session record shortly after this process's wall clock, and a `done`
/// report that arrives after that is refused and the picker is left saying
/// nothing happened. `None` means "no room left, skip it".
fn connect_budget(elapsed: Duration) -> Option<Duration> {
    let remaining = WALL_CLOCK
        .saturating_sub(elapsed)
        .saturating_sub(REPORT_SLACK);
    let budget = remaining.min(CONNECT_WAIT);
    (budget >= MIN_CONNECT_BUDGET).then_some(budget)
}

/// One `bluetooth/pairing_response` event from jwm.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResponseEvent {
    cookie: String,
    /// Which request this answers. Absent from a bare cancel jwm sends with
    /// no prompt on screen — that one is not answering anything.
    request_id: Option<u64>,
    reply: UserReply,
}

/// Parse an event frame; anything that is not a well-formed pairing response
/// is `None` and ignored. A malformed PIN is not a reply at all.
fn parse_response_event(event: &Value) -> Option<ResponseEvent> {
    if event.get("event")?.as_str()? != "bluetooth/pairing_response" {
        return None;
    }
    let payload = event.get("payload")?;
    let cookie = payload.get("cookie")?.as_str()?.to_string();
    let request_id = payload.get("request_id").and_then(Value::as_u64);
    let accepted = payload.get("accepted")?.as_bool()?;
    let reply = if accepted {
        match payload.get("pin").and_then(Value::as_str) {
            Some(pin) if !pin.is_empty() => UserReply::Pin(pin.to_string()),
            Some(_) => return None,
            None => UserReply::Confirmed,
        }
    } else {
        match payload.get("reason").and_then(Value::as_str) {
            Some("rejected") => UserReply::Rejected,
            Some("closed") => UserReply::SessionClosed,
            _ => UserReply::Cancelled,
        }
    };
    Some(ResponseEvent {
        cookie,
        request_id,
        reply,
    })
}

/// The `get_bluetooth_pairing` self-heal: whether jwm still holds *this*
/// session. A helper whose session is already gone exits without touching
/// bluez.
fn session_matches(value: &Value, address: &str, cookie: &str) -> bool {
    value.get("active").and_then(Value::as_bool) == Some(true)
        && value
            .get("address")
            .and_then(Value::as_str)
            .is_some_and(|other| other.eq_ignore_ascii_case(address))
        && value.get("cookie").and_then(Value::as_str) == Some(cookie)
}

/// The same self-heal for an inbound window, which has no address yet.
///
/// The direction is checked as well as the cookie: a helper must never serve
/// an inbound agent against a session jwm opened to pair one chosen device,
/// which is exactly the confusion that would turn the narrow window into a
/// standing one.
fn inbound_session_matches(value: &Value, cookie: &str) -> bool {
    value.get("active").and_then(Value::as_bool) == Some(true)
        && value.get("cookie").and_then(Value::as_str) == Some(cookie)
        && value.get("kind").and_then(Value::as_str) == Some("inbound")
}

// ---------------------------------------------------------------------------
// Session driver
// ---------------------------------------------------------------------------

/// Tell jwm bluez withdrew the outstanding request, so the panel takes the
/// prompt down instead of waiting out its own timeout. Best effort like
/// [`report_done`]: a jwm that is gone or rejects the frame still has its
/// prompt timeout as the backstop, so a failure here is worth a log line,
/// never a failed session.
async fn report_withdraw(ipc: &JwmIpc, address: &str, cookie: &str, request_id: Option<u64>) {
    let args = withdraw_args(address, cookie, request_id);
    let ipc = ipc.clone();
    let sent =
        tokio::task::spawn_blocking(move || ipc.command("bluetooth_pairing_withdraw", args)).await;
    match sent {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => log::warn!("jwm rejected the withdraw report: {error}"),
        Err(error) => log::warn!("could not report the withdrawn prompt: {error}"),
    }
}

/// Report the terminal outcome to jwm. Best effort by definition: when jwm
/// is gone there is nothing left to inform.
async fn report_done(
    ipc: &JwmIpc,
    address: &str,
    cookie: &str,
    ok: bool,
    error: Option<&str>,
    connected: Option<bool>,
) {
    let args = done_args(address, cookie, ok, error, connected);
    let ipc = ipc.clone();
    let sent =
        tokio::task::spawn_blocking(move || ipc.command("bluetooth_pairing_done", args)).await;
    match sent {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => log::warn!("jwm rejected the pairing report: {error}"),
        Err(error) => log::warn!("could not report the pairing outcome: {error}"),
    }
}

/// Turn jwm's answer broadcasts into agent replies.
///
/// With a request outstanding the reply resolves it. With none — a display
/// prompt on screen, or `Pair` still running after the last answer — a
/// rejection/cancel means "stop pairing" and is relayed as `CancelPairing`;
/// a late accept means nothing and is dropped.
async fn pump_responses(
    connection: Connection,
    device_path: OwnedObjectPath,
    shared: Arc<Shared>,
    mut responses: mpsc::Receiver<Value>,
) {
    while let Some(event) = responses.recv().await {
        let Some(response) = parse_response_event(&event) else {
            continue;
        };
        if response.cookie != shared.cookie {
            log::warn!("ignoring a pairing response for a foreign cookie");
            continue;
        }
        if response.reply.is_user_termination() {
            shared.ended_by_user.store(true, Ordering::Relaxed);
        }
        // Only the request jwm was actually answering. A stale answer — one
        // whose request bluez already withdrew — resolves nothing, so it
        // cannot be applied to whatever took its place.
        let pending = response.request_id.and_then(|id| shared.take_pending(id));
        match pending {
            Some(reply) => {
                let _ = reply.send(response.reply);
            }
            None => {
                if response.reply.is_user_termination() {
                    log::info!("cancel with no matching request: CancelPairing");
                    let result = connection
                        .call_method(
                            Some(bluez_name()),
                            &device_path,
                            Some(DEVICE_IFACE),
                            "CancelPairing",
                            &(),
                        )
                        .await;
                    if let Err(error) = result {
                        log::debug!("CancelPairing failed (pairing likely already over): {error}");
                    }
                }
            }
        }
    }
}

/// Find the object path hosting the target device.
async fn find_device_path(connection: &Connection, address: &str) -> Option<OwnedObjectPath> {
    let proxy = zbus::fdo::ObjectManagerProxy::builder(connection)
        .destination(BLUEZ)
        .ok()
        .and_then(|builder| builder.path("/").ok())?
        .build()
        .await
        .map_err(|error| log::warn!("pair: cannot build an ObjectManager proxy: {error}"))
        .ok()?;
    let objects = proxy
        .get_managed_objects()
        .await
        .map_err(|error| log::warn!("pair: GetManagedObjects failed: {error}"))
        .ok()?;
    let found = device_path_from_managed_objects(&objects, address);
    if found.is_none() {
        log::warn!(
            "pair: {address} is not among the {} managed objects",
            objects.len()
        );
    }
    found
}

fn agent_path() -> ObjectPath<'static> {
    ObjectPath::from_static_str(AGENT_PATH).expect("AGENT_PATH is a valid object path")
}

fn bluez_name() -> BusName<'static> {
    BusName::from_static_str(BLUEZ).expect("org.bluez is a valid bus name")
}

/// One `org.bluez.AgentManager1` call at `/org/bluez` with an `(os)` body.
async fn agent_manager_call(
    connection: &Connection,
    method: &str,
    body: &(ObjectPath<'static>, &str),
) -> zbus::Result<()> {
    connection
        .call_method(
            Some(bluez_name()),
            "/org/bluez",
            Some(AGENT_MANAGER_IFACE),
            method,
            body,
        )
        .await
        .map(|_| ())
}

/// `RequestDefaultAgent`/`UnregisterAgent` carry a lone object path `(o)`.
async fn agent_manager_call_path(
    connection: &Connection,
    method: &str,
    path: ObjectPath<'static>,
) -> zbus::Result<()> {
    connection
        .call_method(
            Some(bluez_name()),
            "/org/bluez",
            Some(AGENT_MANAGER_IFACE),
            method,
            &path,
        )
        .await
        .map(|_| ())
}

/// Mark a freshly bonded device trusted.
///
/// Without this every later service connection from the device raises an
/// `AuthorizeService` callback, and with no agent registered bluez answers it
/// by refusing — a keyboard that pairs fine and then never types. The user
/// chose this device explicitly, so the bond carries the authorization.
/// Best effort: a controller that refuses the property still leaves a usable
/// bond, and the failure is worth a log line, not a failed pairing.
async fn mark_trusted(connection: &Connection, device_path: &OwnedObjectPath) {
    let result = connection
        .call_method(
            Some(bluez_name()),
            device_path,
            Some(PROPERTIES_IFACE),
            "Set",
            &(DEVICE_IFACE, "Trusted", zbus::zvariant::Value::Bool(true)),
        )
        .await;
    if let Err(error) = result {
        log::warn!("could not mark the device trusted: {error}");
    }
}

/// Connect the profiles of a device that just finished bonding.
///
/// Pairing establishes the bond; it does not put the headset in your ears.
/// BlueZ connects some devices on its own and leaves others waiting for an
/// explicit `Connect`, so the picker used to need a second `Enter` for half
/// the devices in existence. Bounded by `budget`: a device that wandered out
/// of range must not hold the session open.
async fn connect_device(
    connection: &Connection,
    device_path: &OwnedObjectPath,
    budget: Duration,
) -> Result<(), String> {
    let call = connection.call_method(
        Some(bluez_name()),
        device_path,
        Some(DEVICE_IFACE),
        "Connect",
        &(),
    );
    match tokio::time::timeout(budget, call).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("{error}")),
        Err(_) => Err(format!("the device did not connect within {budget:?}")),
    }
}

/// Trust and connect a device whose `Pair` just returned successfully.
/// Returns whether the profile connection came up; the bond stands either
/// way, so nothing here can turn a successful pairing into a failure.
async fn settle_paired_device(
    connection: &Connection,
    device_path: &OwnedObjectPath,
    elapsed: Duration,
) -> bool {
    mark_trusted(connection, device_path).await;
    let Some(budget) = connect_budget(elapsed) else {
        log::info!("paired, but no wall-clock budget left to connect the device");
        return false;
    };
    match connect_device(connection, device_path, budget).await {
        Ok(()) => true,
        Err(error) => {
            log::warn!("paired, but connecting the device failed: {error}");
            false
        }
    }
}

/// Drive one pairing session to its end: register the agent, pair, relay
/// callbacks both ways, report `done`, unregister. Returns the exit code.
pub async fn pair_session(ipc: JwmIpc, connection: Connection, address: &str, cookie: &str) -> i32 {
    let target = address.to_uppercase();
    // The post-pair connect borrows what is left of the wall clock rather
    // than extending it; this is where that clock starts.
    let session_started = std::time::Instant::now();

    // Self-heal before touching bluez: jwm may have cancelled between
    // spawning this process and now.
    let liveness = {
        let ipc = ipc.clone();
        tokio::task::spawn_blocking(move || ipc.query("get_bluetooth_pairing")).await
    };
    match liveness {
        Ok(Ok(value)) if session_matches(&value, &target, cookie) => {}
        Ok(Ok(_)) => {
            log::info!("pair {target}: jwm no longer holds this session; exiting");
            return EXIT_CANCELLED;
        }
        Ok(Err(error)) => {
            log::warn!("pair {target}: jwm refused the session query: {error}");
            return EXIT_ERROR;
        }
        Err(error) => {
            log::warn!("pair {target}: could not query jwm: {error}");
            return EXIT_ERROR;
        }
    }

    let (tx, responses) = mpsc::channel(EVENT_QUEUE_CAPACITY);
    crate::jwm_ipc::subscribe(ipc.clone(), &["bluetooth"], tx);

    let Some(device_path) = find_device_path(&connection, &target).await else {
        log::warn!("pair {target}: no bluez device with that address");
        report_done(&ipc, &target, cookie, false, Some("device not found"), None).await;
        return EXIT_FAILED;
    };

    let shared = Arc::new(Shared {
        target: Mutex::new(Some(target.clone())),
        cookie: cookie.to_string(),
        // This agent exists to bond one chosen device; it holds the default
        // registration for the session's lifetime and must not answer for
        // anything else that rings while it does.
        accepts_inbound: false,
        pending: Mutex::new(None),
        next_request_id: std::sync::atomic::AtomicU64::new(1),
        ended_by_user: AtomicBool::new(false),
        authorized: AtomicBool::new(false),
        ipc: ipc.clone(),
    });
    if let Err(error) = connection
        .object_server()
        .at(
            AGENT_PATH,
            PairingAgent {
                shared: shared.clone(),
                // jwm already knows this device's name: it came off the
                // picker row the user pressed Enter on.
                name_lookup: None,
            },
        )
        .await
    {
        log::warn!("pair {target}: could not serve the agent: {error}");
        report_done(
            &ipc,
            &target,
            cookie,
            false,
            Some("could not register the pairing agent"),
            None,
        )
        .await;
        return EXIT_ERROR;
    }

    let agent_path = agent_path();
    let registered = agent_manager_call(
        &connection,
        "RegisterAgent",
        &(agent_path.clone(), CAPABILITY),
    )
    .await;
    if let Err(error) = registered {
        log::warn!("pair {target}: RegisterAgent failed: {error}");
        report_done(
            &ipc,
            &target,
            cookie,
            false,
            Some("bluez refused the pairing agent"),
            None,
        )
        .await;
        return EXIT_FAILED;
    }
    // Make this agent the default for the session's lifetime: `Pair` asks the
    // default agent, and without this a stray desktop agent would answer in
    // our place. The registration dies with this process either way.
    let _ = agent_manager_call_path(&connection, "RequestDefaultAgent", agent_path.clone()).await;

    let pump = tokio::spawn(pump_responses(
        connection.clone(),
        device_path.clone(),
        shared.clone(),
        responses,
    ));
    let outcome = connection
        .call_method(
            Some(bluez_name()),
            &device_path,
            Some(DEVICE_IFACE),
            "Pair",
            &(),
        )
        .await;
    pump.abort();

    // Polite teardown; the process exit would drop the registration anyway.
    let _ = agent_manager_call_path(&connection, "UnregisterAgent", agent_path).await;
    connection
        .object_server()
        .remove::<PairingAgent, _>(AGENT_PATH)
        .await
        .ok();

    match outcome {
        Ok(_) => {
            // The bond exists from here on. Trust and connect are the
            // finishing moves that make the device usable without a second
            // trip through the picker; neither can undo the pairing.
            let connected =
                settle_paired_device(&connection, &device_path, session_started.elapsed()).await;
            report_done(&ipc, &target, cookie, true, None, Some(connected)).await;
            EXIT_PAIRED
        }
        Err(error) => {
            let cancelled = shared.ended_by_user.load(Ordering::Relaxed);
            let reason = if cancelled {
                "pairing cancelled".to_string()
            } else {
                format!("{error}")
            };
            report_done(&ipc, &target, cookie, false, Some(&reason), None).await;
            if cancelled {
                EXIT_CANCELLED
            } else {
                EXIT_FAILED
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery (`jwm-bridge discover`)
// ---------------------------------------------------------------------------

/// One device as printed for jwm's picker. Deliberately the same four facts
/// the text path scraped, plus the RSSI that path parsed and threw away.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScannedDevice {
    address: String,
    name: String,
    paired: bool,
    connected: bool,
    /// Signal strength in dBm when bluez has heard the device this session.
    /// Absent for a remembered device that is merely out of range.
    rssi: Option<i16>,
}

impl ScannedDevice {
    /// The wire shape jwm's `parse_bridge_devices` reads. Hand-built rather
    /// than derived so the bridge keeps its dependency list at `serde_json`
    /// alone — the schema is five fields and is pinned by a test on both
    /// sides.
    fn to_json(&self) -> Value {
        serde_json::json!({
            "address": self.address,
            "name": self.name,
            "paired": self.paired,
            "connected": self.connected,
            "rssi": self.rssi,
        })
    }
}

/// Read a boolean property out of a `GetManagedObjects` interface map,
/// defaulting to false — an absent flag is not a true one.
fn managed_bool(
    properties: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    key: &str,
) -> bool {
    properties
        .get(key)
        .and_then(|value| bool::try_from(value.clone()).ok())
        .unwrap_or(false)
}

fn managed_string(
    properties: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    key: &str,
) -> Option<String> {
    properties
        .get(key)
        .and_then(|value| String::try_from(value.clone()).ok())
}

fn managed_u32(
    properties: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    key: &str,
) -> Option<u32> {
    properties
        .get(key)
        .and_then(|value| u32::try_from(value.clone()).ok())
}

/// The controller's current `PairableTimeout` / `DiscoverableTimeout`, read
/// out of the `GetManagedObjects` tree the window already fetched to find the
/// adapter — so no extra `Properties.Get` round trip. `None` for a property
/// bluez did not publish, which the exposure treats as "value unknown".
fn adapter_timeouts(
    objects: &zbus::fdo::ManagedObjects,
    adapter: &OwnedObjectPath,
) -> (Option<u32>, Option<u32>) {
    let Ok(iface) = zbus::names::InterfaceName::try_from(ADAPTER_IFACE) else {
        return (None, None);
    };
    let Some(properties) = objects
        .get(adapter)
        .and_then(|interfaces| interfaces.get(&iface))
    else {
        return (None, None);
    };
    (
        managed_u32(properties, "PairableTimeout"),
        managed_u32(properties, "DiscoverableTimeout"),
    )
}

/// Pick the controller to scan with: a powered adapter if there is one,
/// otherwise the first adapter at all so the caller gets "not powered"
/// rather than "no bluetooth". Machines with two controllers are rare and
/// the picker has no way to choose between them, so first-wins is honest.
fn adapter_from_managed_objects(objects: &zbus::fdo::ManagedObjects) -> Option<OwnedObjectPath> {
    let iface = zbus::names::InterfaceName::try_from(ADAPTER_IFACE).ok()?;
    let mut fallback = None;
    let mut paths: Vec<_> = objects.iter().collect();
    // GetManagedObjects has no defined order; sort so hci0 beats hci1 every
    // run and the same controller is used across refreshes.
    paths.sort_by_key(|(path, _)| path.as_str().to_string());
    for (path, interfaces) in paths {
        let Some(properties) = interfaces.get(&iface) else {
            continue;
        };
        if managed_bool(properties, "Powered") {
            return Some(path.clone());
        }
        fallback.get_or_insert_with(|| path.clone());
    }
    fallback
}

/// Every `org.bluez.Device1` bluez currently knows about — remembered bonds
/// and, during or just after a scan, whatever the radio has heard.
fn devices_from_managed_objects(objects: &zbus::fdo::ManagedObjects) -> Vec<ScannedDevice> {
    let Ok(iface) = zbus::names::InterfaceName::try_from(DEVICE_IFACE) else {
        return Vec::new();
    };
    let mut devices: Vec<ScannedDevice> = Vec::new();
    let mut paths: Vec<_> = objects.iter().collect();
    paths.sort_by_key(|(path, _)| path.as_str().to_string());
    for (_, interfaces) in paths {
        let Some(properties) = interfaces.get(&iface) else {
            continue;
        };
        let Some(address) = managed_string(properties, "Address") else {
            continue;
        };
        let address = address.to_uppercase();
        if !is_valid_address(&address) {
            continue;
        }
        // Alias is what the user set (or bluez's own fallback); Name is what
        // the device announced. Prefer the former, and never show an empty
        // row: the address is a worse name but it is a name.
        // The name is chosen by a stranger in radio range and ends up on
        // jwm's picker, whose rows are joined with newlines and split back
        // into lines — one embedded control character shifts every row below
        // it off its own selection highlight. jwm sanitizes again; doing it
        // here too keeps the wire itself clean.
        let name = managed_string(properties, "Alias")
            .or_else(|| managed_string(properties, "Name"))
            .map(|name| {
                name.trim()
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .take(MAX_DEVICE_NAME_CHARS)
                    .collect::<String>()
                    .trim()
                    .to_string()
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| address.clone());
        let rssi = properties
            .get("RSSI")
            .and_then(|value| i16::try_from(value.clone()).ok());
        devices.push(ScannedDevice {
            address,
            name,
            paired: managed_bool(properties, "Paired"),
            connected: managed_bool(properties, "Connected"),
            rssi,
        });
    }
    // Sort before the cap. Object-path order is MAC order, so truncating
    // first would drop a connected device at `F0:…` behind sixty-four
    // nameless beacons — in exactly the crowded room the cap is for. The
    // ordering matches jwm's own `sort_devices`, so the two agree about
    // which devices matter.
    devices.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then_with(|| b.paired.cmp(&a.paired))
            .then_with(|| b.rssi.is_some().cmp(&a.rssi.is_some()))
            .then_with(|| b.rssi.cmp(&a.rssi))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    devices.truncate(MAX_DISCOVERED_DEVICES);
    devices
}

/// Clamp a caller-supplied scan window. Zero is meaningful: "list what bluez
/// already knows, do not touch the radio".
fn discovery_window(seconds: u64) -> Duration {
    Duration::from_secs(seconds.min(DISCOVERY_MAX_SECONDS))
}

async fn managed_objects(connection: &Connection) -> Option<zbus::fdo::ManagedObjects> {
    let proxy = zbus::fdo::ObjectManagerProxy::builder(connection)
        .destination(BLUEZ)
        .ok()?
        .path("/")
        .ok()?
        .build()
        .await
        .map_err(|error| log::warn!("discover: cannot build an ObjectManager proxy: {error}"))
        .ok()?;
    match tokio::time::timeout(DISCOVERY_CALL_TIMEOUT, proxy.get_managed_objects()).await {
        Ok(Ok(objects)) => Some(objects),
        Ok(Err(error)) => {
            log::warn!("discover: GetManagedObjects failed: {error}");
            None
        }
        Err(_) => {
            log::warn!("discover: GetManagedObjects did not answer in time");
            None
        }
    }
}

/// One bounded `Adapter1` call. Discovery start/stop are both best effort:
/// a controller that refuses to scan still has a device list worth printing,
/// and process exit ends the scan whatever `StopDiscovery` did.
async fn adapter_call(connection: &Connection, adapter: &OwnedObjectPath, method: &str) -> bool {
    let call = connection.call_method(
        Some(bluez_name()),
        adapter,
        Some(ADAPTER_IFACE),
        method,
        &(),
    );
    match tokio::time::timeout(DISCOVERY_CALL_TIMEOUT, call).await {
        Ok(Ok(_)) => true,
        Ok(Err(error)) => {
            log::warn!("discover: {method} failed: {error}");
            false
        }
        Err(_) => {
            log::warn!("discover: {method} did not answer in time");
            false
        }
    }
}

/// Entry point for `jwm-bridge discover`: list what bluez knows, optionally
/// after running the radio for `seconds`. Prints a JSON array on stdout —
/// jwm parses it on the worker thread that used to parse `bluetoothctl`.
pub async fn discover(seconds: u64) -> i32 {
    let connection = match zbus::connection::Builder::system() {
        Ok(builder) => match builder.build().await {
            Ok(connection) => connection,
            Err(error) => {
                log::warn!("discover: cannot reach the system bus: {error}");
                return EXIT_ERROR;
            }
        },
        Err(error) => {
            log::warn!("discover: no system bus address: {error}");
            return EXIT_ERROR;
        }
    };

    // Read the object tree while the scan is still running: bluez publishes
    // RSSI on a device only for as long as it is hearing it, so a sweep
    // taken after StopDiscovery loses exactly the field the text path never
    // had. Stopping afterwards keeps the radio idle for the next refresh.
    let window = discovery_window(seconds);
    let mut scanning = None;
    if !window.is_zero() {
        match managed_objects(&connection)
            .await
            .as_ref()
            .and_then(adapter_from_managed_objects)
        {
            Some(adapter) => {
                if adapter_call(&connection, &adapter, "StartDiscovery").await {
                    tokio::time::sleep(window).await;
                    scanning = Some(adapter);
                }
            }
            None => log::warn!("discover: no bluez adapter to scan with"),
        }
    }

    let objects = managed_objects(&connection).await;
    if let Some(adapter) = scanning {
        adapter_call(&connection, &adapter, "StopDiscovery").await;
    }
    let Some(objects) = objects else {
        return EXIT_ERROR;
    };
    let devices = devices_from_managed_objects(&objects);
    let payload = Value::Array(devices.iter().map(ScannedDevice::to_json).collect());
    println!("{payload}");
    EXIT_OK
}

// ---------------------------------------------------------------------------
// Inbound window (`jwm-bridge accept`)
// ---------------------------------------------------------------------------

/// Set a `u32` `Adapter1` property. Used for the pairable/discoverable
/// timeouts, which are what makes a killed helper self-healing.
async fn set_adapter_u32(
    connection: &Connection,
    adapter: &OwnedObjectPath,
    name: &str,
    value: u32,
) {
    let body = (ADAPTER_IFACE, name, zbus::zvariant::Value::U32(value));
    let call = connection.call_method(
        Some(bluez_name()),
        adapter,
        Some(PROPERTIES_IFACE),
        "Set",
        &body,
    );
    if let Ok(Err(error)) = tokio::time::timeout(DISCOVERY_CALL_TIMEOUT, call).await {
        log::warn!("accept: could not set {name}: {error}");
    }
}

/// Set a `bool` `Adapter1` property, reporting whether the write landed.
///
/// The answer matters on the way *in*: a window whose `Pairable` or
/// `Discoverable` write bluez refused is armed, on screen, and reachable by
/// nothing, and the user has no way to tell that from a window nothing has
/// rung yet. On the way out it is only logged — teardown has nothing better
/// to do about a refusal than say so.
async fn set_adapter_flag(
    connection: &Connection,
    adapter: &OwnedObjectPath,
    name: &str,
    value: bool,
) -> bool {
    let body = (ADAPTER_IFACE, name, zbus::zvariant::Value::Bool(value));
    let call = connection.call_method(
        Some(bluez_name()),
        adapter,
        Some(PROPERTIES_IFACE),
        "Set",
        &body,
    );
    match tokio::time::timeout(DISCOVERY_CALL_TIMEOUT, call).await {
        Ok(Ok(_)) => true,
        Ok(Err(error)) => {
            log::warn!("accept: could not set {name}: {error}");
            false
        }
        Err(_) => {
            log::warn!("accept: setting {name} timed out");
            false
        }
    }
}

/// Make the controller reachable for the life of one inbound window.
///
/// Without `Pairable` and `Discoverable` nothing can ask to bond in the first
/// place — on a default session both are false — so the window would be
/// armed, correct, and completely unreachable.
///
/// **Teardown clears both rather than restoring what it found.** Restoring a
/// snapshot compounds: a second window overlapping the first would snapshot
/// the `true` the first installed and write it back on the way out, leaving
/// the controller pairable and discoverable indefinitely with no agent
/// registered and nothing on screen saying so. Off is also the only safe
/// direction to be wrong in, and jwm turns these on by no other route, so on
/// a jwm session clearing *is* restoring.
///
/// The timeouts are the belt for what teardown cannot cover at all — a
/// SIGKILL, or the session ending underneath the helper. BlueZ counts them
/// down itself and clears both flags, so a helper that dies without
/// unwinding still cannot leave the controller open past its own window.
///
/// But `PairableTimeout`/`DiscoverableTimeout` are adapter-wide settings
/// bluez persists to disk, not per-client state, so shortening them and
/// walking away would silently cut *every other tool's* discoverable/pairable
/// window on this machine to sixty seconds, permanently. So a normal teardown
/// restores whatever it found — while still only ever *lowering* a value that
/// was longer than the window (or the "forever" 0), never re-imposing a value
/// a previous killed helper already shortened as if it were the original.
struct AdapterExposure {
    adapter: OwnedObjectPath,
    /// The original timeout to write back on the way out, when the window
    /// changed it. `None` means "leave it": either the value was already at
    /// or below the window (nothing to lower), or bluez never published it.
    pairable_timeout: Option<u32>,
    discoverable_timeout: Option<u32>,
    /// Whether bluez actually took each flag. A window that could not raise
    /// them is unreachable, and saying so is the difference between the user
    /// waiting out sixty seconds and being told to look at their adapter.
    pairable: bool,
    discoverable: bool,
}

impl AdapterExposure {
    async fn open(
        connection: &Connection,
        adapter: OwnedObjectPath,
        pairable_timeout: Option<u32>,
        discoverable_timeout: Option<u32>,
    ) -> AdapterExposure {
        let window = INBOUND_WINDOW.as_secs().min(u64::from(u32::MAX)) as u32;
        let pairable_timeout = clamp_timeout(
            connection,
            &adapter,
            "PairableTimeout",
            pairable_timeout,
            window,
        )
        .await;
        let discoverable_timeout = clamp_timeout(
            connection,
            &adapter,
            "DiscoverableTimeout",
            discoverable_timeout,
            window,
        )
        .await;
        let pairable = set_adapter_flag(connection, &adapter, "Pairable", true).await;
        let discoverable = set_adapter_flag(connection, &adapter, "Discoverable", true).await;
        AdapterExposure {
            adapter,
            pairable_timeout,
            discoverable_timeout,
            pairable,
            discoverable,
        }
    }

    async fn close(&self, connection: &Connection) {
        let _ = set_adapter_flag(connection, &self.adapter, "Pairable", false).await;
        let _ = set_adapter_flag(connection, &self.adapter, "Discoverable", false).await;
        // Put the adapter-wide countdowns back the way they were, so one jwm
        // window does not permanently shorten every other tool's window.
        if let Some(original) = self.pairable_timeout {
            set_adapter_u32(connection, &self.adapter, "PairableTimeout", original).await;
        }
        if let Some(original) = self.discoverable_timeout {
            set_adapter_u32(connection, &self.adapter, "DiscoverableTimeout", original).await;
        }
    }
}

/// What one adapter-wide timeout should become while the window is open, and
/// what to put back at teardown: `(write, restore)`.
///
/// Only a value that is `0` ("forever") or longer than the window is
/// shortened. A value already within the window is left untouched *and* not
/// remembered — that is the guard against the snapshot compounding: a timeout
/// a previous killed helper already dropped to the window must never be taken
/// for the user's own and written back as if it were.
const fn plan_timeout(current: Option<u32>, window: u32) -> (Option<u32>, Option<u32>) {
    match current {
        // Already short enough: leave it, and nothing to restore.
        Some(value) if value != 0 && value <= window => (None, None),
        // 0 (forever), longer than the window, or unknown: shorten to the
        // window and restore the original at close (`None` restores nothing).
        other => (Some(window), other),
    }
}

/// Thin executor for [`plan_timeout`]: write the planned value, if any, and
/// report what `close` should restore.
async fn clamp_timeout(
    connection: &Connection,
    adapter: &OwnedObjectPath,
    name: &str,
    current: Option<u32>,
    window: u32,
) -> Option<u32> {
    let (write, restore) = plan_timeout(current, window);
    if let Some(value) = write {
        set_adapter_u32(connection, adapter, name, value).await;
    }
    restore
}

/// Why an armed window cannot be rung, or `None` when it can.
///
/// Every arm of this is a state the old code held the window open through for
/// the full sixty seconds: jwm's picker counted down an armed window while the
/// helper sat on an adapter that did not exist, or one that had refused to
/// become pairable or discoverable, with nothing on screen saying so.
///
/// Both flags are required, not just `Pairable`. A window is armed by pressing
/// `a` in front of a device that has never seen this controller — that is the
/// whole gesture — and such a device cannot ring a controller it cannot find.
/// A pairable-but-hidden window would work only for a device that already
/// knows the address, which is not what the panel promised.
const fn window_failure(adapter: bool, pairable: bool, discoverable: bool) -> Option<&'static str> {
    if !adapter {
        Some("no Bluetooth adapter")
    } else if !pairable {
        Some("the adapter refused to become pairable")
    } else if !discoverable {
        Some("the adapter refused to become discoverable")
    } else {
        None
    }
}

/// The `bluetooth_pairing_failed` payload: a cookie and a reason, and
/// deliberately no address.
///
/// `done` reports an outcome *for a device* and is bound to its session by
/// the address it names; a window nothing has rung has no address to name, so
/// the terminal report for a helper that died before it armed anything needs
/// a shape of its own. See `jwm::features::pairing::parse_failed_command`.
fn failed_args(cookie: &str, error: &str) -> Value {
    serde_json::json!({
        "cookie": cookie,
        "error": error,
    })
}

/// Tell jwm this session never got off the ground, so the picker stops
/// claiming an armed window the helper is not holding.
///
/// A jwm too old to know the verb answers with an error, which is logged and
/// changes nothing: the picker then times the window out as it did before.
async fn report_failed(ipc: &JwmIpc, cookie: &str, error: &str) {
    let args = failed_args(cookie, error);
    let ipc = ipc.clone();
    let sent =
        tokio::task::spawn_blocking(move || ipc.command("bluetooth_pairing_failed", args)).await;
    match sent {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => log::warn!("jwm rejected the window failure report: {error}"),
        Err(error) => log::warn!("could not report the window failure: {error}"),
    }
}

/// Hold one inbound window open: register an agent, make the controller
/// reachable, and answer whatever rings until jwm cancels or the window
/// closes. Unlike [`pair_session`] this never calls `Pair` — the remote side
/// drives, and this process only relays the question and the answer.
pub async fn accept_session(ipc: JwmIpc, connection: Connection, cookie: &str) -> i32 {
    // Self-heal before touching bluez, exactly as the pairing helper does:
    // jwm may have closed the window between spawning this and now.
    let liveness = {
        let ipc = ipc.clone();
        tokio::task::spawn_blocking(move || ipc.query("get_bluetooth_pairing")).await
    };
    match liveness {
        Ok(Ok(value)) if inbound_session_matches(&value, cookie) => {}
        Ok(Ok(_)) => {
            log::info!("accept: jwm no longer holds this window; exiting");
            return EXIT_CANCELLED;
        }
        Ok(Err(error)) => {
            log::warn!("accept: jwm refused the session query: {error}");
            return EXIT_ERROR;
        }
        Err(error) => {
            log::warn!("accept: could not query jwm: {error}");
            return EXIT_ERROR;
        }
    }

    let (tx, responses) = mpsc::channel(EVENT_QUEUE_CAPACITY);
    crate::jwm_ipc::subscribe(ipc.clone(), &["bluetooth"], tx);

    let shared = Arc::new(Shared {
        // Nobody can say which device will ring; the first callback binds it.
        target: Mutex::new(None),
        cookie: cookie.to_string(),
        accepts_inbound: true,
        pending: Mutex::new(None),
        next_request_id: std::sync::atomic::AtomicU64::new(1),
        ended_by_user: AtomicBool::new(false),
        authorized: AtomicBool::new(false),
        ipc: ipc.clone(),
    });
    if let Err(error) = connection
        .object_server()
        .at(
            INBOUND_AGENT_PATH,
            PairingAgent {
                shared: shared.clone(),
                name_lookup: Some(connection.clone()),
            },
        )
        .await
    {
        log::warn!("accept: could not serve the agent: {error}");
        report_failed(&ipc, cookie, "could not register the pairing agent").await;
        return EXIT_ERROR;
    }

    let agent_path = ObjectPath::from_static_str(INBOUND_AGENT_PATH)
        .expect("INBOUND_AGENT_PATH is a valid object path");
    if let Err(error) = agent_manager_call(
        &connection,
        "RegisterAgent",
        &(agent_path.clone(), CAPABILITY),
    )
    .await
    {
        log::warn!("accept: RegisterAgent failed: {error}");
        // The object server is still holding the agent; drop it before
        // leaving, the same way the normal teardown below does. Nothing has
        // been registered with bluez, so there is no UnregisterAgent to
        // pair with it.
        connection
            .object_server()
            .remove::<PairingAgent, _>(INBOUND_AGENT_PATH)
            .await
            .ok();
        report_failed(&ipc, cookie, "bluez refused the pairing agent").await;
        return EXIT_FAILED;
    }
    // Inbound requests go to the *default* agent, so without this the window
    // is registered and never called.
    let _ = agent_manager_call_path(&connection, "RequestDefaultAgent", agent_path.clone()).await;

    let objects = managed_objects(&connection).await;
    let exposure = match objects.as_ref().and_then(adapter_from_managed_objects) {
        Some(adapter) => {
            // Read the current adapter-wide timeouts out of the same tree, so
            // teardown can put them back rather than leaving the machine's
            // discoverable/pairable window permanently cut to sixty seconds.
            let (pairable, discoverable) = objects
                .as_ref()
                .map(|objects| adapter_timeouts(objects, &adapter))
                .unwrap_or((None, None));
            Some(AdapterExposure::open(&connection, adapter, pairable, discoverable).await)
        }
        None => {
            log::warn!("accept: no bluez adapter to make discoverable");
            None
        }
    };

    // An armed window nothing can reach is worse than no window: jwm's picker
    // counts down sixty seconds the user could have spent on a controller
    // that works. Report it and unwind through the same teardown a normal
    // close uses, rather than returning early past it.
    let failure = window_failure(
        exposure.is_some(),
        exposure.as_ref().is_some_and(|exposure| exposure.pairable),
        exposure
            .as_ref()
            .is_some_and(|exposure| exposure.discoverable),
    );

    // Nothing to drive: wait for a callback to arrive and be answered, or for
    // jwm to cancel, or for the window to close on its own. `pump_responses`
    // needs a device path for `CancelPairing`; an inbound window has none
    // until something rings, so cancellation is handled inline here.
    let cancelled = if let Some(reason) = failure {
        log::warn!("accept: {reason}; giving the window back");
        report_failed(&ipc, cookie, reason).await;
        false
    } else {
        tokio::time::timeout(
            INBOUND_WINDOW,
            pump_inbound_responses(shared.clone(), responses),
        )
        .await
        .is_ok()
    };

    if let Some(exposure) = exposure.as_ref() {
        exposure.close(&connection).await;
    }
    let _ = agent_manager_call_path(&connection, "UnregisterAgent", agent_path).await;
    connection
        .object_server()
        .remove::<PairingAgent, _>(INBOUND_AGENT_PATH)
        .await
        .ok();

    // The window is not a pairing attempt, so there is no pairing outcome to
    // report unless something actually rang and bound a device. Closing the
    // window is not a refusal — only an explicit `n` is — so the report reads
    // `ok` off what the user actually granted, not off whether the session
    // ended by the user's hand (which a close always makes true).
    if let Some(address) = shared.target() {
        let ok = shared.authorized.load(Ordering::Relaxed);
        report_done(&ipc, &address, cookie, ok, (!ok).then_some("refused"), None).await;
    }
    match (failure, cancelled) {
        (Some(_), _) => EXIT_FAILED,
        (None, true) => EXIT_CANCELLED,
        (None, false) => EXIT_OK,
    }
}

/// Turn jwm's answers into agent replies for an inbound window. Returns when
/// the user cancels with nothing outstanding — the window is closed, so there
/// is nothing left to wait for.
async fn pump_inbound_responses(shared: Arc<Shared>, mut responses: mpsc::Receiver<Value>) {
    while let Some(event) = responses.recv().await {
        let Some(response) = parse_response_event(&event) else {
            continue;
        };
        if response.cookie != shared.cookie {
            log::warn!("ignoring a pairing response for a foreign cookie");
            continue;
        }
        if response.reply.is_user_termination() {
            shared.ended_by_user.store(true, Ordering::Relaxed);
        }
        let closing = response.reply.ends_the_session();
        // Answer the request this is an answer to, so bluez gets a real
        // reply rather than a dropped channel. An answer whose request is
        // already gone resolves nothing.
        if let Some(reply) = response.request_id.and_then(|id| shared.take_pending(id)) {
            // A granted request is what makes the window's outcome a success;
            // record it so `done` reports `ok` on its own merits rather than
            // on "the user did not end the session", which the close itself
            // would falsify.
            if matches!(response.reply, UserReply::Confirmed | UserReply::Pin(_)) {
                shared.authorized.store(true, Ordering::Relaxed);
            }
            let _ = reply.send(response.reply);
        }
        // Closing the window has to happen here even when it answered an
        // outstanding request — Esc on an authorization prompt IS a close
        // with a request pending, which is the common case. Returning only
        // when nothing was pending left the controller pairable and
        // discoverable, and this process bluez's default agent, for the rest
        // of the sixty seconds while jwm said the window was shut. A
        // withdrawn prompt is not a close: the session lives on, so the
        // window does too.
        if closing {
            log::info!("accept: window closed");
            return;
        }
    }
}

/// Entry point for `jwm-bridge accept`: connect the system bus and hold one
/// inbound window open.
pub async fn run_accept(cookie: &str) -> i32 {
    let ipc = JwmIpc::new();
    // The IPC socket is independent of D-Bus, so a bus that cannot be reached
    // must still be reported — otherwise the picker counts down an armed
    // sixty-second window while the helper that was supposed to hold it open
    // has already exited. The report carries no address because there is none
    // to carry: nothing has rung this window, and nothing now can.
    let connection = match zbus::connection::Builder::system() {
        Ok(builder) => match builder.build().await {
            Ok(connection) => connection,
            Err(error) => {
                log::warn!("accept: cannot reach the system bus: {error}");
                report_failed(&ipc, cookie, "no system bus").await;
                return EXIT_ERROR;
            }
        },
        Err(error) => {
            log::warn!("accept: no system bus address: {error}");
            report_failed(&ipc, cookie, "no system bus").await;
            return EXIT_ERROR;
        }
    };
    // One window's hard bound, a little past jwm's own so jwm's cancel is
    // what normally ends it — the same ordering the pairing clocks use.
    match tokio::time::timeout(
        INBOUND_WINDOW + PROMPT_WAIT,
        accept_session(ipc, connection, cookie),
    )
    .await
    {
        Ok(code) => code,
        Err(_) => {
            // No report here, unlike the pairing wall clock: this one is
            // thirty-five seconds past the window jwm itself drops at sixty,
            // so there is no session left for a report to name.
            log::warn!("accept: exceeded the inbound window wall clock");
            EXIT_ERROR
        }
    }
}

/// Entry point for `jwm-bridge pair`: connect the system bus and run one
/// session under the hard wall clock.
pub async fn run(address: &str, cookie: &str) -> i32 {
    if !is_valid_address(&address.to_uppercase()) {
        log::warn!("pair: {address:?} is not a Bluetooth address");
        return EXIT_USAGE;
    }
    let ipc = JwmIpc::new();
    // The IPC socket is independent of D-Bus, so a bus that cannot be reached
    // must still be reported — otherwise the picker sits on "Pairing with X…"
    // for the full 95s session timeout before jwm gives up on a helper that
    // died in its first fifty milliseconds.
    let connection = match zbus::connection::Builder::system() {
        Ok(builder) => match builder.build().await {
            Ok(connection) => connection,
            Err(error) => {
                log::warn!("pair {address}: cannot reach the system bus: {error}");
                report_done(&ipc, address, cookie, false, Some("no system bus"), None).await;
                return EXIT_ERROR;
            }
        },
        Err(error) => {
            log::warn!("pair {address}: no system bus address: {error}");
            report_done(&ipc, address, cookie, false, Some("no system bus"), None).await;
            return EXIT_ERROR;
        }
    };
    match tokio::time::timeout(
        WALL_CLOCK,
        pair_session(ipc.clone(), connection, address, cookie),
    )
    .await
    {
        Ok(code) => code,
        Err(_) => {
            log::warn!("pair {address}: exceeded the {WALL_CLOCK:?} wall clock");
            report_done(
                &ipc,
                address,
                cookie,
                false,
                Some("pairing timed out"),
                None,
            )
            .await;
            EXIT_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use zbus::zvariant::OwnedValue;

    const ADDR: &str = "5C:FB:7C:1A:2B:3C";
    const COOKIE: &str = "0123456789abcdef";
    const DEVICE_PATH: &str = "/org/bluez/hci0/dev_5C_FB_7C_1A_2B_3C";
    const ADAPTER_PATH: &str = "/org/bluez/hci0";

    // --- Pure mapping tests ---

    #[test]
    fn device_paths_yield_their_address() {
        assert_eq!(
            address_from_device_path(DEVICE_PATH),
            Some(ADDR.to_string())
        );
        // Case folds up for comparison.
        assert_eq!(
            address_from_device_path("/org/bluez/hci1/dev_5c_fb_7c_1a_2b_3c"),
            Some(ADDR.to_string())
        );
        assert_eq!(address_from_device_path("/org/bluez/hci0"), None);
        assert_eq!(
            address_from_device_path("/org/bluez/hci0/dev_not_an_address"),
            None
        );
        assert_eq!(address_from_device_path(""), None);
    }

    fn managed_objects(entries: &[(&str, &str)]) -> zbus::fdo::ManagedObjects {
        entries
            .iter()
            .map(|(path, address)| {
                let mut properties = HashMap::new();
                properties.insert(
                    "Address".to_string(),
                    OwnedValue::from(zbus::zvariant::Str::from(*address)),
                );
                let mut interfaces = HashMap::new();
                interfaces.insert(
                    zbus::names::OwnedInterfaceName::try_from(DEVICE_IFACE).expect("interface"),
                    properties,
                );
                (OwnedObjectPath::try_from(*path).expect("path"), interfaces)
            })
            .collect()
    }

    #[test]
    fn the_target_device_is_found_by_address_not_by_guessing_hci0() {
        let objects = managed_objects(&[
            ("/org/bluez/hci1/dev_5C_FB_7C_1A_2B_3C", "5C:FB:7C:1A:2B:3C"),
            ("/org/bluez/hci0/dev_11_22_33_44_55_66", "11:22:33:44:55:66"),
        ]);
        let found = device_path_from_managed_objects(&objects, ADDR).expect("found");
        assert_eq!(found.as_str(), "/org/bluez/hci1/dev_5C_FB_7C_1A_2B_3C");

        // Case-insensitive on the property side, absent devices yield None.
        assert!(device_path_from_managed_objects(&objects, "5c:fb:7c:1a:2b:3c").is_some());
        assert!(device_path_from_managed_objects(&objects, "AA:BB:CC:DD:EE:FF").is_none());
        assert!(device_path_from_managed_objects(&managed_objects(&[]), ADDR).is_none());
    }

    /// One object in a fabricated `GetManagedObjects` reply: where it lives,
    /// which interface it implements, and the properties bluez would publish.
    type ManagedEntry<'a> = (&'a str, &'a str, &'a [(&'a str, OwnedValue)]);

    /// A `GetManagedObjects` reply built from [`ManagedEntry`] triples, so a
    /// test can describe adapters and devices in one tree.
    fn managed_tree(entries: &[ManagedEntry<'_>]) -> zbus::fdo::ManagedObjects {
        let mut tree: zbus::fdo::ManagedObjects = HashMap::new();
        for (path, interface, properties) in entries {
            let properties: HashMap<String, OwnedValue> = properties
                .iter()
                .map(|(key, value)| ((*key).to_string(), value.clone()))
                .collect();
            tree.entry(OwnedObjectPath::try_from(*path).expect("path"))
                .or_default()
                .insert(
                    zbus::names::OwnedInterfaceName::try_from(*interface).expect("interface"),
                    properties,
                );
        }
        tree
    }

    fn text(value: &str) -> OwnedValue {
        OwnedValue::from(zbus::zvariant::Str::from(value))
    }

    #[test]
    fn a_powered_adapter_wins_and_the_choice_is_stable_across_runs() {
        let objects = managed_tree(&[
            (
                "/org/bluez/hci1",
                ADAPTER_IFACE,
                &[("Powered", OwnedValue::from(true))],
            ),
            (
                "/org/bluez/hci0",
                ADAPTER_IFACE,
                &[("Powered", OwnedValue::from(false))],
            ),
        ]);
        assert_eq!(
            adapter_from_managed_objects(&objects)
                .expect("adapter")
                .as_str(),
            "/org/bluez/hci1"
        );

        // With nothing powered the caller still gets an adapter, so the
        // failure reads as "not powered" rather than "no bluetooth". Two
        // unpowered controllers resolve by path so refreshes agree.
        let dark = managed_tree(&[
            (
                "/org/bluez/hci1",
                ADAPTER_IFACE,
                &[("Powered", OwnedValue::from(false))],
            ),
            (
                "/org/bluez/hci0",
                ADAPTER_IFACE,
                &[("Powered", OwnedValue::from(false))],
            ),
        ]);
        assert_eq!(
            adapter_from_managed_objects(&dark)
                .expect("adapter")
                .as_str(),
            "/org/bluez/hci0"
        );

        // A tree with only devices in it has no adapter.
        let devices = managed_tree(&[(DEVICE_PATH, DEVICE_IFACE, &[("Address", text(ADDR))])]);
        assert!(adapter_from_managed_objects(&devices).is_none());
    }

    #[test]
    fn discovered_devices_carry_the_facts_the_text_path_had_to_shell_out_for() {
        let objects = managed_tree(&[
            (
                "/org/bluez/hci0",
                ADAPTER_IFACE,
                &[("Powered", OwnedValue::from(true))],
            ),
            (
                DEVICE_PATH,
                DEVICE_IFACE,
                &[
                    ("Address", text(ADDR)),
                    ("Alias", text("Studio Headphones")),
                    ("Name", text("WH-1000XM4")),
                    ("Paired", OwnedValue::from(true)),
                    ("Connected", OwnedValue::from(true)),
                    ("RSSI", OwnedValue::from(-63i16)),
                ],
            ),
        ]);
        let devices = devices_from_managed_objects(&objects);
        assert_eq!(devices.len(), 1, "the adapter is not a device");
        let device = &devices[0];
        assert_eq!(device.address, ADDR);
        // Alias is what the user renamed it to; Name is what it announced.
        assert_eq!(device.name, "Studio Headphones");
        assert!(device.paired);
        assert!(device.connected);
        assert_eq!(device.rssi, Some(-63));
    }

    #[test]
    fn a_device_with_nothing_but_an_address_still_makes_a_usable_row() {
        let objects = managed_tree(&[(
            DEVICE_PATH,
            DEVICE_IFACE,
            // No Alias, no Name, no flags: bluez publishes exactly this for
            // a beacon it has only just heard.
            &[("Address", text("5c:fb:7c:1a:2b:3c"))],
        )]);
        let devices = devices_from_managed_objects(&objects);
        assert_eq!(devices.len(), 1);
        // The address is a poor name but never an empty row, and it is
        // upper-cased so it matches everything else on the wire.
        assert_eq!(devices[0].address, ADDR);
        assert_eq!(devices[0].name, ADDR);
        assert!(!devices[0].paired);
        assert!(!devices[0].connected);
        assert_eq!(devices[0].rssi, None);
    }

    #[test]
    fn the_device_list_is_bounded_before_it_reaches_the_wire() {
        // A remote peer controls both how many devices appear and what they
        // are called; the picker's own caps are mirrored here so a crowded
        // room cannot make stdout unbounded.
        let long = "x".repeat(MAX_DEVICE_NAME_CHARS * 3);
        let mut entries: Vec<(String, String)> = Vec::new();
        for index in 0..(MAX_DISCOVERED_DEVICES + 20) {
            entries.push((
                format!(
                    "/org/bluez/hci0/dev_AA_BB_CC_{:02X}_{:02X}_{:02X}",
                    index / 256,
                    index % 256,
                    index % 7
                ),
                format!(
                    "AA:BB:CC:{:02X}:{:02X}:{:02X}",
                    index / 256,
                    index % 256,
                    index % 7
                ),
            ));
        }
        type OwnedProperties<'a> = Vec<(&'a str, OwnedValue)>;
        let properties: Vec<(&str, OwnedProperties<'_>)> = entries
            .iter()
            .map(|(path, address)| {
                (
                    path.as_str(),
                    vec![("Address", text(address)), ("Alias", text(&long))],
                )
            })
            .collect();
        let mut borrowed: Vec<ManagedEntry<'_>> = properties
            .iter()
            .map(|(path, props)| (*path, DEVICE_IFACE, props.as_slice()))
            .collect();

        // A connected, paired device whose MAC sorts to the very end of the
        // object tree. Truncating in path order — i.e. MAC order, the way the
        // code did before it sorted first — would drop it behind the sixty-
        // four nameless beacons, which is exactly the connected headset the
        // cap exists to keep. Sorting before the cap floats it to the front.
        let connected_path = "/org/bluez/hci0/dev_F0_FF_FF_FF_FF_FF";
        let connected_props: OwnedProperties<'_> = vec![
            ("Address", text("F0:FF:FF:FF:FF:FF")),
            ("Alias", text("Studio Headphones")),
            ("Connected", OwnedValue::from(true)),
            ("Paired", OwnedValue::from(true)),
        ];
        borrowed.push((connected_path, DEVICE_IFACE, connected_props.as_slice()));

        let devices = devices_from_managed_objects(&managed_tree(&borrowed));
        assert_eq!(devices.len(), MAX_DISCOVERED_DEVICES);
        assert_eq!(
            devices[0].address, "F0:FF:FF:FF:FF:FF",
            "the connected device survives the cap and leads the list"
        );
        assert_eq!(devices[0].name, "Studio Headphones");
        assert!(
            devices
                .iter()
                .all(|device| device.name.chars().count() <= MAX_DEVICE_NAME_CHARS)
        );
    }

    #[test]
    fn a_ringing_device_is_named_even_when_the_picker_cap_would_drop_it() {
        // The device that rings an inbound window is unpaired and, without a
        // scan running, has no RSSI, so it sorts into the tail — past the
        // 64-device cap `devices_from_managed_objects` applies for the picker.
        // The prompt must still name it, so the label comes straight out of
        // the tree by address rather than through the capped list.
        let mut entries: Vec<(String, Vec<(&str, OwnedValue)>)> = Vec::new();
        // The target: last in MAC order, unpaired, no RSSI — the worst case.
        entries.push((
            "/org/bluez/hci0/dev_FF_FF_FF_FF_FF_FF".to_string(),
            vec![
                ("Address", text("FF:FF:FF:FF:FF:FF")),
                ("Alias", text("Ringing Phone")),
            ],
        ));
        // Enough connected, paired devices to fill the cap and outrank it.
        for index in 0..(MAX_DISCOVERED_DEVICES + 4) {
            let address = format!(
                "00:00:00:{:02X}:{:02X}:{:02X}",
                index / 256,
                index % 256,
                index % 7
            );
            entries.push((
                format!(
                    "/org/bluez/hci0/dev_00_00_00_{:02X}_{:02X}_{:02X}",
                    index / 256,
                    index % 256,
                    index % 7
                ),
                vec![
                    ("Address", text(&address)),
                    ("Connected", OwnedValue::from(true)),
                    ("Paired", OwnedValue::from(true)),
                ],
            ));
        }
        let borrowed: Vec<ManagedEntry<'_>> = entries
            .iter()
            .map(|(path, props)| (path.as_str(), DEVICE_IFACE, props.as_slice()))
            .collect();
        let tree = managed_tree(&borrowed);

        // The picker cap would indeed have dropped the target...
        let capped = devices_from_managed_objects(&tree);
        assert_eq!(capped.len(), MAX_DISCOVERED_DEVICES);
        assert!(
            !capped
                .iter()
                .any(|device| device.address == "FF:FF:FF:FF:FF:FF"),
            "the target sorts past the cap, so the capped list cannot name it"
        );
        // ...but the direct lookup names it regardless, case-folding the
        // address the way every other comparison on this wire does.
        assert_eq!(
            device_name_from_managed_objects(&tree, "FF:FF:FF:FF:FF:FF").as_deref(),
            Some("Ringing Phone")
        );
        assert_eq!(
            device_name_from_managed_objects(&tree, "ff:ff:ff:ff:ff:ff").as_deref(),
            Some("Ringing Phone")
        );
        // A device bluez knows only by address yields no name: jwm falls
        // back to the MAC rather than showing it twice.
        let bare = managed_tree(&[(DEVICE_PATH, DEVICE_IFACE, &[("Address", text(ADDR))])]);
        assert_eq!(device_name_from_managed_objects(&bare, ADDR), None);
    }

    #[test]
    fn an_adapter_timeout_is_only_ever_lowered_and_only_a_lowered_one_is_restored() {
        let window = INBOUND_WINDOW.as_secs() as u32;

        // A default session: pairable forever, discoverable for three
        // minutes. Both are longer than the window, so both are shortened to
        // it and both are put back at teardown.
        assert_eq!(plan_timeout(Some(0), window), (Some(window), Some(0)));
        assert_eq!(plan_timeout(Some(180), window), (Some(window), Some(180)));

        // Already within the window: nothing is written, so there is nothing
        // to restore. This is the arm that stops the snapshot compounding —
        // a value a previous killed helper left at the window is never taken
        // for the user's own and written back as if it were.
        assert_eq!(plan_timeout(Some(window), window), (None, None));
        assert_eq!(plan_timeout(Some(30), window), (None, None));
        assert_eq!(plan_timeout(Some(1), window), (None, None));

        // A property bluez never published: shorten it as the belt for a
        // SIGKILL, but restore nothing — the original is not knowable, and
        // inventing one would be worse than leaving the belt on.
        assert_eq!(plan_timeout(None, window), (Some(window), None));
    }

    #[test]
    fn an_unreachable_window_is_reported_rather_than_held_open() {
        // Everything in place: the window is real, so there is nothing to
        // report and the helper waits for something to ring.
        assert_eq!(window_failure(true, true, true), None);

        // Each of these was a sixty-second countdown on the picker for a
        // window nothing could ever ring. The adapter is checked first
        // because without one the other two are meaningless.
        assert_eq!(
            window_failure(false, false, false),
            Some("no Bluetooth adapter")
        );
        assert_eq!(
            window_failure(true, false, true),
            Some("the adapter refused to become pairable")
        );
        assert_eq!(
            window_failure(true, true, false),
            Some("the adapter refused to become discoverable")
        );
        // Both refused reports the first: one message, and pairable is the
        // one bluez needs to accept a bond at all.
        assert_eq!(
            window_failure(true, false, false),
            Some("the adapter refused to become pairable")
        );
    }

    #[test]
    fn a_failure_report_names_the_session_by_cookie_and_carries_no_address() {
        let failed = failed_args(COOKIE, "no system bus");
        assert_eq!(failed["cookie"], COOKIE);
        assert_eq!(failed["error"], "no system bus");
        // The whole point of the frame: an unrung window has no address, so
        // demanding one — as `done` does — would make it unsendable.
        assert!(failed.get("address").is_none(), "{failed}");
    }

    #[test]
    fn a_scan_window_is_clamped_and_zero_means_list_without_touching_the_radio() {
        assert_eq!(discovery_window(0), Duration::ZERO);
        assert_eq!(discovery_window(5), Duration::from_secs(5));
        assert_eq!(
            discovery_window(DISCOVERY_DEFAULT_SECONDS),
            Duration::from_secs(DISCOVERY_DEFAULT_SECONDS)
        );
        assert_eq!(
            discovery_window(u64::MAX),
            Duration::from_secs(DISCOVERY_MAX_SECONDS)
        );
    }

    #[test]
    fn the_printed_device_shape_is_the_one_jwm_parses() {
        let json = ScannedDevice {
            address: ADDR.to_string(),
            name: "Studio Headphones".to_string(),
            paired: true,
            connected: false,
            rssi: Some(-40),
        }
        .to_json();
        assert_eq!(json["address"], ADDR);
        assert_eq!(json["name"], "Studio Headphones");
        assert_eq!(json["paired"], true);
        assert_eq!(json["connected"], false);
        assert_eq!(json["rssi"], -40);
        // An unheard device reports a null rather than a fabricated floor.
        let quiet = ScannedDevice {
            address: ADDR.to_string(),
            name: ADDR.to_string(),
            paired: true,
            connected: false,
            rssi: None,
        }
        .to_json();
        assert!(quiet["rssi"].is_null());
    }

    #[test]
    fn prompt_args_carry_each_kind() {
        let pin = prompt_args(ADDR, COOKIE, "", 1, &PromptRequest::Pin);
        assert_eq!(pin["kind"], "pin");
        assert_eq!(pin["address"], ADDR);
        assert_eq!(pin["cookie"], COOKIE);
        assert!(pin.get("passkey").is_none());
        assert!(pin.get("code").is_none());
        // An outbound session's name came off the picker row, so the field
        // is absent rather than an empty string jwm would have to ignore.
        assert!(pin.get("device_name").is_none());

        let confirm = prompt_args(ADDR, COOKIE, "", 2, &PromptRequest::Confirm { passkey: 42 });
        assert_eq!(confirm["kind"], "confirm");
        assert_eq!(confirm["passkey"], 42);

        let display = prompt_args(
            ADDR,
            COOKIE,
            "",
            3,
            &PromptRequest::Display {
                code: "1234".to_string(),
            },
        );
        assert_eq!(display["kind"], "display");
        assert_eq!(display["code"], "1234");
    }

    #[test]
    fn authorization_args_distinguish_a_bond_request_from_a_service_request() {
        // RequestAuthorization: something wants to bond. No service field at
        // all, so jwm reads its presence rather than its value.
        let bond = prompt_args(
            ADDR,
            COOKIE,
            "MX Master 3S",
            4,
            &PromptRequest::Authorize { service: None },
        );
        assert_eq!(bond["kind"], "authorize");
        assert_eq!(bond["address"], ADDR);
        assert!(bond["service"].is_null());
        // An inbound window has no name until it asks bluez for one, so the
        // name travels with the prompt instead of coming off a picker row.
        assert_eq!(bond["device_name"], "MX Master 3S");

        // AuthorizeService: a bonded device wants one profile.
        let service = prompt_args(
            ADDR,
            COOKIE,
            "",
            5,
            &PromptRequest::Authorize {
                service: Some("0000110B-0000-1000-8000-00805F9B34FB".to_string()),
            },
        );
        assert_eq!(service["kind"], "authorize");
        assert_eq!(service["service"], "0000110B-0000-1000-8000-00805F9B34FB");
    }

    #[test]
    fn an_inbound_window_only_serves_the_session_jwm_opened_for_it() {
        let window = serde_json::json!({
            "active": true, "address": null, "cookie": COOKIE,
            "state": "working", "kind": "inbound",
        });
        assert!(inbound_session_matches(&window, COOKIE));
        assert!(!inbound_session_matches(&window, "other-cookie"));

        // An outbound pairing session must never be served by an inbound
        // agent: that is exactly how a 90s one-device window would become a
        // standing invitation.
        let pairing = serde_json::json!({
            "active": true, "address": ADDR, "cookie": COOKIE,
            "state": "working", "kind": "outbound",
        });
        assert!(!inbound_session_matches(&pairing, COOKIE));

        // Nothing live, and a helper too old to say which direction it is.
        assert!(!inbound_session_matches(
            &serde_json::json!({"active": false, "cookie": COOKIE, "kind": "inbound"}),
            COOKIE
        ));
        assert!(!inbound_session_matches(
            &serde_json::json!({"active": true, "cookie": COOKIE}),
            COOKIE
        ));
    }

    #[test]
    fn a_late_bound_target_still_admits_exactly_one_device() {
        let shared = |target: Option<&str>, accepts_inbound: bool| Shared {
            target: Mutex::new(target.map(str::to_string)),
            cookie: COOKIE.to_string(),
            accepts_inbound,
            pending: Mutex::new(None),
            next_request_id: std::sync::atomic::AtomicU64::new(1),
            ended_by_user: AtomicBool::new(false),
            authorized: AtomicBool::new(false),
            ipc: JwmIpc::with_socket(PathBuf::from("/nonexistent/jwm-ipc.sock")),
        };

        // An outbound session is bound before it starts and never moves.
        let outbound = shared(Some(ADDR), false);
        assert!(outbound.bind_target(ADDR));
        assert!(!outbound.bind_target("11:22:33:44:55:66"));
        assert_eq!(outbound.target().as_deref(), Some(ADDR));

        // An inbound window binds on the first caller — and from then on
        // behaves exactly like an outbound one.
        let inbound = shared(None, true);
        assert_eq!(inbound.target(), None);
        assert!(inbound.bind_target("11:22:33:44:55:66"));
        assert_eq!(inbound.target().as_deref(), Some("11:22:33:44:55:66"));
        assert!(
            !inbound.bind_target(ADDR),
            "a second device must not be able to take over an armed window"
        );
    }

    #[test]
    fn done_args_carry_the_outcome() {
        let done = done_args(ADDR, COOKIE, true, None, Some(true));
        assert_eq!(done["ok"], true);
        assert!(done["error"].is_null());
        assert_eq!(done["connected"], true);
        // Paired, but the profile connection did not come up: still a
        // successful pairing, and the picker says so.
        let done = done_args(ADDR, COOKIE, true, None, Some(false));
        assert_eq!(done["ok"], true);
        assert_eq!(done["connected"], false);
        // A failed pairing has nothing to connect, so the verdict is absent
        // rather than a false "not connected".
        let done = done_args(ADDR, COOKIE, false, Some("pairing cancelled"), None);
        assert_eq!(done["ok"], false);
        assert_eq!(done["error"], "pairing cancelled");
        assert!(done["connected"].is_null());
    }

    #[test]
    fn withdraw_args_name_the_cancelled_request_when_one_was_pending() {
        let withdraw = withdraw_args(ADDR, COOKIE, Some(7));
        assert_eq!(withdraw["address"], ADDR);
        assert_eq!(withdraw["cookie"], COOKIE);
        assert_eq!(withdraw["request_id"], 7);

        // A cancel with nothing outstanding still reports — a display-only
        // prompt on the panel has no pending request to name — and null,
        // not an absent field, is what jwm reads as "whatever is showing",
        // exactly like `done`'s null `error`/`connected`.
        let bare = withdraw_args(ADDR, COOKIE, None);
        assert_eq!(bare["address"], ADDR);
        assert!(bare["request_id"].is_null());
        assert!(bare.get("request_id").is_some());
    }

    #[test]
    fn the_connect_borrows_only_what_the_wall_clock_has_left() {
        // A prompt-free pairing leaves the whole budget.
        assert_eq!(connect_budget(Duration::ZERO), Some(CONNECT_WAIT));
        assert_eq!(
            connect_budget(Duration::from_secs(30)),
            Some(CONNECT_WAIT),
            "still far from the wall clock"
        );
        // Close to the wall clock the budget shrinks, always keeping the
        // report slack for the `done` frame.
        assert_eq!(
            connect_budget(WALL_CLOCK - REPORT_SLACK - Duration::from_secs(4)),
            Some(Duration::from_secs(4))
        );
        // Under the floor, and past the wall clock, there is no connect.
        assert_eq!(
            connect_budget(WALL_CLOCK - REPORT_SLACK - Duration::from_millis(500)),
            None
        );
        assert_eq!(connect_budget(WALL_CLOCK), None);
        assert_eq!(connect_budget(WALL_CLOCK + Duration::from_secs(60)), None);
    }

    #[test]
    fn response_events_parse_every_answer_shape() {
        let pin = parse_response_event(&serde_json::json!({
            "event": "bluetooth/pairing_response",
            "payload": {"cookie": COOKIE, "accepted": true, "pin": "4321"},
        }))
        .expect("pin answer");
        assert_eq!(pin.reply, UserReply::Pin("4321".to_string()));

        let confirmed = parse_response_event(&serde_json::json!({
            "event": "bluetooth/pairing_response",
            "payload": {"cookie": COOKIE, "accepted": true},
        }))
        .expect("confirm answer");
        assert_eq!(confirmed.reply, UserReply::Confirmed);

        let rejected = parse_response_event(&serde_json::json!({
            "event": "bluetooth/pairing_response",
            "payload": {"cookie": COOKIE, "accepted": false, "reason": "rejected"},
        }))
        .expect("reject answer");
        assert_eq!(rejected.reply, UserReply::Rejected);

        let cancelled = parse_response_event(&serde_json::json!({
            "event": "bluetooth/pairing_response",
            "payload": {"cookie": COOKIE, "accepted": false},
        }))
        .expect("cancel answer");
        assert_eq!(cancelled.reply, UserReply::Cancelled);

        // `reason: "closed"` is the one answer that ends an inbound window,
        // as opposed to withdrawing a single prompt. It is otherwise only
        // exercised by the bus-backed integration tests, which skip when
        // dbus-daemon is absent, so pin the wire mapping here where nothing
        // can skip it.
        let closed = parse_response_event(&serde_json::json!({
            "event": "bluetooth/pairing_response",
            "payload": {"cookie": COOKIE, "accepted": false, "reason": "closed"},
        }))
        .expect("closed answer");
        assert_eq!(closed.reply, UserReply::SessionClosed);
        assert!(closed.reply.ends_the_session());
        assert!(closed.reply.is_user_termination());
    }

    #[test]
    fn malformed_or_foreign_events_are_ignored() {
        for event in [
            serde_json::json!({"event": "bluetooth/status", "payload": {}}),
            serde_json::json!({"event": "bluetooth/pairing_response"}),
            serde_json::json!({"event": "bluetooth/pairing_response", "payload": {"accepted": true}}),
            serde_json::json!({"event": "bluetooth/pairing_response", "payload": {"cookie": COOKIE}}),
            serde_json::json!({"event": "bluetooth/pairing_response", "payload": {"cookie": COOKIE, "accepted": "yes"}}),
            // An empty PIN is not an answer.
            serde_json::json!({"event": "bluetooth/pairing_response", "payload": {"cookie": COOKIE, "accepted": true, "pin": ""}}),
            serde_json::json!({"payload": {"cookie": COOKIE, "accepted": true}}),
        ] {
            assert!(
                parse_response_event(&event).is_none(),
                "accepted malformed event: {event}"
            );
        }
    }

    #[test]
    fn liveness_matches_only_the_exact_session() {
        let active = serde_json::json!({
            "active": true, "address": ADDR, "cookie": COOKIE, "state": "working",
        });
        assert!(session_matches(&active, ADDR, COOKIE));
        assert!(session_matches(&active, &ADDR.to_lowercase(), COOKIE));
        assert!(!session_matches(&active, ADDR, "other-cookie"));
        assert!(!session_matches(
            &serde_json::json!({"active": false}),
            ADDR,
            COOKIE
        ));
        assert!(!session_matches(&serde_json::json!(null), ADDR, COOKIE));
    }

    // --- Integration tests over a private bus and a fake jwm ---

    /// Whether a runner has demanded the bus-backed tests actually run, the
    /// way `JWM_REQUIRE_HEADLESS_GL` guards the GL tests. When set (and not
    /// "0") a missing dbus-daemon is a hard failure, not a silent skip, so CI
    /// cannot go green on the fourteen integration tests that are the only
    /// executed coverage of the `SessionClosed`/timeout-restore paths.
    fn dbus_daemon_required() -> bool {
        std::env::var_os("JWM_REQUIRE_DBUS_DAEMON").is_some_and(|value| value != "0")
    }

    /// A private `dbus-daemon` session bus, or `None` on machines without one
    /// (the tests then skip rather than fail, unless `JWM_REQUIRE_DBUS_DAEMON`
    /// turns the skip into a failure).
    fn spawn_private_bus() -> Option<(String, std::process::Child)> {
        let require = dbus_daemon_required();
        let mut child = match std::process::Command::new("dbus-daemon")
            .args(["--session", "--print-address=1", "--nofork"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                assert!(
                    !require,
                    "JWM_REQUIRE_DBUS_DAEMON is set but dbus-daemon could not be spawned: {error}"
                );
                return None;
            }
        };
        // Every remaining way out of this function is also a skip, so each
        // one answers to the same demand: a guard that only covers the
        // easiest failure is a guard CI can still walk around.
        let Some(stdout) = child.stdout.take() else {
            assert!(
                !require,
                "JWM_REQUIRE_DBUS_DAEMON is set but dbus-daemon's stdout was not captured"
            );
            return None;
        };
        let mut address = String::new();
        if BufReader::new(stdout).read_line(&mut address).is_err() {
            assert!(
                !require,
                "JWM_REQUIRE_DBUS_DAEMON is set but dbus-daemon's bus address could not be read"
            );
            return None;
        }
        let address = address.trim().to_string();
        if address.is_empty() {
            assert!(
                !require,
                "JWM_REQUIRE_DBUS_DAEMON is set but dbus-daemon printed no bus address"
            );
            return None;
        }
        Some((address, child))
    }

    #[derive(Default)]
    struct FakeBluezState {
        agent: Mutex<Option<(String, String)>>,
        capability: Mutex<Option<String>>,
        default_agent: Mutex<Option<String>>,
        unregistered: Mutex<Option<String>>,
        pair_calls: Mutex<usize>,
        cancel_pairing_calls: Mutex<usize>,
        agent_error_seen: Mutex<Option<String>>,
        connect_calls: Mutex<usize>,
        trusted: Mutex<bool>,
        /// When set, `Connect` refuses, standing in for a device that bonded
        /// and then walked out of range.
        connect_fails: Mutex<bool>,
        /// The adapter flags an inbound window has to turn on. Seeded to
        /// what a default session really has (both false, verified against
        /// the live controller) so the restore path is exercised.
        pairable: Mutex<bool>,
        discoverable: Mutex<bool>,
        pairable_timeout: Mutex<u32>,
        discoverable_timeout: Mutex<u32>,
        /// When set, `Discoverable` refuses to be written, standing in for an
        /// adapter that cannot be made reachable — a window nothing can ring.
        discoverable_fails: Mutex<bool>,
    }

    impl FakeBluezState {
        fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
            mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    struct FakeAgentManager {
        state: Arc<FakeBluezState>,
    }

    #[interface(name = "org.bluez.AgentManager1")]
    impl FakeAgentManager {
        async fn register_agent(
            &mut self,
            path: OwnedObjectPath,
            capability: String,
            #[zbus(header)] header: zbus::message::Header<'_>,
        ) -> zbus::fdo::Result<()> {
            let sender = header
                .sender()
                .map(|name| name.as_str().to_string())
                .unwrap_or_default();
            *FakeBluezState::lock(&self.state.agent) = Some((sender, path.to_string()));
            *FakeBluezState::lock(&self.state.capability) = Some(capability);
            Ok(())
        }

        async fn unregister_agent(&mut self, path: OwnedObjectPath) -> zbus::fdo::Result<()> {
            *FakeBluezState::lock(&self.state.unregistered) = Some(path.to_string());
            Ok(())
        }

        async fn request_default_agent(&mut self, path: OwnedObjectPath) -> zbus::fdo::Result<()> {
            *FakeBluezState::lock(&self.state.default_agent) = Some(path.to_string());
            Ok(())
        }
    }

    struct FakeAdapter {
        state: Arc<FakeBluezState>,
    }

    #[interface(name = "org.bluez.Adapter1")]
    impl FakeAdapter {
        #[zbus(property)]
        fn powered(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn pairable(&self) -> bool {
            *FakeBluezState::lock(&self.state.pairable)
        }

        #[zbus(property)]
        fn set_pairable(&self, value: bool) {
            *FakeBluezState::lock(&self.state.pairable) = value;
        }

        #[zbus(property)]
        fn discoverable(&self) -> bool {
            *FakeBluezState::lock(&self.state.discoverable)
        }

        #[zbus(property)]
        fn set_discoverable(&self, value: bool) -> zbus::Result<()> {
            if *FakeBluezState::lock(&self.state.discoverable_fails) {
                return Err(zbus::Error::FDO(Box::new(zbus::fdo::Error::Failed(
                    "adapter refused".into(),
                ))));
            }
            *FakeBluezState::lock(&self.state.discoverable) = value;
            Ok(())
        }

        /// BlueZ's own countdown. The window sets it so a helper that is
        /// killed outright still cannot leave the controller open.
        #[zbus(property)]
        fn pairable_timeout(&self) -> u32 {
            *FakeBluezState::lock(&self.state.pairable_timeout)
        }

        #[zbus(property)]
        fn set_pairable_timeout(&self, value: u32) {
            *FakeBluezState::lock(&self.state.pairable_timeout) = value;
        }

        #[zbus(property)]
        fn discoverable_timeout(&self) -> u32 {
            *FakeBluezState::lock(&self.state.discoverable_timeout)
        }

        #[zbus(property)]
        fn set_discoverable_timeout(&self, value: u32) {
            *FakeBluezState::lock(&self.state.discoverable_timeout) = value;
        }
    }

    /// How the fake device's `Pair` behaves.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PairScript {
        /// Ask the agent to confirm `passkey`, succeed when it agrees.
        Confirm(u32),
        /// Sit until CancelPairing arrives, then fail.
        UntilCancelled,
    }

    struct FakeDevice {
        state: Arc<FakeBluezState>,
        connection: Connection,
        device_path: OwnedObjectPath,
        script: PairScript,
    }

    #[interface(name = "org.bluez.Device1")]
    impl FakeDevice {
        /// The property the helper's device lookup keys on.
        #[zbus(property)]
        fn address(&self) -> String {
            ADDR.to_string()
        }

        async fn pair(&self) -> zbus::fdo::Result<()> {
            *FakeBluezState::lock(&self.state.pair_calls) += 1;
            let agent = FakeBluezState::lock(&self.state.agent).clone();
            let Some((destination, path)) = agent else {
                return Err(zbus::fdo::Error::Failed("no agent registered".into()));
            };
            match self.script {
                PairScript::Confirm(passkey) => {
                    let reply = self
                        .connection
                        .call_method(
                            Some(destination.as_str()),
                            path,
                            Some(<PairingAgent as zbus::object_server::Interface>::name()),
                            "RequestConfirmation",
                            &(self.device_path.clone(), passkey),
                        )
                        .await;
                    match reply {
                        Ok(_) => Ok(()),
                        Err(error) => {
                            *FakeBluezState::lock(&self.state.agent_error_seen) =
                                Some(error.to_string());
                            Err(zbus::fdo::Error::Failed("pairing failed".into()))
                        }
                    }
                }
                PairScript::UntilCancelled => {
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    loop {
                        if *FakeBluezState::lock(&self.state.cancel_pairing_calls) > 0 {
                            return Err(zbus::fdo::Error::Failed(
                                "org.bluez.Error.Canceled".into(),
                            ));
                        }
                        if std::time::Instant::now() >= deadline {
                            return Err(zbus::fdo::Error::Failed(
                                "fake device was never cancelled".into(),
                            ));
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        }

        async fn cancel_pairing(&self) -> zbus::fdo::Result<()> {
            *FakeBluezState::lock(&self.state.cancel_pairing_calls) += 1;
            Ok(())
        }

        /// The post-pair auto-connect. A bonded device that cannot bring its
        /// profiles up is the interesting case, so it is scriptable.
        async fn connect(&self) -> zbus::fdo::Result<()> {
            *FakeBluezState::lock(&self.state.connect_calls) += 1;
            if *FakeBluezState::lock(&self.state.connect_fails) {
                return Err(zbus::fdo::Error::Failed(
                    "org.bluez.Error.Failed: br-connection-profile-unavailable".into(),
                ));
            }
            Ok(())
        }

        /// Writable so the helper's `Trusted = true` lands somewhere the test
        /// can read it back.
        #[zbus(property)]
        fn trusted(&self) -> bool {
            *FakeBluezState::lock(&self.state.trusted)
        }

        #[zbus(property)]
        fn set_trusted(&self, value: bool) {
            *FakeBluezState::lock(&self.state.trusted) = value;
        }
    }

    /// A fake jwm on a private Unix socket: records every command/query frame
    /// and hands the subscription connection back to the test for injecting
    /// events. `session_json` is what `get_bluetooth_pairing` answers.
    struct FakeJwm {
        dir: PathBuf,
        socket: PathBuf,
        requests: std::sync::mpsc::Receiver<Value>,
        /// Frames that arrived while a different command was being awaited.
        /// Sends that race each other (a withdraw and the done from the Pair
        /// unwind it sets off) may land in either order, and a frame dropped
        /// for arriving at the wrong wait never comes back.
        backlog: std::cell::RefCell<Vec<Value>>,
        subscription: Arc<Mutex<Option<UnixStream>>>,
    }

    impl FakeJwm {
        fn start(session_json: Value) -> FakeJwm {
            let dir = std::env::temp_dir().join(format!(
                "jwm-bridge-pairing-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|since| since.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("test directory");
            let socket = dir.join("jwm-ipc.sock");
            let listener = UnixListener::bind(&socket).expect("bind fake jwm socket");
            let (tx, requests) = std::sync::mpsc::channel();
            let subscription = Arc::new(Mutex::new(None));
            let slot = subscription.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else {
                        continue;
                    };
                    let Ok(clone) = stream.try_clone() else {
                        continue;
                    };
                    let mut line = String::new();
                    if BufReader::new(clone).read_line(&mut line).is_err() || line.is_empty() {
                        continue;
                    }
                    let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
                        continue;
                    };
                    if frame.get("subscribe").is_some() {
                        let _ = stream.write_all(b"{\"success\":true}\n");
                        let _ = stream.flush();
                        *FakeBluezState::lock(&slot) = Some(stream);
                        continue;
                    }
                    let is_query = frame.get("query").is_some();
                    let _ = tx.send(frame);
                    let response = if is_query {
                        serde_json::json!({ "success": true, "data": session_json })
                    } else {
                        serde_json::json!({ "success": true, "data": null })
                    };
                    let _ = writeln!(stream, "{response}");
                    let _ = stream.flush();
                }
            });
            FakeJwm {
                dir,
                socket,
                requests,
                backlog: std::cell::RefCell::new(Vec::new()),
                subscription,
            }
        }

        /// Wait for the helper's event subscription to come up.
        fn await_subscription(&self) {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if FakeBluezState::lock(&self.subscription).is_some() {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the helper never subscribed to jwm events"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn send_response(&self, payload: Value) {
            self.await_subscription();
            let mut guard = FakeBluezState::lock(&self.subscription);
            let stream = guard.as_mut().expect("subscription stream");
            let frame = serde_json::json!({
                "event": "bluetooth/pairing_response",
                "payload": payload,
            });
            writeln!(stream, "{frame}").expect("write response event");
            stream.flush().expect("flush response event");
        }

        /// Answer the prompt frame `prompt`, echoing the request id it
        /// carried. Answers are bound to the request they answer, so a test
        /// that forgets the id is a test that does not resolve anything —
        /// which is the property being protected.
        fn answer(&self, prompt: &Value, mut payload: Value) {
            let request_id = prompt["args"]["request_id"].clone();
            assert!(
                request_id.is_u64(),
                "every prompt carries the id its answer must name: {prompt}"
            );
            payload
                .as_object_mut()
                .expect("answer payload is an object")
                .insert("request_id".to_string(), request_id);
            self.send_response(payload);
        }

        fn recv_command(&self, name: &str) -> Value {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            loop {
                // The stash is searched first: a frame that arrived while a
                // different command was awaited is still a valid answer to
                // this wait.
                if let Some(at) =
                    self.backlog.borrow_mut().iter().position(|frame| {
                        frame.get("command").and_then(Value::as_str) == Some(name)
                    })
                {
                    return self.backlog.borrow_mut().remove(at);
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                assert!(!remaining.is_zero(), "no {name} command within 15s");
                let frame = self
                    .requests
                    .recv_timeout(remaining)
                    .expect("IPC request from the helper");
                if frame.get("command").and_then(Value::as_str) == Some(name) {
                    return frame;
                }
                // Queries (the liveness self-heal) and racing commands are
                // not what we wait for; stash them for later waits.
                self.backlog.borrow_mut().push(frame);
            }
        }
    }

    impl Drop for FakeJwm {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Bring up a private bus with a fake org.bluez, and return everything
    /// the test needs. `None` when dbus-daemon is unavailable.
    async fn fake_setup(script: PairScript) -> Option<FakeSetup> {
        let _ =
            env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug"))
                .is_test(true)
                .try_init();
        let (address, mut bus) = spawn_private_bus()?;
        // A dead-on-arrival daemon reads as a skipped test too — unless a
        // runner demanded the bus, in which case it is a failure. A `wait`
        // that itself fails is treated as death, so it cannot become a
        // silent third way out.
        if !matches!(bus.try_wait(), Ok(None)) {
            assert!(
                !dbus_daemon_required(),
                "JWM_REQUIRE_DBUS_DAEMON is set but dbus-daemon exited on startup"
            );
            return None;
        }
        let state = Arc::new(FakeBluezState::default());
        let connection = zbus::connection::Builder::address(address.as_str())
            .expect("private bus address")
            .name(BLUEZ)
            .expect("org.bluez name")
            .build()
            .await
            .expect("connect to the private bus");
        let server = connection.object_server();
        server
            .at("/", zbus::fdo::ObjectManager)
            .await
            .expect("serve ObjectManager");
        server
            .at(
                "/org/bluez",
                FakeAgentManager {
                    state: state.clone(),
                },
            )
            .await
            .expect("serve AgentManager1");
        server
            .at(
                ADAPTER_PATH,
                FakeAdapter {
                    state: state.clone(),
                },
            )
            .await
            .expect("serve Adapter1");
        server
            .at(
                DEVICE_PATH,
                FakeDevice {
                    state: state.clone(),
                    connection: connection.clone(),
                    device_path: OwnedObjectPath::try_from(DEVICE_PATH).expect("device path"),
                    script,
                },
            )
            .await
            .expect("serve Device1");
        Some(FakeSetup {
            bus_address: address,
            bus,
            state,
            _bluez: connection,
        })
    }

    struct FakeSetup {
        bus_address: String,
        bus: std::process::Child,
        state: Arc<FakeBluezState>,
        _bluez: Connection,
    }

    impl Drop for FakeSetup {
        fn drop(&mut self) {
            let _ = self.bus.kill();
        }
    }

    async fn helper_connection(bus_address: &str) -> Connection {
        zbus::connection::Builder::address(bus_address)
            .expect("private bus address")
            .build()
            .await
            .expect("helper connection")
    }

    /// Wait for the helper to register its agent, then return where it
    /// lives so the fake bluez can call into it the way bluetoothd would.
    async fn await_registered_agent(state: &Arc<FakeBluezState>) -> (String, String) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(agent) = FakeBluezState::lock(&state.agent).clone() {
                return agent;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the helper never registered an agent"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Wait until the window has made the controller reachable. Without
    /// both flags nothing can ask to bond in the first place, so this is
    /// part of the feature working at all, not a nicety.
    async fn await_adapter_exposed(state: &Arc<FakeBluezState>) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if *FakeBluezState::lock(&state.pairable) && *FakeBluezState::lock(&state.discoverable)
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the window never made the controller pairable and discoverable"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Call an agent callback the way bluetoothd would for an unsolicited
    /// request: no `Pair`, no prior conversation, just a device ringing.
    async fn call_agent(
        connection: &Connection,
        agent: &(String, String),
        method: &str,
        body: &(impl zbus::export::serde::Serialize + zbus::zvariant::DynamicType),
    ) -> zbus::Result<zbus::message::Message> {
        let (destination, path) = agent;
        connection
            .call_method(
                Some(destination.as_str()),
                path.as_str(),
                Some(<PairingAgent as zbus::object_server::Interface>::name()),
                method,
                body,
            )
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_armed_window_lets_the_user_allow_an_incoming_request() {
        let Some(setup) = fake_setup(PairScript::Confirm(1)).await else {
            eprintln!("dbus-daemon unavailable; skipping the inbound integration test");
            return;
        };
        let jwm = FakeJwm::start(serde_json::json!({
            "active": true, "address": null, "cookie": COOKIE,
            "state": "working", "kind": "inbound",
        }));
        let ipc = JwmIpc::with_socket(jwm.socket.clone());
        let connection = helper_connection(&setup.bus_address).await;
        let bluez = helper_connection(&setup.bus_address).await;

        // Seed the adapter-wide timeouts to a real default session's values
        // (Pairable forever = 0, Discoverable = 180) so the restore path is
        // exercised: the window must lower them and put them back, not leave
        // the machine's discoverable window permanently cut to sixty seconds.
        *FakeBluezState::lock(&setup.state.discoverable_timeout) = 180;

        let session = tokio::spawn(accept_session(ipc, connection, COOKIE));
        let agent = await_registered_agent(&setup.state).await;
        // The window advertises itself, or nothing could ever ring it.
        assert_eq!(
            FakeBluezState::lock(&setup.state.default_agent).as_deref(),
            Some(INBOUND_AGENT_PATH)
        );
        await_adapter_exposed(&setup.state).await;
        // While the window is open both countdowns are at the window length —
        // the belt a helper killed outright leaves behind. `open` writes them
        // before it raises the flags, so waiting on the flags is enough. The
        // restore asserted at the end would pass just as well if nothing had
        // ever been lowered, so pin the lowering here.
        let window = INBOUND_WINDOW.as_secs() as u32;
        assert_eq!(*FakeBluezState::lock(&setup.state.pairable_timeout), window);
        assert_eq!(
            *FakeBluezState::lock(&setup.state.discoverable_timeout),
            window
        );

        let device = OwnedObjectPath::try_from(DEVICE_PATH).expect("device path");
        let call = tokio::spawn({
            let bluez = bluez.clone();
            let device = device.clone();
            async move { call_agent(&bluez, &agent, "RequestAuthorization", &(device,)).await }
        });

        // jwm is asked, and told which device and what kind of request.
        let prompt = jwm.recv_command("bluetooth_pairing_prompt");
        assert_eq!(prompt["args"]["kind"], "authorize");
        assert_eq!(prompt["args"]["address"], ADDR);
        assert_eq!(prompt["args"]["cookie"], COOKIE);
        assert!(prompt["args"]["service"].is_null(), "a bond request");

        jwm.answer(
            &prompt,
            serde_json::json!({"cookie": COOKIE, "accepted": true}),
        );
        assert!(
            call.await.expect("agent call task").is_ok(),
            "an allowed request returns success to bluez"
        );

        // Closing the window is its own answer shape: a prompt merely
        // withdrawn leaves the window armed, so the two cannot be the same
        // payload.
        jwm.send_response(
            serde_json::json!({"cookie": COOKIE, "accepted": false, "reason": "closed"}),
        );

        // Closing the window after the user allowed the bond is not a
        // refusal: the teardown report says the request was granted, so jwm
        // can refresh the list rather than flash "refused" for a device that
        // paired.
        let done = jwm.recv_command("bluetooth_pairing_done");
        assert_eq!(done["args"]["ok"], true);
        assert!(done["args"]["error"].is_null());

        let code = session.await.expect("session task");
        assert_eq!(code, EXIT_CANCELLED);
        assert_eq!(
            FakeBluezState::lock(&setup.state.unregistered).as_deref(),
            Some(INBOUND_AGENT_PATH)
        );
        // And the controller is put back, rather than left discoverable
        // behind the user.
        assert!(!*FakeBluezState::lock(&setup.state.pairable));
        assert!(!*FakeBluezState::lock(&setup.state.discoverable));
        // The adapter-wide countdowns are restored to what the window found,
        // so one jwm window does not permanently shorten every other tool's
        // discoverable/pairable window (Pairable back to 0 = forever,
        // Discoverable back to 180). A killed helper — which never reaches
        // this teardown — still leaves them at the window as the belt.
        assert_eq!(*FakeBluezState::lock(&setup.state.pairable_timeout), 0);
        assert_eq!(
            *FakeBluezState::lock(&setup.state.discoverable_timeout),
            180
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_window_the_adapter_refuses_is_handed_back_instead_of_held_open() {
        let Some(setup) = fake_setup(PairScript::Confirm(1)).await else {
            eprintln!("dbus-daemon unavailable; skipping the inbound integration test");
            return;
        };
        let jwm = FakeJwm::start(serde_json::json!({
            "active": true, "address": null, "cookie": COOKIE,
            "state": "working", "kind": "inbound",
        }));
        let ipc = JwmIpc::with_socket(jwm.socket.clone());
        let connection = helper_connection(&setup.bus_address).await;

        // The controller refuses to become discoverable: the window would be
        // armed, on screen, counting down, and findable by nothing. The
        // helper used to hold it for the full sixty seconds and say nothing.
        *FakeBluezState::lock(&setup.state.discoverable_fails) = true;

        let started = std::time::Instant::now();
        let session = tokio::spawn(accept_session(ipc, connection, COOKIE));

        // The report names the session by cookie and carries no address —
        // nothing rang this window, so there is no device to name, which is
        // exactly why this is not a `done`.
        let failed = jwm.recv_command("bluetooth_pairing_failed");
        assert_eq!(failed["args"]["cookie"], COOKIE);
        assert!(
            failed["args"]["error"]
                .as_str()
                .is_some_and(|error| error.contains("discoverable")),
            "{failed}"
        );
        assert!(failed["args"].get("address").is_none(), "{failed}");

        let code = session.await.expect("session task");
        assert_eq!(code, EXIT_FAILED);
        assert!(
            started.elapsed() < INBOUND_WINDOW,
            "the window was handed back, not waited out"
        );

        // And it unwound through the same teardown a normal close uses,
        // rather than returning early past it: the agent is unregistered and
        // the one flag that did take is off again.
        assert_eq!(
            FakeBluezState::lock(&setup.state.unregistered).as_deref(),
            Some(INBOUND_AGENT_PATH)
        );
        assert!(!*FakeBluezState::lock(&setup.state.pairable));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closing_the_window_from_a_live_prompt_puts_the_controller_back() {
        let Some(setup) = fake_setup(PairScript::Confirm(1)).await else {
            eprintln!("dbus-daemon unavailable; skipping the inbound integration test");
            return;
        };
        let jwm = FakeJwm::start(serde_json::json!({
            "active": true, "address": null, "cookie": COOKIE,
            "state": "working", "kind": "inbound",
        }));
        let ipc = JwmIpc::with_socket(jwm.socket.clone());
        let connection = helper_connection(&setup.bus_address).await;
        let bluez = helper_connection(&setup.bus_address).await;

        let session = tokio::spawn(accept_session(ipc, connection, COOKIE));
        let agent = await_registered_agent(&setup.state).await;
        await_adapter_exposed(&setup.state).await;

        let device = OwnedObjectPath::try_from(DEVICE_PATH).expect("device path");
        let call = tokio::spawn({
            let bluez = bluez.clone();
            async move { call_agent(&bluez, &agent, "RequestAuthorization", &(device,)).await }
        });
        let prompt = jwm.recv_command("bluetooth_pairing_prompt");

        // Esc on an authorization prompt closes the window *with a request
        // outstanding* — the common case, not the rare one. Answering it
        // must also end the session: jwm drops its record and tells the user
        // the window is shut, and a helper that kept looping would hold the
        // controller pairable and discoverable, and bluez's default agent
        // registration, for the rest of the sixty seconds.
        jwm.answer(
            &prompt,
            serde_json::json!({"cookie": COOKIE, "accepted": false, "reason": "closed"}),
        );
        assert!(
            call.await.expect("agent call task").is_err(),
            "the outstanding request still fails for bluez"
        );

        assert_eq!(session.await.expect("session task"), EXIT_CANCELLED);
        assert!(!*FakeBluezState::lock(&setup.state.pairable));
        assert!(!*FakeBluezState::lock(&setup.state.discoverable));
        assert_eq!(
            FakeBluezState::lock(&setup.state.unregistered).as_deref(),
            Some(INBOUND_AGENT_PATH)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_armed_window_refuses_a_second_device_and_an_unanswered_service() {
        let Some(setup) = fake_setup(PairScript::Confirm(1)).await else {
            eprintln!("dbus-daemon unavailable; skipping the inbound integration test");
            return;
        };
        let jwm = FakeJwm::start(serde_json::json!({
            "active": true, "address": null, "cookie": COOKIE,
            "state": "working", "kind": "inbound",
        }));
        let ipc = JwmIpc::with_socket(jwm.socket.clone());
        let connection = helper_connection(&setup.bus_address).await;
        let bluez = helper_connection(&setup.bus_address).await;

        let session = tokio::spawn(accept_session(ipc, connection, COOKIE));
        let agent = await_registered_agent(&setup.state).await;

        // First caller binds the window, and is refused by the user.
        let device = OwnedObjectPath::try_from(DEVICE_PATH).expect("device path");
        let first = tokio::spawn({
            let bluez = bluez.clone();
            let agent = agent.clone();
            let device = device.clone();
            async move {
                call_agent(
                    &bluez,
                    &agent,
                    "AuthorizeService",
                    &(device, "0000110B-0000-1000-8000-00805F9B34FB".to_string()),
                )
                .await
            }
        });
        let prompt = jwm.recv_command("bluetooth_pairing_prompt");
        assert_eq!(prompt["args"]["kind"], "authorize");
        assert_eq!(
            prompt["args"]["service"],
            "0000110B-0000-1000-8000-00805F9B34FB"
        );
        jwm.answer(
            &prompt,
            serde_json::json!({"cookie": COOKIE, "accepted": false, "reason": "rejected"}),
        );
        assert!(
            first.await.expect("agent call task").is_err(),
            "a refused request must fail for bluez, not silently succeed"
        );

        // A different device may not take over the window the first one bound.
        let other = OwnedObjectPath::try_from("/org/bluez/hci0/dev_11_22_33_44_55_66")
            .expect("device path");
        assert!(
            call_agent(&bluez, &agent, "RequestAuthorization", &(other,))
                .await
                .is_err(),
            "one window answers for one device"
        );

        session.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pairing_agent_still_refuses_everything_inbound() {
        let Some(setup) = fake_setup(PairScript::UntilCancelled).await else {
            eprintln!("dbus-daemon unavailable; skipping the pairing integration test");
            return;
        };
        let jwm = FakeJwm::start(serde_json::json!({
            "active": true, "address": ADDR, "cookie": COOKIE, "state": "working",
        }));
        let ipc = JwmIpc::with_socket(jwm.socket.clone());
        let connection = helper_connection(&setup.bus_address).await;
        let bluez = helper_connection(&setup.bus_address).await;

        let session = tokio::spawn(pair_session(ipc, connection, ADDR, COOKIE));
        let agent = await_registered_agent(&setup.state).await;

        // The pairing agent holds the default registration for its whole
        // session. Anything that rings during that window — even the very
        // device being paired — is refused without asking the user, because
        // the user armed a pairing, not an open door.
        let device = OwnedObjectPath::try_from(DEVICE_PATH).expect("device path");
        assert!(
            call_agent(&bluez, &agent, "RequestAuthorization", &(device.clone(),))
                .await
                .is_err()
        );
        assert!(
            call_agent(
                &bluez,
                &agent,
                "AuthorizeService",
                &(device, "0000110B-0000-1000-8000-00805F9B34FB".to_string())
            )
            .await
            .is_err()
        );

        session.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_full_confirm_handshake_pairs_the_device() {
        let Some(setup) = fake_setup(PairScript::Confirm(123_456)).await else {
            eprintln!("dbus-daemon unavailable; skipping the pairing integration test");
            return;
        };
        let jwm = FakeJwm::start(serde_json::json!({
            "active": true, "address": ADDR, "cookie": COOKIE, "state": "working",
        }));
        let ipc = JwmIpc::with_socket(jwm.socket.clone());
        let connection = helper_connection(&setup.bus_address).await;

        let session = tokio::spawn(pair_session(ipc, connection, ADDR, COOKIE));

        let prompt = jwm.recv_command("bluetooth_pairing_prompt");
        assert_eq!(prompt["command"], "bluetooth_pairing_prompt");
        assert_eq!(prompt["args"]["address"], ADDR);
        assert_eq!(prompt["args"]["cookie"], COOKIE);
        assert_eq!(prompt["args"]["kind"], "confirm");
        assert_eq!(prompt["args"]["passkey"], 123_456);

        jwm.answer(
            &prompt,
            serde_json::json!({"cookie": COOKIE, "accepted": true}),
        );

        let done = jwm.recv_command("bluetooth_pairing_done");
        assert_eq!(done["command"], "bluetooth_pairing_done");
        assert_eq!(done["args"]["ok"], true);
        // v2: the bond is followed through to a usable device.
        assert_eq!(done["args"]["connected"], true);

        let code = session.await.expect("session task");
        assert_eq!(code, EXIT_PAIRED);
        assert!(
            *FakeBluezState::lock(&setup.state.trusted),
            "a device the user chose is trusted, so its services do not need a second answer"
        );
        assert_eq!(*FakeBluezState::lock(&setup.state.connect_calls), 1);
        assert_eq!(
            FakeBluezState::lock(&setup.state.capability).as_deref(),
            Some("KeyboardDisplay")
        );
        assert_eq!(
            FakeBluezState::lock(&setup.state.default_agent).as_deref(),
            Some(AGENT_PATH)
        );
        assert_eq!(
            FakeBluezState::lock(&setup.state.unregistered).as_deref(),
            Some(AGENT_PATH)
        );
        assert_eq!(*FakeBluezState::lock(&setup.state.pair_calls), 1);
        assert_eq!(*FakeBluezState::lock(&setup.state.cancel_pairing_calls), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_device_that_bonds_but_will_not_connect_still_counts_as_paired() {
        let Some(setup) = fake_setup(PairScript::Confirm(123_456)).await else {
            eprintln!("dbus-daemon unavailable; skipping the pairing integration test");
            return;
        };
        *FakeBluezState::lock(&setup.state.connect_fails) = true;
        let jwm = FakeJwm::start(serde_json::json!({
            "active": true, "address": ADDR, "cookie": COOKIE, "state": "working",
        }));
        let ipc = JwmIpc::with_socket(jwm.socket.clone());
        let connection = helper_connection(&setup.bus_address).await;

        let session = tokio::spawn(pair_session(ipc, connection, ADDR, COOKIE));

        let prompt = jwm.recv_command("bluetooth_pairing_prompt");
        jwm.answer(
            &prompt,
            serde_json::json!({"cookie": COOKIE, "accepted": true}),
        );

        let done = jwm.recv_command("bluetooth_pairing_done");
        // The bond is durable and the user asked for it; a profile that would
        // not come up is reported, never converted into a failed pairing.
        assert_eq!(done["args"]["ok"], true);
        assert!(done["args"]["error"].is_null());
        assert_eq!(done["args"]["connected"], false);

        assert_eq!(session.await.expect("session task"), EXIT_PAIRED);
        assert_eq!(*FakeBluezState::lock(&setup.state.connect_calls), 1);
        assert!(*FakeBluezState::lock(&setup.state.trusted));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rejected_confirmation_fails_pairing_with_the_bluez_error_name() {
        let Some(setup) = fake_setup(PairScript::Confirm(654_321)).await else {
            eprintln!("dbus-daemon unavailable; skipping the pairing integration test");
            return;
        };
        let jwm = FakeJwm::start(serde_json::json!({
            "active": true, "address": ADDR, "cookie": COOKIE, "state": "working",
        }));
        let ipc = JwmIpc::with_socket(jwm.socket.clone());
        let connection = helper_connection(&setup.bus_address).await;

        let session = tokio::spawn(pair_session(ipc, connection, ADDR, COOKIE));

        let prompt = jwm.recv_command("bluetooth_pairing_prompt");
        assert_eq!(prompt["args"]["kind"], "confirm");
        assert_eq!(prompt["args"]["passkey"], 654_321);

        jwm.answer(
            &prompt,
            serde_json::json!({"cookie": COOKIE, "accepted": false, "reason": "rejected"}),
        );

        let done = jwm.recv_command("bluetooth_pairing_done");
        assert_eq!(done["command"], "bluetooth_pairing_done");
        assert_eq!(done["args"]["ok"], false);
        assert_eq!(done["args"]["error"], "pairing cancelled");

        let code = session.await.expect("session task");
        assert_eq!(code, EXIT_CANCELLED);
        // The agent's answer reached bluez under its real error name, not the
        // generic D-Bus failure name zbus would otherwise send.
        let seen = FakeBluezState::lock(&setup.state.agent_error_seen).clone();
        assert!(
            seen.as_deref()
                .is_some_and(|error| error.contains("org.bluez.Error.Rejected")),
            "expected org.bluez.Error.Rejected, got {seen:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancel_with_nothing_pending_cancels_the_pairing_itself() {
        let Some(setup) = fake_setup(PairScript::UntilCancelled).await else {
            eprintln!("dbus-daemon unavailable; skipping the pairing integration test");
            return;
        };
        let jwm = FakeJwm::start(serde_json::json!({
            "active": true, "address": ADDR, "cookie": COOKIE, "state": "working",
        }));
        let ipc = JwmIpc::with_socket(jwm.socket.clone());
        let connection = helper_connection(&setup.bus_address).await;

        let session = tokio::spawn(pair_session(ipc, connection, ADDR, COOKIE));

        // The user bails before bluez asks anything (e.g. Esc on "Pairing…").
        jwm.send_response(serde_json::json!({
            "cookie": COOKIE, "accepted": false, "reason": "cancelled",
        }));

        let done = jwm.recv_command("bluetooth_pairing_done");
        assert_eq!(done["command"], "bluetooth_pairing_done");
        assert_eq!(done["args"]["ok"], false);

        let code = session.await.expect("session task");
        assert_eq!(code, EXIT_CANCELLED);
        assert_eq!(
            *FakeBluezState::lock(&setup.state.cancel_pairing_calls),
            1,
            "a bare cancel must reach bluez as CancelPairing"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_bluez_cancels_withdraws_the_prompt_from_the_panel() {
        let Some(setup) = fake_setup(PairScript::Confirm(123_456)).await else {
            eprintln!("dbus-daemon unavailable; skipping the pairing integration test");
            return;
        };
        let jwm = FakeJwm::start(serde_json::json!({
            "active": true, "address": ADDR, "cookie": COOKIE, "state": "working",
        }));
        let ipc = JwmIpc::with_socket(jwm.socket.clone());
        let connection = helper_connection(&setup.bus_address).await;
        let bluez = helper_connection(&setup.bus_address).await;

        let session = tokio::spawn(pair_session(ipc, connection, ADDR, COOKIE));
        let agent = await_registered_agent(&setup.state).await;

        // BlueZ asked; the prompt is on the panel.
        let prompt = jwm.recv_command("bluetooth_pairing_prompt");
        assert_eq!(prompt["args"]["kind"], "confirm");

        // The remote side gives up before the user answers, so bluez
        // withdraws the request. The prompt must come down now, not when
        // jwm's own prompt timeout notices it has gone unanswered.
        let device = OwnedObjectPath::try_from(DEVICE_PATH).expect("device path");
        call_agent(&bluez, &agent, "Cancel", &(device,))
            .await
            .expect("Cancel returns unit");
        let withdraw = jwm.recv_command("bluetooth_pairing_withdraw");
        assert_eq!(withdraw["command"], "bluetooth_pairing_withdraw");
        assert_eq!(withdraw["args"]["address"], ADDR);
        assert_eq!(withdraw["args"]["cookie"], COOKIE);
        assert_eq!(
            withdraw["args"]["request_id"], prompt["args"]["request_id"],
            "the withdraw names the request that raised the prompt"
        );

        // The cancelled request unwinds the Pair, and the session reports its
        // outcome as usual — the withdraw replaced no part of that path. A
        // cancel bluez initiated is a failure, not a user cancel: no answer
        // broadcast ever set `ended_by_user`.
        let done = jwm.recv_command("bluetooth_pairing_done");
        assert_eq!(done["args"]["ok"], false);
        assert_eq!(session.await.expect("session task"), EXIT_FAILED);
        let seen = FakeBluezState::lock(&setup.state.agent_error_seen).clone();
        assert!(
            seen.as_deref()
                .is_some_and(|error| error.contains("org.bluez.Error.Canceled")),
            "expected org.bluez.Error.Canceled, got {seen:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_inbound_request_withdraws_the_prompt_and_the_window_lives_on() {
        let Some(setup) = fake_setup(PairScript::Confirm(1)).await else {
            eprintln!("dbus-daemon unavailable; skipping the inbound integration test");
            return;
        };
        let jwm = FakeJwm::start(serde_json::json!({
            "active": true, "address": null, "cookie": COOKIE,
            "state": "working", "kind": "inbound",
        }));
        let ipc = JwmIpc::with_socket(jwm.socket.clone());
        let connection = helper_connection(&setup.bus_address).await;
        let bluez = helper_connection(&setup.bus_address).await;

        let session = tokio::spawn(accept_session(ipc, connection, COOKIE));
        let agent = await_registered_agent(&setup.state).await;
        await_adapter_exposed(&setup.state).await;

        let device = OwnedObjectPath::try_from(DEVICE_PATH).expect("device path");
        let call = tokio::spawn({
            let bluez = bluez.clone();
            let device = device.clone();
            let agent = agent.clone();
            async move { call_agent(&bluez, &agent, "RequestAuthorization", &(device,)).await }
        });
        let prompt = jwm.recv_command("bluetooth_pairing_prompt");
        assert_eq!(prompt["args"]["kind"], "authorize");

        // The device gives up before the user answers: bluez cancels the
        // request. The prompt it raised comes down, but the window was the
        // user's gesture — it stays armed for whatever rings next.
        call_agent(&bluez, &agent, "Cancel", &(device,))
            .await
            .expect("Cancel returns unit");
        let withdraw = jwm.recv_command("bluetooth_pairing_withdraw");
        assert_eq!(withdraw["args"]["address"], ADDR);
        assert_eq!(withdraw["args"]["cookie"], COOKIE);
        assert_eq!(withdraw["args"]["request_id"], prompt["args"]["request_id"]);
        assert!(
            call.await.expect("agent call task").is_err(),
            "a cancelled request still fails for bluez"
        );

        // The window really did live on: it still takes jwm's close to end
        // it, exactly as if nothing had ever been asked.
        jwm.send_response(serde_json::json!({
            "cookie": COOKIE, "accepted": false, "reason": "closed",
        }));
        let done = jwm.recv_command("bluetooth_pairing_done");
        assert_eq!(done["args"]["ok"], false);
        assert_eq!(session.await.expect("session task"), EXIT_CANCELLED);
        assert!(!*FakeBluezState::lock(&setup.state.pairable));
        assert!(!*FakeBluezState::lock(&setup.state.discoverable));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancel_with_nothing_pending_still_reports_the_withdrawal() {
        let Some(setup) = fake_setup(PairScript::UntilCancelled).await else {
            eprintln!("dbus-daemon unavailable; skipping the pairing integration test");
            return;
        };
        let jwm = FakeJwm::start(serde_json::json!({
            "active": true, "address": ADDR, "cookie": COOKIE, "state": "working",
        }));
        let ipc = JwmIpc::with_socket(jwm.socket.clone());
        let connection = helper_connection(&setup.bus_address).await;
        let bluez = helper_connection(&setup.bus_address).await;

        let session = tokio::spawn(pair_session(ipc, connection, ADDR, COOKIE));
        let agent = await_registered_agent(&setup.state).await;

        // No request was ever outstanding — the panel is showing "Pairing…",
        // not a prompt. The withdraw still reports, naming no request: jwm
        // no-ops on no match, and a display-only prompt would still come
        // down, which is the case this frame exists for.
        let device = OwnedObjectPath::try_from(DEVICE_PATH).expect("device path");
        call_agent(&bluez, &agent, "Cancel", &(device,))
            .await
            .expect("Cancel returns unit");
        let withdraw = jwm.recv_command("bluetooth_pairing_withdraw");
        assert_eq!(withdraw["args"]["cookie"], COOKIE);
        assert!(
            withdraw["args"]["request_id"].is_null(),
            "nothing pending, so no request to name: {withdraw}"
        );

        // The pairing itself is untouched by it; a bare cancel from jwm still
        // ends the session the usual way.
        jwm.send_response(serde_json::json!({
            "cookie": COOKIE, "accepted": false, "reason": "cancelled",
        }));
        let done = jwm.recv_command("bluetooth_pairing_done");
        assert_eq!(done["args"]["ok"], false);
        assert_eq!(session.await.expect("session task"), EXIT_CANCELLED);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_session_jwm_no_longer_holds_never_touches_bluez() {
        let Some(setup) = fake_setup(PairScript::Confirm(123_456)).await else {
            eprintln!("dbus-daemon unavailable; skipping the pairing integration test");
            return;
        };
        // jwm already forgot the session (picker closed between spawn and
        // startup): the helper must exit before registering anything.
        let jwm = FakeJwm::start(serde_json::json!({ "active": false }));
        let ipc = JwmIpc::with_socket(jwm.socket.clone());
        let connection = helper_connection(&setup.bus_address).await;

        let code = pair_session(ipc, connection, ADDR, COOKIE).await;
        assert_eq!(code, EXIT_CANCELLED);
        assert!(FakeBluezState::lock(&setup.state.agent).is_none());
        assert_eq!(*FakeBluezState::lock(&setup.state.pair_calls), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn responses_with_a_foreign_cookie_are_dropped() {
        let Some(setup) = fake_setup(PairScript::Confirm(999_999)).await else {
            eprintln!("dbus-daemon unavailable; skipping the pairing integration test");
            return;
        };
        let jwm = FakeJwm::start(serde_json::json!({
            "active": true, "address": ADDR, "cookie": COOKIE, "state": "working",
        }));
        let ipc = JwmIpc::with_socket(jwm.socket.clone());
        let connection = helper_connection(&setup.bus_address).await;

        let session = tokio::spawn(pair_session(ipc, connection, ADDR, COOKIE));

        let prompt = jwm.recv_command("bluetooth_pairing_prompt");
        assert_eq!(prompt["args"]["kind"], "confirm");

        // A stale answer from a dead session must not resolve the prompt,
        // and neither must one that names no request or the wrong one — the
        // cookie says which session, only the id says which question.
        jwm.answer(
            &prompt,
            serde_json::json!({"cookie": "deadbeefdeadbeef", "accepted": true}),
        );
        jwm.send_response(serde_json::json!({
            "cookie": COOKIE, "accepted": true, "request_id": 99_999,
        }));
        jwm.answer(
            &prompt,
            serde_json::json!({"cookie": COOKIE, "accepted": true}),
        );

        let done = jwm.recv_command("bluetooth_pairing_done");
        assert_eq!(done["args"]["ok"], true);
        let code = session.await.expect("session task");
        assert_eq!(code, EXIT_PAIRED);
    }
}
