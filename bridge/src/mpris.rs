//! MPRIS player tracking, pushed into jwm's shell.
//!
//! The compositor has no bus connection, so this module is the eyes and hands
//! of its media row: it watches every `org.mpris.MediaPlayer2.*` name on the
//! session bus, pushes the *active* player's state in over IPC, and turns
//! jwm's `media/command` broadcasts back into method calls. When several
//! players are on the bus the push also names them all, and jwm can pin the
//! row to one of them with a `select_player` command; the pin holds while
//! its bus name is alive, survives bridge restart via
//! `$XDG_STATE_HOME/jwm/mpris-pin` (else `~/.local/state/jwm/mpris-pin`),
//! and clears itself when the player goes away while another is still on
//! the bus. An empty sweep keeps the pin so a later launch of the same
//! player — or a session that starts the bridge before any player — can
//! reclaim it.
//!
//! Player selection and metadata extraction are pure functions so the rules
//! (a playing player outranks a paused one; `xesam:artist` is a list) are unit
//! tested without a bus.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{StreamExt, stream};
use serde_json::Value;
use zbus::Connection;
use zbus::fdo::DBusProxy;
use zbus::names::{BusName, OwnedBusName};
use zvariant::OwnedValue;

use crate::jwm_ipc::JwmIpc;

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const PLAYER_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const ROOT_INTERFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_CALL_TIMEOUT: Duration = Duration::from_secs(1);
const SWEEP_TIMEOUT: Duration = Duration::from_secs(3);
const SNAPSHOT_CONCURRENCY: usize = 8;

/// One player's state as read off the bus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayerSnapshot {
    /// Bus suffix after `org.mpris.MediaPlayer2.`, e.g. `spotify`.
    pub player: String,
    pub identity: String,
    pub status: String,
    pub title: String,
    pub artist: String,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    /// Whether the player reports `CanSeek` — the shell only offers a
    /// clickable position zone when this is true and both counters exist.
    pub can_seek: bool,
    /// The `Position` property in microseconds. It is a live counter, not
    /// metadata, and players only emit `Seeked` for it — so it is read on the
    /// same sweep as everything else. `None` when the player did not report
    /// one; the value is passed through as-is, garbage included, and the
    /// display side decides what is sane to show.
    pub position_us: Option<i64>,
    /// `mpris:length` from the track metadata, microseconds. `None` when the
    /// track does not say — streams routinely do not.
    pub length_us: Option<i64>,
    /// `mpris:trackid` object path, needed for absolute `SetPosition`. Kept
    /// bridge-side only — jwm never sees it.
    pub track_id: Option<String>,
    /// Resource and process identity come from this exact unique bus owner,
    /// never from the shell's requested transport command.
    pub track_url: Option<String>,
    pub bus_owner: Option<String>,
    pub process_id: Option<u32>,
    pub queued_urls: Vec<String>,
    /// Native command receipts stay separate from ordinary media publication.
    pub eva_open_receipts: Vec<Value>,
}

impl PlayerSnapshot {
    fn rank(&self) -> u8 {
        match self.status.as_str() {
            "Playing" => 2,
            "Paused" => 1,
            _ => 0,
        }
    }

    fn to_args(&self) -> Value {
        serde_json::json!({
            "player": self.player,
            "identity": self.identity,
            "status": self.status,
            "title": self.title,
            "artist": self.artist,
            "can_go_next": self.can_go_next,
            "can_go_previous": self.can_go_previous,
            // Append-only wire fields: an old jwm ignores them, and a new jwm
            // reads a missing key (old bridge) as `None` / false.
            "can_seek": self.can_seek,
            "position_us": self.position_us,
            "length_us": self.length_us,
            "track_id": self.track_id,
            "track_url": self.track_url,
            "bus_owner": self.bus_owner,
            "process_id": self.process_id,
            "queued_urls": self.queued_urls,
        })
    }
}

/// Pick the player the shell should show: playing beats paused beats stopped,
/// and ties keep the earlier entry so the choice does not flap between two
/// idle players on every poll.
#[must_use]
pub fn pick_active(players: &[PlayerSnapshot]) -> Option<&PlayerSnapshot> {
    // Not `max_by_key`: that keeps the *last* maximum, so two idle players
    // would swap places whenever the bus reordered its name list.
    let mut best: Option<&PlayerSnapshot> = None;
    for player in players {
        if best.is_none_or(|current| player.rank() > current.rank()) {
            best = Some(player);
        }
    }
    best
}

/// Which player one sweep publishes and transport commands target: the
/// pinned one while its bus name is still alive, else the ordinary active
/// pick. The second return is the pin to keep — a pin that named nobody
/// alive while other players remain comes back cleared, so a player that
/// quit cannot hold the row. An empty sweep keeps the pin: nobody on the
/// bus is not the same as "this name is gone", and a restart (or a later
/// launch of the same player) must be able to reclaim it.
fn resolve_active<'a>(
    players: &'a [PlayerSnapshot],
    pinned: Option<&str>,
) -> (Option<&'a PlayerSnapshot>, Option<String>) {
    if let Some(pin) = pinned {
        if let Some(player) = players.iter().find(|player| player.player == pin) {
            return (Some(player), Some(pin.to_string()));
        }
        if players.is_empty() {
            return (None, Some(pin.to_string()));
        }
    }
    (pick_active(players), None)
}

/// The `set_media_status` payload for one sweep: the resolved player's
/// state, plus every bus suffix the sweep saw in sweep order, so jwm can
/// offer player switching without a second channel. The list is an
/// append-only key — an old jwm ignores it, and a new jwm reading a missing
/// one (an old bridge) treats the session as single-player. `player_details`
/// rides alongside with Identity and PlaybackStatus so the Players picker
/// can label rows without scraping the active push; an old jwm ignores it
/// the same way, and a new jwm without it still keys/cycles on `players`.
fn publish_args(players: &[PlayerSnapshot], pinned: Option<&str>) -> (Value, Option<String>) {
    let (active, pin) = resolve_active(players, pinned);
    let args = match active {
        Some(active) => {
            let mut args = active.to_args();
            args["players"] = players
                .iter()
                .map(|player| Value::String(player.player.clone()))
                .collect();
            // Append-only rich rows: same sweep order as `players`. Old jwm
            // ignores the key; the string list stays the cycle/select key.
            args["player_details"] = players
                .iter()
                .map(|player| {
                    serde_json::json!({
                        "player": player.player,
                        "identity": player.identity,
                        "status": player.status,
                        "track_id": player.track_id,
                        "track_url": player.track_url,
                        "bus_owner": player.bus_owner,
                        "process_id": player.process_id,
                        "queued_urls": player.queued_urls,
                    })
                })
                .collect();
            args
        }
        None => serde_json::json!({ "player": Value::Null }),
    };
    (args, pin)
}

/// `xesam:title` from an MPRIS metadata dict.
#[must_use]
pub fn title_from_metadata(metadata: &HashMap<String, OwnedValue>) -> String {
    metadata
        .get("xesam:title")
        .map(string_of)
        .unwrap_or_default()
}

/// `xesam:artist` is an array of strings; players that publish a bare string
/// are tolerated because several do.
#[must_use]
pub fn artist_from_metadata(metadata: &HashMap<String, OwnedValue>) -> String {
    let Some(value) = metadata.get("xesam:artist") else {
        return String::new();
    };
    if let Ok(list) = Vec::<String>::try_from(value.clone()) {
        return list.join(", ");
    }
    string_of(value)
}

fn string_of(value: &OwnedValue) -> String {
    String::try_from(value.clone()).unwrap_or_default()
}

fn bool_of(value: &OwnedValue) -> bool {
    bool::try_from(value.clone()).unwrap_or(false)
}

/// Microseconds as the spec sends them (`x`, a signed 64-bit). Players that
/// send the value unsigned are tolerated; anything else — or an unsigned
/// value past `i64::MAX` — reads as "not reported" rather than failing the
/// whole snapshot.
fn i64_of(value: &OwnedValue) -> Option<i64> {
    if let Ok(value) = i64::try_from(value.clone()) {
        return Some(value);
    }
    u64::try_from(value.clone())
        .ok()
        .and_then(|value| i64::try_from(value).ok())
}

/// `mpris:length` from an MPRIS metadata dict, in microseconds.
#[must_use]
pub fn length_from_metadata(metadata: &HashMap<String, OwnedValue>) -> Option<i64> {
    metadata.get("mpris:length").and_then(i64_of)
}

/// `mpris:trackid` from an MPRIS metadata dict — the object path
/// `SetPosition` needs. Players that publish a bare string are tolerated.
#[must_use]
pub fn track_id_from_metadata(metadata: &HashMap<String, OwnedValue>) -> Option<String> {
    let value = metadata.get("mpris:trackid")?;
    if let Ok(path) = zvariant::ObjectPath::try_from(value.clone()) {
        let path = path.as_str();
        return (!path.is_empty()).then(|| path.to_string());
    }
    let path = string_of(value);
    (!path.is_empty()).then_some(path)
}

