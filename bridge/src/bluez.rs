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

/// The strongest capability jwm's picker can honor: it can show a passkey and
/// read a typed PIN back.
pub const CAPABILITY: &str = "KeyboardDisplay";

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
}

/// The user's answer, as parsed from a `bluetooth/pairing_response` event.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UserReply {
    Pin(String),
    Confirmed,
    Rejected,
    Cancelled,
}

impl UserReply {
    /// Whether this answer ends the pairing from the user's side — used to
    /// classify the session's exit code after `Pair` unwinds.
    fn is_user_termination(&self) -> bool {
        matches!(self, Self::Rejected | Self::Cancelled)
    }
}

/// State shared between the agent callbacks and the response pump.
struct Shared {
    /// Target device address, uppercase; callbacks for anything else are
    /// rejected before they can touch the session.
    target: String,
    cookie: String,
    /// The one request bluez has outstanding. BlueZ serializes agent
    /// requests per `Pair` call; a replacement means the first was withdrawn.
    pending: Mutex<Option<oneshot::Sender<UserReply>>>,
    /// Set when a user-side rejection/cancel was seen, so the exit code says
    /// "cancelled" rather than reporting bluez's resulting error as failure.
    ended_by_user: AtomicBool,
    ipc: JwmIpc,
}

impl Shared {
    fn lock_pending(&self) -> std::sync::MutexGuard<'_, Option<oneshot::Sender<UserReply>>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct PairingAgent {
    shared: Arc<Shared>,
}

impl PairingAgent {
    fn device_matches(&self, device: &OwnedObjectPath) -> bool {
        address_from_device_path(device.as_str())
            .is_some_and(|address| address == self.shared.target)
    }