/// Read one player's properties. A player that disappears mid-read yields
/// `None` rather than failing the whole sweep.
async fn snapshot(connection: &Connection, name: &OwnedBusName) -> Option<PlayerSnapshot> {
    let suffix = name.as_str().strip_prefix(MPRIS_PREFIX)?.to_string();
    let bus = DBusProxy::new(connection).await.ok()?;
    let owner = bus.get_name_owner(name.clone().into()).await.ok()?;
    let process_id = if bus
        .get_connection_unix_user(owner.clone().into())
        .await
        .ok()?
        == unsafe { libc::geteuid() }
    {
        bus.get_connection_unix_process_id(owner.clone().into())
            .await
            .ok()
    } else {
        None
    };
    let properties = zbus::fdo::PropertiesProxy::builder(connection)
        .destination(owner.clone())
        .ok()?
        .path(PLAYER_PATH)
        .ok()?
        .build()
        .await
        .ok()?;

    let player = properties
        .get_all(PLAYER_INTERFACE.try_into().ok()?)
        .await
        .ok()?;
    let metadata: HashMap<String, OwnedValue> = player
        .get("Metadata")
        .and_then(|value| HashMap::try_from(value.clone()).ok())
        .unwrap_or_default();
    let identity = properties
        .get(ROOT_INTERFACE.try_into().ok()?, "Identity")
        .await
        .ok()
        .map(|value| String::try_from(value).unwrap_or_default())
        .unwrap_or_default();
    let queued_urls = if suffix == "jwm" {
        properties
            .get("org.jwm.Player".try_into().ok()?, "QueuedUrls")
            .await
            .ok()
            .and_then(|value| Vec::<String>::try_from(value).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let eva_open_receipts = if suffix == "jwm" && process_id.is_some() {
        properties
            .get("org.jwm.Player".try_into().ok()?, "EvaOpenReceipts")
            .await
            .ok()
            .and_then(|value| Vec::<String>::try_from(value).ok())
            .unwrap_or_default()
            .into_iter()
            .take(64)
            .filter(|value| value.len() <= 32768)
            .filter_map(|value| serde_json::from_str::<Value>(&value).ok())
            .filter(Value::is_object)
            .collect()
    } else {
        Vec::new()
    };

    Some(PlayerSnapshot {
        player: suffix,
        identity,
        status: player
            .get("PlaybackStatus")
            .map(string_of)
            .unwrap_or_default(),
        title: title_from_metadata(&metadata),
        artist: artist_from_metadata(&metadata),
        can_go_next: player.get("CanGoNext").is_some_and(bool_of),
        can_go_previous: player.get("CanGoPrevious").is_some_and(bool_of),
        can_seek: player.get("CanSeek").is_some_and(bool_of),
        // `Position` rides the same GetAll as everything else: it does not
        // emit PropertiesChanged reliably, so the sweep's re-read is the
        // update mechanism — a separate subscription would add per-player
        // proxies for nothing.
        position_us: player.get("Position").and_then(i64_of),
        length_us: length_from_metadata(&metadata),
        track_id: track_id_from_metadata(&metadata),
        track_url: metadata
            .get("xesam:url")
            .map(string_of)
            .filter(|url| !url.is_empty()),
        bus_owner: Some(owner.to_string()),
        process_id,
        queued_urls,
        eva_open_receipts,
    })
}

struct PlayerSweep {
    players: Vec<PlayerSnapshot>,
    live: Vec<String>,
}

impl PlayerSweep {
    fn resolve<'a>(&'a self, pinned: Option<&str>) -> (Option<&'a PlayerSnapshot>, Option<String>) {
        // A slow player still owns its bus name. Do not forget the user's
        // selection, or send their next transport command to a different app.
        if let Some(pin) = pinned
            && self.live.iter().any(|name| name == pin)
        {
            return (
                self.players.iter().find(|player| player.player == pin),
                Some(pin.to_string()),
            );
        }
        resolve_active(&self.players, pinned)
    }

    fn publication(&self, pinned: Option<&str>) -> (Value, Option<String>) {
        let (active, pin) = self.resolve(pinned);
        let mut args = match active {
            Some(active) => publish_args(&self.players, Some(&active.player)).0,
            None if pin.as_ref().is_some_and(|pin| self.live.contains(pin)) => {
                // A null active player would erase the Players picker too.
                // Publish a disabled placeholder so the user can still select
                // another healthy player, without showing stale track data.
                let mut players = self.players.clone();
                players.push(PlayerSnapshot {
                    player: pinned.unwrap_or_default().to_string(),
                    status: "Stopped".into(),
                    ..PlayerSnapshot::default()
                });
                publish_args(&players, pin.as_deref()).0
            }
            None => serde_json::json!({"player": Value::Null}),
        };
        if !args["player"].is_null() {
            args["players"] = serde_json::json!(self.live);
        }
        (args, pin)
    }
}

/// A hung player cannot serialize all of the healthy players behind its bus
/// timeout. Bound both concurrent requests and the complete sweep; dropping
/// unfinished futures also cancels their local reply waiters.
async fn collect_players(connection: &Connection) -> Option<PlayerSweep> {
    let deadline = tokio::time::Instant::now() + SWEEP_TIMEOUT;
    let names = tokio::time::timeout_at(deadline, async {
        let dbus = DBusProxy::new(connection).await.ok()?;
        dbus.list_names().await.ok()
    })
    .await
    .ok()??;
    let mut names: Vec<_> = names
        .into_iter()
        .filter(|name| name.as_str().starts_with(MPRIS_PREFIX))
        .collect();
    // ListNames ordering is not stable. Preserve a deterministic tie-break
    // regardless of bus enumeration order or which concurrent reply wins.
    names.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
    let live = names
        .iter()
        .filter_map(|name| name.as_str().strip_prefix(MPRIS_PREFIX).map(str::to_owned))
        .collect();
    let snapshots = stream::iter(names)
        .map(|name| async move {
            tokio::time::timeout(PLAYER_CALL_TIMEOUT, snapshot(connection, &name))
                .await
                .ok()
                .flatten()
        })
        .buffer_unordered(SNAPSHOT_CONCURRENCY);
    futures_util::pin_mut!(snapshots);
    let mut players = Vec::new();
    while let Ok(Some(snapshot)) = tokio::time::timeout_at(deadline, snapshots.next()).await {
        if let Some(snapshot) = snapshot {
            players.push(snapshot);
        }
    }
    players.sort_unstable_by(|left, right| left.player.cmp(&right.player));
    Some(PlayerSweep { players, live })
}

/// Sweep every MPRIS player currently on the bus and push the resolved one
/// to jwm, with the sweep's full suffix list alongside. Pushing
/// `player: null` is how "every player went away" is reported. Returns the
/// pin to keep: the pinned player while its bus name is alive, otherwise
/// the pin clears itself rather than sticking to a player that is gone. A
/// sweep that could not run at all (the bus daemon unreachable) validates
/// nothing, so the pin passes through untouched.
async fn publish(connection: &Connection, ipc: &JwmIpc, pinned: Option<&str>) -> Option<String> {
    let Some(sweep) = collect_players(connection).await else {
        return pinned.map(str::to_string);
    };
    let (args, pin) = sweep.publication(pinned);
    forward_open_receipts(ipc, &sweep.players).await;
    let ipc = ipc.clone();
    let _ = tokio::task::spawn_blocking(move || ipc.command("set_media_status", args)).await;
    pin
}

/// Forward only proofs observed on this unique owner's native interface. No
/// current playlist or ordinary player status can stand in for an operation.
fn open_receipt(player: &PlayerSnapshot, receipt: &Value) -> Option<(String, Value)> {
    if player.player != "jwm" {
        return None;
    }
    let owner = player.bus_owner.as_deref()?;
    let pid = player.process_id.filter(|pid| *pid > 0)?;
    if receipt["target_player"] != "jwm"
        || receipt["operation_id"].as_str()?.is_empty()
        || receipt["wm_instance_id"].as_str()?.is_empty()
        || receipt["sequence"].as_u64()? == 0
        || receipt["resource_uri"].as_str()?.is_empty()
        || !receipt["enqueue"].is_boolean()
        || !receipt["verified"].is_boolean()
    {
        return None;
    }
    if receipt["target_bus_owner"]
        .as_str()
        .is_some_and(|expected| expected != owner)
        || receipt["target_pid"]
            .as_u64()
            .is_some_and(|expected| expected != u64::from(pid))
    {
        return None;
    }
    let mut proof = receipt.clone();
    proof["player"] = serde_json::json!("jwm");
    proof["bus_owner"] = serde_json::json!(owner);
    proof["process_id"] = serde_json::json!(pid);
    let key = serde_json::json!([
        owner,
        pid,
        receipt["wm_instance_id"],
        receipt["operation_id"],
        receipt["sequence"]
    ])
    .to_string();
    Some((
        key,
        serde_json::json!({"assistant_completion_only":true,"assistant_media_open_completion":proof}),
    ))
}

async fn forward_open_receipts(ipc: &JwmIpc, players: &[PlayerSnapshot]) {
    use std::sync::{Mutex, OnceLock};
    static SENT: OnceLock<Mutex<std::collections::BTreeSet<String>>> = OnceLock::new();
    let sent = SENT.get_or_init(Mutex::default);
    for player in players {
        for receipt in &player.eva_open_receipts {
            let Some((key, payload)) = open_receipt(player, receipt) else {
                continue;
            };
            if sent
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&key)
            {
                continue;
            }
            let ipc = ipc.clone();
            if tokio::task::spawn_blocking(move || ipc.command("set_media_status", payload))
                .await
                .is_ok_and(|result| result.is_ok())
            {
                let mut sent = sent.lock().unwrap_or_else(|e| e.into_inner());
                if sent.len() >= 4096 {
                    sent.clear();
                }
                sent.insert(key);
            }
        }
    }
}

/// Method name on `org.mpris.MediaPlayer2.Player` for a jwm media action.
#[must_use]
pub fn method_for(action: &str) -> Option<&'static str> {
    match action {
        "play" => Some("Play"),
        "pause" => Some("Pause"),
        "play_pause" => Some("PlayPause"),
        "next" => Some("Next"),
        "previous" => Some("Previous"),
        "stop" => Some("Stop"),
        _ => None,
    }
}