    /// Relay a display-only callback to jwm's picker. Nothing comes back; a
    /// cancel while this is showing arrives as a response event with no
    /// pending request and turns into `CancelPairing` in the pump.
    async fn tell_jwm(&self, prompt: PromptRequest) {
        let args = prompt_args(&self.shared.target, &self.shared.cookie, &prompt);
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
        let (tx, rx) = oneshot::channel();
        *self.shared.lock_pending() = Some(tx);

        let args = prompt_args(&self.shared.target, &self.shared.cookie, &prompt);
        let ipc = self.shared.ipc.clone();
        let sent =
            tokio::task::spawn_blocking(move || ipc.command("bluetooth_pairing_prompt", args))
                .await;
        match sent {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                let _ = self.shared.lock_pending().take();
                return Err(AgentError::Canceled(format!(
                    "jwm did not accept the pairing prompt: {error}"
                )));
            }
            Err(error) => {
                let _ = self.shared.lock_pending().take();
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
                let _ = self.shared.lock_pending().take();
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
            UserReply::Cancelled => Err(AgentError::Canceled("PIN prompt cancelled".to_string())),
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
            UserReply::Cancelled => {
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
            UserReply::Cancelled => Err(AgentError::Canceled(
                "confirmation prompt cancelled".to_string(),
            )),
        }
    }

    async fn request_authorization(&self, device: OwnedObjectPath) -> Result<(), AgentError> {
        log::warn!(
            "rejecting inbound authorization request for {}",
            device.as_str()
        );
        Err(AgentError::Rejected(
            "this agent only pairs the device it was started for".to_string(),
        ))
    }

    async fn authorize_service(
        &self,
        device: OwnedObjectPath,
        _uuid: String,
    ) -> Result<(), AgentError> {
        log::warn!("rejecting service authorization for {}", device.as_str());
        Err(AgentError::Rejected(
            "this agent does not authorize services".to_string(),
        ))
    }

    fn cancel(&self, device: OwnedObjectPath) {
        if !self.device_matches(&device) {
            return;
        }
        // BlueZ withdrew the outstanding request: the user can no longer
        // answer it, so the pending callback resolves as cancelled and the
        // Pair call unwinds.
        if let Some(reply) = self.shared.lock_pending().take() {
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

/// Build the `bluetooth_pairing_prompt` command arguments. The passkey/code
/// a prompt carries is display material, never a secret to keep off the wire
/// — but it is never logged either.
fn prompt_args(address: &str, cookie: &str, prompt: &PromptRequest) -> Value {
    match prompt {
        PromptRequest::Pin => serde_json::json!({
            "address": address,
            "cookie": cookie,
            "kind": "pin",
        }),
        PromptRequest::Confirm { passkey } => serde_json::json!({
            "address": address,
            "cookie": cookie,
            "kind": "confirm",
            "passkey": passkey,
        }),
        PromptRequest::Display { code } => serde_json::json!({
            "address": address,
            "cookie": cookie,
            "kind": "display",
            "code": code,
        }),
    }
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
            _ => UserReply::Cancelled,
        }
    };
    Some(ResponseEvent { cookie, reply })
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

// ---------------------------------------------------------------------------
// Session driver
// ---------------------------------------------------------------------------

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
        let pending = shared.lock_pending().take();
        match pending {
            Some(reply) => {
                let _ = reply.send(response.reply);
            }
            None => {
                if response.reply.is_user_termination() {
                    log::info!("cancel with no request outstanding: CancelPairing");
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
        target: target.clone(),
        cookie: cookie.to_string(),
        pending: Mutex::new(None),
        ended_by_user: AtomicBool::new(false),
        ipc: ipc.clone(),
    });
    if let Err(error) = connection
        .object_server()
        .at(
            AGENT_PATH,
            PairingAgent {
                shared: shared.clone(),
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
        if devices.len() >= MAX_DISCOVERED_DEVICES {
            break;
        }
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
        let name = managed_string(properties, "Alias")
            .or_else(|| managed_string(properties, "Name"))
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| address.clone());
        let name: String = name.chars().take(MAX_DEVICE_NAME_CHARS).collect();
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

/// Entry point for `jwm-bridge pair`: connect the system bus and run one
/// session under the hard wall clock.
pub async fn run(address: &str, cookie: &str) -> i32 {
    if !is_valid_address(&address.to_uppercase()) {
        log::warn!("pair: {address:?} is not a Bluetooth address");
        return EXIT_USAGE;
    }
    let ipc = JwmIpc::new();
    let connection = match zbus::connection::Builder::system() {
        Ok(builder) => match builder.build().await {
            Ok(connection) => connection,
            Err(error) => {
                log::warn!("pair {address}: cannot reach the system bus: {error}");
                return EXIT_ERROR;
            }
        },
        Err(error) => {
            log::warn!("pair {address}: no system bus address: {error}");
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
        let borrowed: Vec<ManagedEntry<'_>> = properties
            .iter()
            .map(|(path, props)| (*path, DEVICE_IFACE, props.as_slice()))
            .collect();
        let devices = devices_from_managed_objects(&managed_tree(&borrowed));
        assert_eq!(devices.len(), MAX_DISCOVERED_DEVICES);
        assert!(
            devices
                .iter()
                .all(|device| device.name.chars().count() <= MAX_DEVICE_NAME_CHARS)
        );
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
        let pin = prompt_args(ADDR, COOKIE, &PromptRequest::Pin);
        assert_eq!(pin["kind"], "pin");
        assert_eq!(pin["address"], ADDR);
        assert_eq!(pin["cookie"], COOKIE);
        assert!(pin.get("passkey").is_none());
        assert!(pin.get("code").is_none());

        let confirm = prompt_args(ADDR, COOKIE, &PromptRequest::Confirm { passkey: 42 });
        assert_eq!(confirm["kind"], "confirm");
        assert_eq!(confirm["passkey"], 42);

        let display = prompt_args(
            ADDR,
            COOKIE,
            &PromptRequest::Display {
                code: "1234".to_string(),
            },
        );
        assert_eq!(display["kind"], "display");
        assert_eq!(display["code"], "1234");
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

    /// A private `dbus-daemon` session bus, or `None` on machines without one
    /// (the tests then skip rather than fail).
    fn spawn_private_bus() -> Option<(String, std::process::Child)> {
        let mut child = std::process::Command::new("dbus-daemon")
            .args(["--session", "--print-address=1", "--nofork"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        let mut address = String::new();
        BufReader::new(stdout).read_line(&mut address).ok()?;
        let address = address.trim().to_string();
        if address.is_empty() {
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

        fn recv_command(&self, name: &str) -> Value {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                assert!(!remaining.is_zero(), "no {name} command within 15s");
                let frame = self
                    .requests
                    .recv_timeout(remaining)
                    .expect("IPC request from the helper");
                if frame.get("command").and_then(Value::as_str) == Some(name) {
                    return frame;
                }
                // Queries (the liveness self-heal) are not what we wait for.
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
        // A dead-on-arrival daemon reads as a skipped test too.
        if bus.try_wait().ok()?.is_some() {
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

        jwm.send_response(serde_json::json!({"cookie": COOKIE, "accepted": true}));

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

        jwm.recv_command("bluetooth_pairing_prompt");
        jwm.send_response(serde_json::json!({"cookie": COOKIE, "accepted": true}));

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

        jwm.send_response(serde_json::json!({
            "cookie": COOKIE, "accepted": false, "reason": "rejected",
        }));

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

        // A stale answer from a dead session must not resolve the prompt.
        jwm.send_response(serde_json::json!({"cookie": "deadbeefdeadbeef", "accepted": true}));
        jwm.send_response(serde_json::json!({"cookie": COOKIE, "accepted": true}));

        let done = jwm.recv_command("bluetooth_pairing_done");
        assert_eq!(done["args"]["ok"], true);
        let code = session.await.expect("session task");
        assert_eq!(code, EXIT_PAIRED);
    }
}