/// What one `media/command` broadcast asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MediaRequest {
    /// Eva freezes both the chosen suffix and its unique owner at admission.
    TargetedTransport(TargetedTransport),
    /// Call the MPRIS method on the resolved player.
    Transport(&'static str),
    /// Pin resolution to this bus suffix; an empty name clears the pin back
    /// to the active pick.
    Select(String),
    /// Seek the resolved player to an absolute position in microseconds.
    Seek(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetedTransport {
    operation_id: String,
    wm_instance_id: String,
    player: String,
    bus_owner: String,
    pid: u32,
    action: &'static str,
    method: &'static str,
    desired_status: Option<&'static str>,
    track_id: Option<String>,
    position_us: Option<i64>,
}

/// Parse a command's payload. Unknown actions are `None`, which the caller
/// logs and drops: an old jwm and a new bridge — or the reverse — degrade
/// to ignoring each other's news rather than failing.
fn parse_request(payload: &Value) -> Option<MediaRequest> {
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // A malformed targeted command must never fall back to the dynamic
    // transport path, which could mutate a different active player.
    if [
        "assistant_operation_id",
        "wm_instance_id",
        "target_player",
        "target_bus_owner",
        "target_pid",
        "target_track_id",
    ]
    .iter()
    .any(|key| payload.get(key).is_some())
    {
        let (action, method, desired_status) = match action {
            "play" => ("play", "Play", Some("playing")),
            "pause" => ("pause", "Pause", Some("paused")),
            "stop" => ("stop", "Stop", Some("stopped")),
            "next" => ("next", "Next", None),
            "previous" => ("previous", "Previous", None),
            "play_pause" => ("play_pause", "PlayPause", None),
            "seek" => ("seek", "SetPosition", None),
            _ => return None,
        };
        let field = |key: &str| {
            payload
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty() && value.len() <= 256)
                .map(str::to_owned)
        };
        return Some(MediaRequest::TargetedTransport(TargetedTransport {
            operation_id: field("assistant_operation_id")?,
            wm_instance_id: field("wm_instance_id")?,
            player: field("target_player")?,
            bus_owner: field("target_bus_owner")?,
            pid: payload["target_pid"]
                .as_u64()
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid > 0)?,
            action,
            method,
            desired_status,
            track_id: if action == "seek" {
                Some(
                    payload["target_track_id"]
                        .as_str()
                        .filter(|id| !id.is_empty() && id.len() <= 4096)?
                        .to_owned(),
                )
            } else {
                None
            },
            position_us: if action == "seek" {
                Some(
                    payload["position_us"]
                        .as_i64()
                        .filter(|position| (0..=86_400_000_000).contains(position))?,
                )
            } else {
                None
            },
        }));
    }
    if action == "select_player" {
        let player = payload
            .get("player")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        return Some(MediaRequest::Select(player.to_string()));
    }
    if action == "seek" {
        let position = payload.get("position_us").and_then(Value::as_i64)?;
        return Some(MediaRequest::Seek(position.max(0)));
    }
    method_for(action).map(MediaRequest::Transport)
}

async fn call_active(connection: &Connection, player: &str, method: &'static str) {
    let destination = format!("{MPRIS_PREFIX}{player}");
    let Ok(bus_name) = BusName::try_from(destination.clone()) else {
        return;
    };
    match tokio::time::timeout(
        PLAYER_CALL_TIMEOUT,
        connection.call_method(
            Some(bus_name),
            PLAYER_PATH,
            Some(PLAYER_INTERFACE),
            method,
            &(),
        ),
    )
    .await
    {
        Ok(Ok(_)) => log::debug!("{method} on {destination}"),
        Ok(Err(error)) => log::warn!("{method} on {destination} failed: {error}"),
        Err(_) => log::warn!("{method} on {destination} timed out"),
    }
}

async fn targeted_state(
    properties: &zbus::fdo::PropertiesProxy<'_>,
    deadline: tokio::time::Instant,
) -> Result<Value, &'static str> {
    let values = tokio::time::timeout_at(
        deadline,
        properties.get_all(PLAYER_INTERFACE.try_into().map_err(|_| "unknown")?),
    )
    .await
    .map_err(|_| "unknown")?
    .map_err(|_| "unknown")?;
    let status = values
        .get("PlaybackStatus")
        .map(string_of)
        .filter(|status| !status.is_empty())
        .ok_or("unknown")?;
    let metadata: HashMap<String, OwnedValue> = values
        .get("Metadata")
        .and_then(|value| HashMap::try_from(value.clone()).ok())
        .unwrap_or_default();
    Ok(
        serde_json::json!({"status":status,"track_id":track_id_from_metadata(&metadata),"position_us":values.get("Position").and_then(i64_of)}),
    )
}

/// One setter, followed only by reads against the same immutable bus owner.
/// The completion bypasses the ordinary active-player row and carries its own
/// operation/WM identity, so a prior periodic sweep cannot confirm this setter.
async fn targeted_transport(connection: &Connection, request: &TargetedTransport) -> Value {
    let mut completion = serde_json::json!({
        "operation_id":request.operation_id,"wm_instance_id":request.wm_instance_id,
        "player":request.player,"bus_owner":request.bus_owner,"process_id":request.pid,"action":request.action,
        "status":Value::Null,"verified":false,
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let result = async {
        let name = OwnedBusName::try_from(format!("{MPRIS_PREFIX}{}", request.player))
            .map_err(|_| "target_gone")?;
        let owner = zbus::names::OwnedUniqueName::try_from(request.bus_owner.clone())
            .map_err(|_| "target_gone")?;
        let bus = tokio::time::timeout_at(deadline, DBusProxy::new(connection))
            .await
            .map_err(|_| "target_gone")?
            .map_err(|_| "target_gone")?;
        let current = tokio::time::timeout_at(deadline, bus.get_name_owner(name.into()))
            .await
            .map_err(|_| "target_gone")?
            .map_err(|_| "target_gone")?;
        if current != owner {
            return Err("target_gone");
        }
        let uid =
            tokio::time::timeout_at(deadline, bus.get_connection_unix_user(owner.clone().into()))
                .await
                .map_err(|_| "target_gone")?
                .map_err(|_| "target_gone")?;
        let pid = tokio::time::timeout_at(
            deadline,
            bus.get_connection_unix_process_id(owner.clone().into()),
        )
        .await
        .map_err(|_| "target_gone")?
        .map_err(|_| "target_gone")?;
        if uid != unsafe { libc::geteuid() } || pid != request.pid {
            return Err("target_gone");
        }
        let properties = zbus::fdo::PropertiesProxy::builder(connection)
            .destination(owner.clone())
            .map_err(|_| "unknown")?
            .path(PLAYER_PATH)
            .map_err(|_| "unknown")?;
        let properties = tokio::time::timeout_at(deadline, properties.build())
            .await
            .map_err(|_| "unknown")?
            .map_err(|_| "unknown")?;
        let before = targeted_state(&properties, deadline).await?;
        completion["before_status"] = before["status"].clone();
        completion["before_track_id"] = before["track_id"].clone();
        if request.action == "seek"
            && (before["track_id"].as_str() != request.track_id.as_deref()
                || request.track_id.as_deref() == Some("/org/mpris/MediaPlayer2/TrackList/NoTrack"))
        {
            return Err("target_gone");
        }
        let toggle_status = if request.action == "play_pause" {
            match before["status"].as_str() {
                Some(status) if status.eq_ignore_ascii_case("playing") => Some("paused"),
                Some(status)
                    if status.eq_ignore_ascii_case("paused")
                        || status.eq_ignore_ascii_case("stopped") =>
                {
                    Some("playing")
                }
                _ => return Err("unknown"),
            }
        } else {
            None
        };
        if tokio::time::Instant::now() >= deadline {
            return Err("unknown");
        }
        let method_deadline = deadline.min(tokio::time::Instant::now() + PLAYER_CALL_TIMEOUT);
        if request.action == "seek" {
            let track =
                zvariant::ObjectPath::try_from(request.track_id.as_deref().ok_or("target_gone")?)
                    .map_err(|_| "target_gone")?;
            tokio::time::timeout_at(
                method_deadline,
                connection.call_method(
                    Some(owner.clone()),
                    PLAYER_PATH,
                    Some(PLAYER_INTERFACE),
                    "SetPosition",
                    &(track, request.position_us.ok_or("unknown")?),
                ),
            )
            .await
            .map_err(|_| "unknown")?
            .map_err(|_| "unknown")?;
        } else {
            tokio::time::timeout_at(
                method_deadline,
                connection.call_method(
                    Some(owner.clone()),
                    PLAYER_PATH,
                    Some(PLAYER_INTERFACE),
                    request.method,
                    &(),
                ),
            )
            .await
            .map_err(|_| "unknown")?
            .map_err(|_| "unknown")?;
        }
        loop {
            let after = targeted_state(&properties, deadline).await?;
            if tokio::time::Instant::now() >= deadline {
                return Err("unknown");
            }
            let complete = match request.action {
                "next" | "previous" => before["track_id"]
                    .as_str()
                    .zip(after["track_id"].as_str())
                    .is_some_and(|(old, new)| {
                        old != new
                            && old != "/org/mpris/MediaPlayer2/TrackList/NoTrack"
                            && new != "/org/mpris/MediaPlayer2/TrackList/NoTrack"
                            && zvariant::ObjectPath::try_from(old).is_ok()
                            && zvariant::ObjectPath::try_from(new).is_ok()
                    }),
                "seek" => {
                    after["track_id"].as_str() == request.track_id.as_deref()
                        && after["position_us"]
                            .as_i64()
                            .zip(request.position_us)
                            .is_some_and(|(actual, desired)| actual.abs_diff(desired) <= 1_000_000)
                }
                _ => after["status"]
                    .as_str()
                    .zip(request.desired_status.or(toggle_status))
                    .is_some_and(|(actual, desired)| actual.eq_ignore_ascii_case(desired)),
            };
            if complete {
                return Ok(after);
            }
            tokio::time::sleep_until(
                deadline.min(tokio::time::Instant::now() + Duration::from_millis(40)),
            )
            .await;
        }
    }
    .await;
    match result {
        Ok(state) => {
            completion["verified"] = Value::Bool(true);
            for field in ["status", "track_id", "position_us"] {
                completion[field] = state[field].clone();
            }
        }
        Err(error) => {
            completion["error"] = Value::String(error.into());
        }
    }
    serde_json::json!({"assistant_completion_only":true,"assistant_media_completion":completion})
}

/// Absolute seek via `SetPosition` when the player published a track id,
/// else a relative `Seek` from the last reported position. Players that
/// refuse `CanSeek` are left alone.
async fn seek_active(connection: &Connection, player: &PlayerSnapshot, position_us: i64) {
    if !player.can_seek {
        return;
    }
    let destination = format!("{MPRIS_PREFIX}{}", player.player);
    let Ok(bus_name) = BusName::try_from(destination.clone()) else {
        return;
    };
    let position_us = position_us.max(0);
    if let Some(track_id) = player.track_id.as_deref()
        && let Ok(path) = zvariant::ObjectPath::try_from(track_id)
    {
        match tokio::time::timeout(
            PLAYER_CALL_TIMEOUT,
            connection.call_method(
                Some(bus_name.clone()),
                PLAYER_PATH,
                Some(PLAYER_INTERFACE),
                "SetPosition",
                &(path, position_us),
            ),
        )
        .await
        {
            Ok(Ok(_)) => log::debug!("SetPosition {position_us} on {destination}"),
            Ok(Err(error)) => log::warn!("SetPosition on {destination} failed: {error}"),
            Err(_) => log::warn!("SetPosition on {destination} timed out"),
        }
        return;
    }
    // No track id: fall back to a relative Seek from the last Position.
    let offset = position_us.saturating_sub(player.position_us.unwrap_or(0));
    match tokio::time::timeout(
        PLAYER_CALL_TIMEOUT,
        connection.call_method(
            Some(bus_name),
            PLAYER_PATH,
            Some(PLAYER_INTERFACE),
            "Seek",
            &(offset,),
        ),
    )
    .await
    {
        Ok(Ok(_)) => log::debug!("Seek {offset} on {destination}"),
        Ok(Err(error)) => log::warn!("Seek on {destination} failed: {error}"),
        Err(_) => log::warn!("Seek on {destination} timed out"),
    }
}

const PIN_FILE_NAME: &str = "mpris-pin";
const MAX_PIN_LEN: usize = 128;
static PIN_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn absolute_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// `$XDG_STATE_HOME/jwm/mpris-pin`, else `~/.local/state/jwm/mpris-pin`,
/// else a uid-scoped `/tmp` file when neither home is usable. Split out
/// so tests can pin the layout without racing process-wide env.
fn pin_state_path(xdg_state_home: Option<&Path>, home: Option<&Path>, uid: u32) -> PathBuf {
    if let Some(path) = xdg_state_home {
        return path.join("jwm").join(PIN_FILE_NAME);
    }
    if let Some(home) = home {
        return home
            .join(".local")
            .join("state")
            .join("jwm")
            .join(PIN_FILE_NAME);
    }
    PathBuf::from(format!("/tmp/jwm-{uid}")).join(PIN_FILE_NAME)
}

fn stored_pin_path() -> PathBuf {
    pin_state_path(
        absolute_env_path("XDG_STATE_HOME").as_deref(),
        absolute_env_path("HOME").as_deref(),
        unsafe { libc::geteuid() },
    )
}

/// Bus suffixes the pin file will accept: ASCII alphanumerics plus the
/// `.`/`-`/`_` MPRIS instance names already use. Anything else — a path,
/// whitespace, a 129th byte — is refused rather than written.
fn pin_suffix(raw: &str) -> Option<String> {
    let pin = raw.trim();
    if pin.is_empty() || pin.len() > MAX_PIN_LEN {
        return None;
    }
    pin.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        .then(|| pin.to_string())
}

fn parse_stored_pin(contents: &str) -> Option<String> {
    pin_suffix(contents.lines().next().unwrap_or(""))
}

fn read_stored_pin(path: &Path) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    parse_stored_pin(&contents)
}

fn atomic_write_pin(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to replace pin symlink: {}", path.display()),
        ));
    }
    let temporary = parent.join(format!(
        ".mpris-pin-{}-{}.tmp",
        std::process::id(),
        PIN_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(contents)?;
        if !contents.ends_with(b"\n") {
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            if let Err(error) = fs::rename(&temporary, path) {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
            Ok(())
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

fn write_stored_pin(path: &Path, pin: Option<&str>) {
    match pin.and_then(pin_suffix) {
        Some(pin) => {
            if let Err(error) = atomic_write_pin(path, pin.as_bytes()) {
                log::warn!("cannot persist mpris pin {}: {error}", path.display());
            }
        }
        None => {
            if let Err(error) = fs::remove_file(path)
                && error.kind() != io::ErrorKind::NotFound
            {
                log::warn!("cannot clear mpris pin file {}: {error}", path.display());
            }
        }
    }
}

fn sync_stored_pin(path: &Path, pin: Option<&str>, last: &mut Option<String>) {
    if last.as_deref() == pin {
        return;
    }
    write_stored_pin(path, pin);
    *last = pin.map(str::to_string);
}

/// Drive the watcher: re-publish when players come and go, and act on jwm's
/// `media/command` broadcasts.
///
/// Name-owner changes cover start/stop; a slow poll covers the property churn
/// (track changes) without subscribing to every player's `PropertiesChanged`,
/// which would mean tracking proxies per player for little gain.
pub async fn run(
    connection: Connection,
    ipc: JwmIpc,
    mut events: tokio::sync::mpsc::Receiver<Value>,
) {
    // The player jwm pinned the row to. Loaded from disk so a restart keeps
    // the choice; the first publish still drops a name nobody on the bus
    // owns (while other players remain).
    let pin_path = stored_pin_path();
    let mut last_written = read_stored_pin(&pin_path);
    let mut pinned = last_written.clone();
    pinned = publish(&connection, &ipc, pinned.as_deref()).await;
    sync_stored_pin(&pin_path, pinned.as_deref(), &mut last_written);

    let mut owner_changes = match DBusProxy::new(&connection).await {
        Ok(dbus) => match dbus.receive_name_owner_changed().await {
            Ok(stream) => Some(stream),
            Err(error) => {
                log::warn!("cannot watch bus names: {error}");
                None
            }
        },
        Err(error) => {
            log::warn!("cannot reach the bus daemon: {error}");
            None
        }
    };
    let mut poll = tokio::time::interval(std::time::Duration::from_secs(3));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { return };
                if event.get("event").and_then(Value::as_str) != Some("media/command") {
                    continue;
                }
                let payload = event.get("payload").cloned().unwrap_or(Value::Null);
                let Some(request) = parse_request(&payload) else {
                    let action = payload
                        .get("action")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    log::warn!("ignoring unknown media action {action:?}");
                    continue;
                };
                match request {
                    MediaRequest::TargetedTransport(request) => {
                        let args = targeted_transport(&connection,&request).await;
                        let ipc=ipc.clone();
                        // A lost completion remains unknown at JWM; never
                        // repeat the already-issued transport mutation.
                        let _=tokio::task::spawn_blocking(move||ipc.command("set_media_status",args)).await;
                    }
                    MediaRequest::Select(player) => {
                        // Empty / malformed names clear the pin. The publish
                        // right after still drops a well-formed name nobody
                        // on the bus owns (while other players remain).
                        pinned = pin_suffix(&player);
                        pinned = publish(&connection, &ipc, pinned.as_deref()).await;
                        sync_stored_pin(&pin_path, pinned.as_deref(), &mut last_written);
                    }
                    MediaRequest::Seek(position_us) => {
                        if let Some(sweep) = collect_players(&connection).await {
                            let (active, keep) = sweep.resolve(pinned.as_deref());
                            pinned = keep;
                            if let Some(active) = active {
                                seek_active(&connection, active, position_us).await;
                            }
                        }
                        pinned = publish(&connection, &ipc, pinned.as_deref()).await;
                        sync_stored_pin(&pin_path, pinned.as_deref(), &mut last_written);
                    }
                    MediaRequest::Transport(method) => {
                        // Re-resolve the active player instead of trusting a
                        // cached one: the user may have switched players since
                        // the last push.
                        if let Some(sweep) = collect_players(&connection).await {
                            let (active, keep) = sweep.resolve(pinned.as_deref());
                            pinned = keep;
                            if let Some(active) = active {
                                call_active(&connection, &active.player, method).await;
                            }
                        }
                        pinned = publish(&connection, &ipc, pinned.as_deref()).await;
                        sync_stored_pin(&pin_path, pinned.as_deref(), &mut last_written);
                    }
                }
            }
            Some(_) = next_owner_change(&mut owner_changes) => {
                pinned = publish(&connection, &ipc, pinned.as_deref()).await;
                sync_stored_pin(&pin_path, pinned.as_deref(), &mut last_written);
            }
            _ = poll.tick() => {
                pinned = publish(&connection, &ipc, pinned.as_deref()).await;
                sync_stored_pin(&pin_path, pinned.as_deref(), &mut last_written);
            }
        }
    }
}

/// Await the next name-owner change, or never resolve when the stream is
/// unavailable so `select!` keeps servicing the other branches.
async fn next_owner_change(
    stream: &mut Option<zbus::fdo::NameOwnerChangedStream>,
) -> Option<zbus::fdo::NameOwnerChanged> {
    match stream {
        Some(stream) => {
            use futures_util::StreamExt as _;
            stream.next().await
        }
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_open_receipts_remain_scoped_and_never_become_ordinary_status() {
        let mut player = super::PlayerSnapshot {
            player: "jwm".into(),
            bus_owner: Some(":1.17".into()),
            process_id: Some(93),
            ..Default::default()
        };
        let proof = serde_json::json!({"operation_id":"a","wm_instance_id":"wm-1","sequence":7,"resource_uri":"file:///song","enqueue":true,
            "target_player":"jwm","target_bus_owner":":1.17","target_pid":93,"verified":true,"mpv_command_ack":true,"player_request_id":41,"entry_id":9});
        let (key, args) = super::open_receipt(&player, &proof).unwrap();
        assert_eq!(args["assistant_completion_only"], true);
        assert_eq!(args["assistant_media_open_completion"]["process_id"], 93);
        player.eva_open_receipts.push(proof.clone());
        assert!(
            player
                .to_args()
                .get("assistant_media_open_completion")
                .is_none()
        );
        assert!(player.to_args().get("eva_open_receipts").is_none());
        let mut other = proof.clone();
        other["operation_id"] = serde_json::json!("b");
        assert_ne!(key, super::open_receipt(&player, &other).unwrap().0);
        other["wm_instance_id"] = serde_json::json!("wm-2");
        assert_ne!(key, super::open_receipt(&player, &other).unwrap().0);
        player.bus_owner = Some(":1.18".into());
        assert!(super::open_receipt(&player, &proof).is_none());
        player.bus_owner = Some(":1.17".into());
        player.process_id = Some(94);
        assert!(super::open_receipt(&player, &proof).is_none());
        player.process_id = None;
        assert!(super::open_receipt(&player, &proof).is_none());
    }
    use super::*;

    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize},
    };

    struct TargetBus {
        child: std::process::Child,
        address: String,
    }
    impl TargetBus {
        fn start() -> Option<Self> {
            use std::io::{BufRead, BufReader};
            use std::process::{Command, Stdio};
            let mut child = match Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(error) => {
                    assert!(
                        std::env::var_os("JWM_REQUIRE_DBUS_DAEMON").is_none(),
                        "dbus-daemon required: {error}"
                    );
                    return None;
                }
            };
            let mut address = String::new();
            BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut address)
                .unwrap();
            Some(Self {
                child,
                address: address.trim().to_owned(),
            })
        }
        async fn client(&self) -> Connection {
            zbus::connection::Builder::address(self.address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap()
        }
        async fn player(&self, suffix: &str, state: Arc<TargetPlayerState>) -> Connection {
            zbus::connection::Builder::address(self.address.as_str())
                .unwrap()
                .name(format!("{MPRIS_PREFIX}{suffix}"))
                .unwrap()
                .serve_at(PLAYER_PATH, TargetPlayer(state))
                .unwrap()
                .serve_at(PLAYER_PATH, TestRoot)
                .unwrap()
                .build()
                .await
                .unwrap()
        }
    }
    impl Drop for TargetBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    struct NativeOpenReceipt;
    #[zbus::interface(name = "org.jwm.Player")]
    impl NativeOpenReceipt {
        #[zbus(property)]
        fn eva_open_receipts(&self) -> Vec<String> {
            vec![
                "invalid old helper payload".into(),
                serde_json::json!({
                    "operation_id":"owned-op","wm_instance_id":"owned-wm","sequence":1,
                    "resource_uri":"file:///song","enqueue":true,"target_player":"jwm",
                    "target_bus_owner":null,"target_pid":null,"verified":true,
                    "mpv_command_ack":true,"player_request_id":7,"entry_id":11
                })
                .to_string(),
            ]
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_open_receipts_are_read_from_the_observed_unique_bus_owner() {
        let Some(bus) = TargetBus::start() else {
            return;
        };
        let server = zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("org.mpris.MediaPlayer2.jwm")
            .unwrap()
            .serve_at(PLAYER_PATH, TargetPlayer(Arc::default()))
            .unwrap()
            .serve_at(PLAYER_PATH, TestRoot)
            .unwrap()
            .serve_at(PLAYER_PATH, NativeOpenReceipt)
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = bus.client().await;
        let snapshot = snapshot(
            &client,
            &OwnedBusName::try_from("org.mpris.MediaPlayer2.jwm").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            snapshot.eva_open_receipts.len(),
            1,
            "invalid legacy values must not become receipts"
        );
        let (_, args) = open_receipt(&snapshot, &snapshot.eva_open_receipts[0]).unwrap();
        let proof = &args["assistant_media_open_completion"];
        assert_eq!(proof["bus_owner"], server.unique_name().unwrap().as_str());
        assert_eq!(proof["process_id"], std::process::id());
        assert_eq!(proof["operation_id"], "owned-op");
        assert_eq!(proof["entry_id"], 11);
    }
    #[derive(Default)]
    struct TargetPlayerState {
        paused: AtomicBool,
        pauses: AtomicUsize,
        plays: AtomicUsize,
        toggles: AtomicUsize,
        reads: AtomicUsize,
        block_method: AtomicBool,
        fail_readback: AtomicBool,
        fail_after_setter: AtomicBool,
        stopped: AtomicBool,
        track: AtomicUsize,
        position: std::sync::atomic::AtomicI64,
        nexts: AtomicUsize,
        ignore_advance: AtomicBool,
        previouses: AtomicUsize,
        stops: AtomicUsize,
        seeks: AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    struct TargetPlayer(Arc<TargetPlayerState>);
    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl TargetPlayer {
        #[zbus(property)]
        fn playback_status(&self) -> zbus::fdo::Result<String> {
            self.0.reads.fetch_add(1, Ordering::SeqCst);
            if self.0.fail_readback.load(Ordering::SeqCst) {
                return Err(zbus::fdo::Error::Failed("readback unavailable".into()));
            }
            Ok(if self.0.stopped.load(Ordering::SeqCst) {
                "Stopped"
            } else if self.0.paused.load(Ordering::SeqCst) {
                "Paused"
            } else {
                "Playing"
            }
            .into())
        }
        async fn pause(&self) {
            self.0.pauses.fetch_add(1, Ordering::SeqCst);
            if self.0.block_method.load(Ordering::SeqCst) {
                self.0.entered.notify_one();
                self.0.release.notified().await;
            }
            self.0.paused.store(true, Ordering::SeqCst);
            if self.0.fail_after_setter.load(Ordering::SeqCst) {
                self.0.fail_readback.store(true, Ordering::SeqCst);
            }
        }
        #[zbus(property)]
        fn metadata(&self) -> HashMap<String, OwnedValue> {
            HashMap::from([(
                "mpris:trackid".into(),
                OwnedValue::from(
                    zvariant::ObjectPath::try_from(format!(
                        "/track/{}",
                        self.0.track.load(Ordering::SeqCst)
                    ))
                    .unwrap(),
                ),
            )])
        }
        #[zbus(property)]
        fn position(&self) -> i64 {
            self.0.position.load(Ordering::SeqCst)
        }
        fn next(&self) {
            self.0.nexts.fetch_add(1, Ordering::SeqCst);
            if !self.0.ignore_advance.load(Ordering::SeqCst) {
                self.0.track.fetch_add(1, Ordering::SeqCst);
            }
        }
        fn previous(&self) {
            self.0.previouses.fetch_add(1, Ordering::SeqCst);
            if !self.0.ignore_advance.load(Ordering::SeqCst) {
                self.0.track.fetch_sub(1, Ordering::SeqCst);
            }
        }
        fn stop(&self) {
            self.0.stops.fetch_add(1, Ordering::SeqCst);
            self.0.stopped.store(true, Ordering::SeqCst);
        }
        fn set_position(&self, track: zvariant::ObjectPath<'_>, position: i64) {
            self.0.seeks.fetch_add(1, Ordering::SeqCst);
            if track.as_str() == format!("/track/{}", self.0.track.load(Ordering::SeqCst)) {
                self.0.position.store(position, Ordering::SeqCst);
            }
        }
        fn play(&self) {
            self.0.plays.fetch_add(1, Ordering::SeqCst);
            self.0.stopped.store(false, Ordering::SeqCst);
            self.0.paused.store(false, Ordering::SeqCst);
        }
        fn play_pause(&self) {
            self.0.toggles.fetch_add(1, Ordering::SeqCst);
            self.0.paused.fetch_xor(true, Ordering::SeqCst);
        }
    }
    fn target_request(operation: &str, connection: &Connection, play: bool) -> TargetedTransport {
        TargetedTransport {
            operation_id: operation.into(),
            wm_instance_id: "frozen-wm-instance".into(),
            player: "frozen".into(),
            bus_owner: connection.unique_name().unwrap().to_string(),
            pid: std::process::id(),
            action: if play { "play" } else { "pause" },
            track_id: None,
            position_us: None,
            method: if play { "Play" } else { "Pause" },
            desired_status: Some(if play { "playing" } else { "paused" }),
        }
    }
    #[test]
    fn targeted_requests_never_fall_back_to_dynamic_or_toggle_transport() {
        let payload = serde_json::json!({"action":"pause","assistant_operation_id":"op-1","wm_instance_id":"wm-1","target_player":"frozen","target_bus_owner":":1.99","target_pid":123});
        let Some(MediaRequest::TargetedTransport(request)) = parse_request(&payload) else {
            panic!("targeted pause not recognized")
        };
        assert_eq!(request.method, "Pause");
        assert_eq!(request.wm_instance_id, "wm-1");
        for field in [
            "assistant_operation_id",
            "wm_instance_id",
            "target_player",
            "target_bus_owner",
            "target_pid",
        ] {
            let mut missing = payload.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(parse_request(&missing).is_none());
        }
        let mut toggle = payload;
        toggle["action"] = Value::String("play_pause".into());
        assert!(matches!(
            parse_request(&toggle),
            Some(MediaRequest::TargetedTransport(_))
        ));
        toggle["action"] = Value::String("seek".into());
        assert!(
            parse_request(&toggle).is_none(),
            "seek requires a frozen track and position"
        );
        assert_eq!(
            parse_request(&serde_json::json!({"action":"play"})),
            Some(MediaRequest::Transport("Play"))
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn targeted_pause_waits_for_setter_and_reads_frozen_player_after_active_changes() {
        let Some(bus) = TargetBus::start() else {
            return;
        };
        let frozen = Arc::new(TargetPlayerState::default());
        frozen.paused.store(true, Ordering::SeqCst);
        let other = Arc::new(TargetPlayerState::default());
        let frozen_connection = bus.player("frozen", frozen.clone()).await;
        let _other_connection = bus.player("other", other.clone()).await;
        let client = bus.client().await;
        let reads = frozen.reads.load(Ordering::SeqCst);
        frozen.block_method.store(true, Ordering::SeqCst);
        let request = target_request("pause-1", &frozen_connection, false);
        let connection = client.clone();
        let pending = tokio::spawn(async move { targeted_transport(&connection, &request).await });
        tokio::time::timeout(Duration::from_millis(700), frozen.entered.notified())
            .await
            .unwrap();
        assert!(
            !pending.is_finished(),
            "a prior Paused state cannot settle a setter still in flight"
        );
        assert_eq!(
            frozen.reads.load(Ordering::SeqCst),
            reads + 1,
            "only the baseline can be read before setter reply"
        );
        frozen.release.notify_one();
        let first = pending.await.unwrap();
        assert_eq!(first["assistant_completion_only"], true);
        assert_eq!(
            first["assistant_media_completion"]["operation_id"],
            "pause-1"
        );
        assert_eq!(
            first["assistant_media_completion"]["wm_instance_id"],
            "frozen-wm-instance"
        );
        assert_eq!(first["assistant_media_completion"]["verified"], true);
        frozen.block_method.store(false, Ordering::SeqCst);
        let repeated = targeted_transport(
            &client,
            &target_request("pause-2", &frozen_connection, false),
        )
        .await;
        assert_eq!(repeated["assistant_media_completion"]["verified"], true);
        assert_eq!(
            frozen.pauses.load(Ordering::SeqCst),
            2,
            "repeat Pause must call the idempotent setter again"
        );
        assert!(frozen.reads.load(Ordering::SeqCst) >= reads + 4);
        let play =
            targeted_transport(&client, &target_request("play", &frozen_connection, true)).await;
        assert_eq!(play["assistant_media_completion"]["verified"], true);
        assert_eq!(frozen.plays.load(Ordering::SeqCst), 1);
        let paused = targeted_transport(
            &client,
            &target_request("pause-3", &frozen_connection, false),
        )
        .await;
        assert_eq!(paused["assistant_media_completion"]["verified"], true);
        let sweep = collect_players(&client).await.unwrap();
        assert_eq!(
            sweep.resolve(None).0.unwrap().player,
            "other",
            "active row can change independently from targeted completion"
        );
        assert_eq!(other.pauses.load(Ordering::SeqCst), 0);
        assert_eq!(other.plays.load(Ordering::SeqCst), 0);
        assert_eq!(frozen.toggles.load(Ordering::SeqCst), 0);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn targeted_transport_rejects_replaced_owner_without_touching_either_player() {
        let Some(bus) = TargetBus::start() else {
            return;
        };
        let old = Arc::new(TargetPlayerState::default());
        let old_connection = bus.player("frozen", old.clone()).await;
        let request = target_request("old-owner", &old_connection, false);
        assert!(
            old_connection
                .release_name(format!("{MPRIS_PREFIX}frozen"))
                .await
                .unwrap()
        );
        let replacement = Arc::new(TargetPlayerState::default());
        let _new_connection = bus.player("frozen", replacement.clone()).await;
        let reply = targeted_transport(&bus.client().await, &request).await;
        assert_eq!(reply["assistant_media_completion"]["verified"], false);
        assert_eq!(reply["assistant_media_completion"]["error"], "target_gone");
        assert_eq!(old.pauses.load(Ordering::SeqCst), 0);
        assert_eq!(replacement.pauses.load(Ordering::SeqCst), 0);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn targeted_relative_transport_stop_and_seek_have_exact_postconditions() {
        let Some(bus) = TargetBus::start() else {
            return;
        };
        let state = Arc::new(TargetPlayerState::default());
        state.track.store(10, Ordering::SeqCst);
        let other = Arc::new(TargetPlayerState::default());
        let server = bus.player("frozen", state.clone()).await;
        let _other = bus.player("other", other.clone()).await;
        let client = bus.client().await;
        let mut request = target_request("next", &server, false);
        request.action = "next";
        request.method = "Next";
        request.desired_status = None;
        let next = targeted_transport(&client, &request).await;
        let proof = &next["assistant_media_completion"];
        assert_eq!(proof["verified"], true);
        assert_eq!(proof["before_track_id"], "/track/10");
        assert_eq!(proof["track_id"], "/track/11");
        assert_eq!(proof["action"], "next");
        assert_eq!(proof["process_id"], std::process::id());
        request.operation_id = "previous".into();
        request.action = "previous";
        request.method = "Previous";
        let previous = targeted_transport(&client, &request).await;
        assert_eq!(
            previous["assistant_media_completion"]["track_id"],
            "/track/10"
        );
        assert_eq!(previous["assistant_media_completion"]["verified"], true);
        for (number, before, after) in [(1, "Playing", "Paused"), (2, "Paused", "Playing")] {
            request.operation_id = format!("toggle-{number}");
            request.action = "play_pause";
            request.method = "PlayPause";
            let toggle = targeted_transport(&client, &request).await;
            let proof = &toggle["assistant_media_completion"];
            assert_eq!(proof["verified"], true);
            assert_eq!(proof["before_status"], before);
            assert_eq!(proof["status"], after);
        }
        for number in 1..=2 {
            request.operation_id = format!("stop-{number}");
            request.action = "stop";
            request.method = "Stop";
            request.desired_status = Some("stopped");
            let stop = targeted_transport(&client, &request).await;
            assert_eq!(stop["assistant_media_completion"]["verified"], true);
            assert_eq!(stop["assistant_media_completion"]["status"], "Stopped");
        }
        request.operation_id = "seek".into();
        request.action = "seek";
        request.method = "SetPosition";
        request.desired_status = None;
        request.track_id = Some("/track/10".into());
        request.position_us = Some(2_500_000);
        let seek = targeted_transport(&client, &request).await;
        assert_eq!(seek["assistant_media_completion"]["verified"], true);
        assert_eq!(seek["assistant_media_completion"]["position_us"], 2_500_000);
        request.operation_id = "stale-seek".into();
        request.track_id = Some("/track/11".into());
        let stale = targeted_transport(&client, &request).await;
        assert_eq!(stale["assistant_media_completion"]["error"], "target_gone");
        assert_eq!(
            state.seeks.load(Ordering::SeqCst),
            1,
            "a changed track must stop before SetPosition"
        );
        request.operation_id = "wrong-pid".into();
        request.action = "stop";
        request.method = "Stop";
        request.pid += 1;
        let wrong = targeted_transport(&client, &request).await;
        assert_eq!(wrong["assistant_media_completion"]["error"], "target_gone");
        assert_eq!(state.nexts.load(Ordering::SeqCst), 1);
        assert_eq!(state.previouses.load(Ordering::SeqCst), 1);
        assert_eq!(state.toggles.load(Ordering::SeqCst), 2);
        assert_eq!(state.stops.load(Ordering::SeqCst), 2);
        assert_eq!(
            other.nexts.load(Ordering::SeqCst)
                + other.previouses.load(Ordering::SeqCst)
                + other.toggles.load(Ordering::SeqCst)
                + other.stops.load(Ordering::SeqCst)
                + other.seeks.load(Ordering::SeqCst),
            0
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn targeted_next_without_a_new_track_is_unknown_and_never_reissued() {
        let Some(bus) = TargetBus::start() else {
            return;
        };
        let state = Arc::new(TargetPlayerState::default());
        state.ignore_advance.store(true, Ordering::SeqCst);
        let server = bus.player("frozen", state.clone()).await;
        let client = bus.client().await;
        let mut request = target_request("no-next", &server, false);
        request.action = "next";
        request.method = "Next";
        request.desired_status = None;
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            targeted_transport(&client, &request),
        )
        .await
        .expect("postcondition wait must stay bounded");
        assert_eq!(result["assistant_media_completion"]["verified"], false);
        assert_eq!(result["assistant_media_completion"]["error"], "unknown");
        assert_eq!(state.nexts.load(Ordering::SeqCst), 1);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn targeted_readback_errors_and_method_timeouts_remain_unknown_without_replay() {
        let Some(bus) = TargetBus::start() else {
            return;
        };
        let state = Arc::new(TargetPlayerState::default());
        let server = bus.player("frozen", state.clone()).await;
        let client = bus.client().await;
        state.fail_after_setter.store(true, Ordering::SeqCst);
        let result =
            targeted_transport(&client, &target_request("read-failed", &server, false)).await;
        assert_eq!(result["assistant_media_completion"]["verified"], false);
        assert_eq!(result["assistant_media_completion"]["error"], "unknown");
        assert_eq!(
            state.pauses.load(Ordering::SeqCst),
            1,
            "an ambiguous read cannot repeat the setter"
        );
        state.fail_readback.store(false, Ordering::SeqCst);
        state.fail_after_setter.store(false, Ordering::SeqCst);
        state.block_method.store(true, Ordering::SeqCst);
        let reads = state.reads.load(Ordering::SeqCst);
        let result =
            targeted_transport(&client, &target_request("method-timeout", &server, false)).await;
        assert_eq!(result["assistant_media_completion"]["error"], "unknown");
        assert_eq!(state.pauses.load(Ordering::SeqCst), 2);
        assert_eq!(
            state.reads.load(Ordering::SeqCst),
            reads + 1,
            "a timed-out setter has a baseline but no post-setter readback"
        );
        state.release.notify_one();
    }

    fn picker_details(args: &Value) -> Value {
        Value::Array(args["player_details"].as_array().unwrap().iter().map(|row| {
            serde_json::json!({"player":row["player"],"identity":row["identity"],"status":row["status"]})
        }).collect())
    }

    struct TestPlayer {
        stalled: bool,
    }

    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl TestPlayer {
        #[zbus(property)]
        async fn playback_status(&self) -> &str {
            if self.stalled {
                std::future::pending::<()>().await;
            }
            "Playing"
        }

        async fn pause(&self) {
            if self.stalled {
                std::future::pending::<()>().await;
            }
        }
    }

    struct TestRoot;

    #[zbus::interface(name = "org.mpris.MediaPlayer2")]
    impl TestRoot {
        #[zbus(property)]
        fn identity(&self) -> &str {
            "Test player"
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_players_do_not_serialize_scans_or_hold_media_commands() {
        use std::io::{BufRead, BufReader};
        use std::process::{Child, Command, Stdio};

        struct PrivateBus(Child);
        impl Drop for PrivateBus {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let child = match Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                assert!(
                    std::env::var_os("JWM_REQUIRE_DBUS_DAEMON").is_none(),
                    "dbus-daemon required: {error}"
                );
                return;
            }
        };
        let mut bus = PrivateBus(child);
        let mut address = String::new();
        BufReader::new(bus.0.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim();
        assert!(!address.is_empty());
        let mut servers = Vec::new();
        // Register the healthy players in reverse order to catch dependence
        // on bus enumeration or concurrent completion order.
        for (suffix, stalled) in [
            ("zslow", true),
            ("aslow", true),
            ("mslow", true),
            ("bb", false),
            ("aa", false),
        ] {
            servers.push(
                zbus::connection::Builder::address(address)
                    .unwrap()
                    .name(format!("{MPRIS_PREFIX}{suffix}"))
                    .unwrap()
                    .serve_at(PLAYER_PATH, TestPlayer { stalled })
                    .unwrap()
                    .serve_at(PLAYER_PATH, TestRoot)
                    .unwrap()
                    .build()
                    .await
                    .unwrap(),
            );
        }
        let client = zbus::connection::Builder::address(address)
            .unwrap()
            .build()
            .await
            .unwrap();
        let sweep = tokio::time::timeout(Duration::from_secs(2), collect_players(&client))
            .await
            .expect("three stalled players must time out concurrently")
            .unwrap();
        assert_eq!(
            sweep
                .players
                .iter()
                .map(|p| p.player.as_str())
                .collect::<Vec<_>>(),
            ["aa", "bb"]
        );
        assert_eq!(sweep.resolve(None).0.unwrap().player, "aa");
        let (active, pin) = sweep.resolve(Some("aslow"));
        assert!(
            active.is_none(),
            "never retarget a pinned command to another player"
        );
        assert_eq!(pin.as_deref(), Some("aslow"));
        let (args, pin) = sweep.publication(Some("aslow"));
        assert_eq!(pin.as_deref(), Some("aslow"));
        assert_eq!(args["player"], "aslow");
        assert_eq!(args["status"], "Stopped");
        assert_eq!(args["can_go_next"], false);
        assert_eq!(args["can_seek"], false);
        assert_eq!(
            args["players"],
            serde_json::json!(["aa", "aslow", "bb", "mslow", "zslow"])
        );
        assert!(
            args["player_details"]
                .as_array()
                .unwrap()
                .iter()
                .any(|player| player["player"] == "aa")
        );
        tokio::time::timeout(
            Duration::from_secs(2),
            call_active(&client, "aslow", "Pause"),
        )
        .await
        .expect("a stalled transport command must release the media loop");
        drop(servers);
    }

    fn player(name: &str, status: &str) -> PlayerSnapshot {
        PlayerSnapshot {
            player: name.to_string(),
            identity: name.to_string(),
            status: status.to_string(),
            title: "Track".to_string(),
            artist: "Artist".to_string(),
            can_go_next: true,
            can_go_previous: true,
            can_seek: false,
            position_us: None,
            length_us: None,
            track_id: None,
            ..PlayerSnapshot::default()
        }
    }

    fn player_named(name: &str, identity: &str, status: &str) -> PlayerSnapshot {
        let mut snapshot = player(name, status);
        snapshot.identity = identity.to_string();
        snapshot
    }

    #[test]
    fn a_playing_player_outranks_a_paused_one() {
        let players = vec![player("mpv", "Paused"), player("spotify", "Playing")];
        assert_eq!(pick_active(&players).unwrap().player, "spotify");
    }
    #[test]
    fn resource_proof_survives_publication_even_when_another_player_is_active() {
        let mut native = player("jwm", "Paused");
        native.bus_owner = Some(":1.24".into());
        native.process_id = Some(1234);
        native.track_id = Some("/org/jwm/Player/track_17".into());
        native.track_url = Some("file:///tmp/requested%20media.ogg".into());
        native.queued_urls = vec![native.track_url.clone().unwrap()];
        let (args, _) = publish_args(&[native, player("other", "Playing")], None);
        assert_eq!(args["player"], "other");
        let proof = &args["player_details"][0];
        assert_eq!(proof["player"], "jwm");
        assert_eq!(proof["bus_owner"], ":1.24");
        assert_eq!(proof["process_id"], 1234);
        assert_eq!(proof["track_url"], "file:///tmp/requested%20media.ogg");
        assert_eq!(
            proof["queued_urls"],
            serde_json::json!(["file:///tmp/requested%20media.ogg"])
        );
    }

    #[test]
    fn a_paused_player_outranks_a_stopped_one() {
        let players = vec![player("mpv", "Stopped"), player("spotify", "Paused")];
        assert_eq!(pick_active(&players).unwrap().player, "spotify");
    }

    #[test]
    fn ties_keep_the_first_player_so_the_choice_does_not_flap() {
        let players = vec![player("aaa", "Paused"), player("bbb", "Paused")];
        assert_eq!(pick_active(&players).unwrap().player, "aaa");
    }

    #[test]
    fn no_players_means_no_active_player() {
        assert!(pick_active(&[]).is_none());
    }

    #[test]
    fn artist_lists_are_joined() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "xesam:artist".to_string(),
            OwnedValue::try_from(zvariant::Value::from(vec!["Miles Davis", "Bill Evans"]))
                .expect("string list"),
        );
        assert_eq!(artist_from_metadata(&metadata), "Miles Davis, Bill Evans");
    }

    #[test]
    fn a_bare_string_artist_is_tolerated() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "xesam:artist".to_string(),
            OwnedValue::from(zvariant::Str::from("Miles Davis")),
        );
        assert_eq!(artist_from_metadata(&metadata), "Miles Davis");
    }

    #[test]
    fn missing_metadata_reads_as_empty() {
        let metadata = HashMap::new();
        assert_eq!(title_from_metadata(&metadata), "");
        assert_eq!(artist_from_metadata(&metadata), "");
    }

    #[test]
    fn titles_come_from_xesam_title() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "xesam:title".to_string(),
            OwnedValue::from(zvariant::Str::from("Blue in Green")),
        );
        assert_eq!(title_from_metadata(&metadata), "Blue in Green");
    }

    #[test]
    fn jwm_actions_map_onto_mpris_methods() {
        assert_eq!(method_for("play"), Some("Play"));
        assert_eq!(method_for("pause"), Some("Pause"));
        assert_eq!(method_for("play_pause"), Some("PlayPause"));
        assert_eq!(method_for("next"), Some("Next"));
        assert_eq!(method_for("previous"), Some("Previous"));
        assert_eq!(method_for("stop"), Some("Stop"));
        assert_eq!(method_for("rewind"), None);
    }

    #[test]
    fn snapshot_args_carry_every_field_jwm_parses() {
        let args = player("spotify", "Playing").to_args();
        assert_eq!(args["player"], "spotify");
        assert_eq!(args["status"], "Playing");
        assert_eq!(args["title"], "Track");
        assert_eq!(args["artist"], "Artist");
        assert_eq!(args["can_go_next"], true);
    }

    #[test]
    fn track_lengths_come_from_mpris_length_in_microseconds() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "mpris:length".to_string(),
            OwnedValue::try_from(zvariant::Value::from(245_000_000i64)).expect("i64"),
        );
        assert_eq!(length_from_metadata(&metadata), Some(245_000_000));
    }

    #[test]
    fn an_unsigned_length_is_tolerated() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "mpris:length".to_string(),
            OwnedValue::try_from(zvariant::Value::from(245_000_000u64)).expect("u64"),
        );
        assert_eq!(length_from_metadata(&metadata), Some(245_000_000));
    }

    #[test]
    fn missing_or_misshapen_lengths_read_as_unreported() {
        let mut metadata = HashMap::new();
        assert_eq!(length_from_metadata(&metadata), None, "no metadata at all");

        // A stream that published a string here must not fail the snapshot.
        metadata.insert(
            "mpris:length".to_string(),
            OwnedValue::from(zvariant::Str::from("forever")),
        );
        assert_eq!(length_from_metadata(&metadata), None);
    }

    #[test]
    fn snapshot_args_push_position_and_length_as_nullable_microseconds() {
        let mut with_progress = player("spotify", "Playing");
        with_progress.position_us = Some(161_000_000);
        with_progress.length_us = Some(245_000_000);
        let args = with_progress.to_args();
        assert_eq!(args["position_us"], 161_000_000);
        assert_eq!(args["length_us"], 245_000_000);

        // A player that reports neither still sends the keys, as nulls: an
        // old jwm ignores unknown keys either way, and a new one reads a
        // missing or null value as "not reported".
        let args = player("spotify", "Playing").to_args();
        assert_eq!(args["position_us"], Value::Null);
        assert_eq!(args["length_us"], Value::Null);
    }

    #[test]
    fn the_publish_payload_lists_every_player_in_sweep_order() {
        let players = vec![player("mpv", "Paused"), player("spotify", "Playing")];
        let (args, pin) = publish_args(&players, None);
        assert_eq!(args["player"], "spotify", "the active pick still leads");
        assert_eq!(args["players"], serde_json::json!(["mpv", "spotify"]));
        assert_eq!(
            picker_details(&args),
            serde_json::json!([
                {"player": "mpv", "identity": "mpv", "status": "Paused"},
                {"player": "spotify", "identity": "spotify", "status": "Playing"},
            ])
        );
        assert_eq!(pin, None, "no pin was given, none comes back");

        // The list is the sweep's, not the resolved player's: a pin does not
        // reorder or trim it.
        let (args, _) = publish_args(&players, Some("mpv"));
        assert_eq!(args["players"], serde_json::json!(["mpv", "spotify"]));
        assert_eq!(
            picker_details(&args),
            serde_json::json!([
                {"player": "mpv", "identity": "mpv", "status": "Paused"},
                {"player": "spotify", "identity": "spotify", "status": "Playing"},
            ])
        );
        // A player-less sweep keeps the null signal, with no list key at all,
        // but holds the pin so a later launch of the same player can reclaim
        // it (a restart that beats the player onto the bus must not forget).
        let (args, pin) = publish_args(&[], Some("mpv"));
        assert_eq!(args["player"], Value::Null);
        assert!(args.get("players").is_none());
        assert!(args.get("player_details").is_none());
        assert_eq!(pin.as_deref(), Some("mpv"));
    }

    #[test]
    fn player_details_carry_identity_alongside_the_string_list() {
        let players = vec![
            player_named("spotify", "Spotify", "Playing"),
            player_named("chromium", "Google Chrome", "Paused"),
        ];
        let (args, _) = publish_args(&players, None);
        assert_eq!(
            args["players"],
            serde_json::json!(["spotify", "chromium"]),
            "string list stays the cycle key"
        );
        assert_eq!(
            picker_details(&args),
            serde_json::json!([
                {"player": "spotify", "identity": "Spotify", "status": "Playing"},
                {"player": "chromium", "identity": "Google Chrome", "status": "Paused"},
            ])
        );
    }

    #[test]
    fn the_pin_outranks_the_active_pick_while_its_player_is_alive() {
        let players = vec![player("mpv", "Paused"), player("spotify", "Playing")];
        let (args, pin) = publish_args(&players, Some("mpv"));
        assert_eq!(args["player"], "mpv", "the pinned player leads the push");
        assert_eq!(args["status"], "Paused");
        assert_eq!(pin.as_deref(), Some("mpv"), "the pin holds");
    }

    #[test]
    fn a_dead_pin_falls_back_to_the_active_pick_and_clears_itself() {
        let players = vec![player("spotify", "Playing")];
        let (args, pin) = publish_args(&players, Some("gone"));
        assert_eq!(args["player"], "spotify");
        assert_eq!(pin, None, "a pin naming nobody alive cannot stick");
    }

    #[test]
    fn select_player_parses_into_a_pin_and_transports_keep_their_methods() {
        assert_eq!(
            parse_request(&serde_json::json!({"action": "select_player", "player": "mpv"})),
            Some(MediaRequest::Select("mpv".to_string()))
        );
        // A missing or empty player name clears the pin rather than naming
        // an empty bus suffix.
        assert_eq!(
            parse_request(&serde_json::json!({"action": "select_player"})),
            Some(MediaRequest::Select(String::new()))
        );
        assert_eq!(
            parse_request(&serde_json::json!({"action": "select_player", "player": "  "})),
            Some(MediaRequest::Select(String::new()))
        );
        assert_eq!(
            parse_request(&serde_json::json!({"action": "play_pause"})),
            Some(MediaRequest::Transport("PlayPause"))
        );
        assert_eq!(
            parse_request(&serde_json::json!({"action": "seek", "position_us": 90_000_000i64})),
            Some(MediaRequest::Seek(90_000_000))
        );
        assert_eq!(
            parse_request(&serde_json::json!({"action": "seek", "position_us": -5i64})),
            Some(MediaRequest::Seek(0)),
            "a negative seek clamps to the start"
        );
        assert_eq!(
            parse_request(&serde_json::json!({"action": "seek"})),
            None,
            "seek without a position is ignorable"
        );
        // Unknown actions stay ignorable, so old and new ends tolerate each
        // other in either direction.
        assert_eq!(
            parse_request(&serde_json::json!({"action": "rewind"})),
            None
        );
        assert_eq!(parse_request(&serde_json::json!({})), None);
    }

    #[test]
    fn track_ids_come_from_mpris_trackid() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "mpris:trackid".to_string(),
            OwnedValue::from(
                zvariant::ObjectPath::try_from("/org/mpris/MediaPlayer2/Track/1").expect("path"),
            ),
        );
        assert_eq!(
            track_id_from_metadata(&metadata).as_deref(),
            Some("/org/mpris/MediaPlayer2/Track/1")
        );
        assert_eq!(track_id_from_metadata(&HashMap::new()), None);
    }

    #[test]
    fn snapshot_args_advertise_can_seek_append_only() {
        let mut seeking = player("spotify", "Playing");
        seeking.can_seek = true;
        let args = seeking.to_args();
        assert_eq!(args["can_seek"], true);
        assert_eq!(player("spotify", "Playing").to_args()["can_seek"], false);
    }

    #[test]
    fn an_empty_sweep_keeps_a_pin_so_a_later_launch_can_reclaim_it() {
        let (args, pin) = publish_args(&[], Some("mpv"));
        assert_eq!(args["player"], Value::Null);
        assert_eq!(
            pin.as_deref(),
            Some("mpv"),
            "nobody on the bus must not forget the pin"
        );
    }

    #[test]
    fn pin_suffixes_reject_paths_and_oversized_names() {
        assert_eq!(pin_suffix("spotify").as_deref(), Some("spotify"));
        assert_eq!(
            pin_suffix("  firefox.instance_1-23  ").as_deref(),
            Some("firefox.instance_1-23")
        );
        assert_eq!(pin_suffix(""), None);
        assert_eq!(pin_suffix("   "), None);
        assert_eq!(pin_suffix("a/b"), None);
        assert_eq!(pin_suffix("has space"), None);
        assert_eq!(pin_suffix(&"x".repeat(MAX_PIN_LEN + 1)), None);
        assert_eq!(pin_suffix("播放器"), None);
    }

    #[test]
    fn pin_state_path_prefers_xdg_then_home_then_tmp() {
        assert_eq!(
            pin_state_path(
                Some(Path::new("/var/state")),
                Some(Path::new("/home/me")),
                1000
            ),
            PathBuf::from("/var/state/jwm/mpris-pin")
        );
        assert_eq!(
            pin_state_path(None, Some(Path::new("/home/me")), 1000),
            PathBuf::from("/home/me/.local/state/jwm/mpris-pin")
        );
        assert_eq!(
            pin_state_path(None, None, 42),
            PathBuf::from("/tmp/jwm-42/mpris-pin")
        );
    }

    #[test]
    fn stored_pin_round_trips_and_clears() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "jwm-mpris-pin-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("temp pin dir");
        let path = dir.join("mpris-pin");

        write_stored_pin(&path, Some("spotify"));
        assert_eq!(read_stored_pin(&path).as_deref(), Some("spotify"));
        assert_eq!(
            parse_stored_pin("spotify\nignored\n").as_deref(),
            Some("spotify")
        );

        // A second write of the same name is a no-op for sync; a clear
        // removes the file so a restart does not resurrect a dead pin.
        let mut last = Some("spotify".to_string());
        sync_stored_pin(&path, Some("spotify"), &mut last);
        assert!(path.is_file());
        sync_stored_pin(&path, None, &mut last);
        assert_eq!(last, None);
        assert!(!path.exists());
        assert_eq!(read_stored_pin(&path), None);

        // Junk on disk is ignored rather than trusted as a bus suffix.
        fs::write(&path, "not a/valid pin\n").expect("junk pin");
        assert_eq!(read_stored_pin(&path), None);

        let _ = fs::remove_dir_all(&dir);
    }
}
