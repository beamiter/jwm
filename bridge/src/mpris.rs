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
    let properties = zbus::fdo::PropertiesProxy::builder(connection)
        .destination(name.clone())
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
    })
}

/// Sweep every MPRIS player currently on the bus and push the resolved one
/// to jwm, with the sweep's full suffix list alongside. Pushing
/// `player: null` is how "every player went away" is reported. Returns the
/// pin to keep: the pinned player while its bus name is alive, otherwise
/// the pin clears itself rather than sticking to a player that is gone. A
/// sweep that could not run at all (the bus daemon unreachable) validates
/// nothing, so the pin passes through untouched.
async fn publish(connection: &Connection, ipc: &JwmIpc, pinned: Option<&str>) -> Option<String> {
    let Ok(dbus) = DBusProxy::new(connection).await else {
        return pinned.map(str::to_string);
    };
    let Ok(names) = dbus.list_names().await else {
        return pinned.map(str::to_string);
    };

    let mut players = Vec::new();
    for name in names
        .into_iter()
        .filter(|name| name.as_str().starts_with(MPRIS_PREFIX))
    {
        if let Some(snapshot) = snapshot(connection, &name).await {
            players.push(snapshot);
        }
    }

    let (args, pin) = publish_args(&players, pinned);
    let ipc = ipc.clone();
    let _ = tokio::task::spawn_blocking(move || ipc.command("set_media_status", args)).await;
    pin
}

/// Method name on `org.mpris.MediaPlayer2.Player` for a jwm media action.
#[must_use]
pub fn method_for(action: &str) -> Option<&'static str> {
    match action {
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
    /// Call the MPRIS method on the resolved player.
    Transport(&'static str),
    /// Pin resolution to this bus suffix; an empty name clears the pin back
    /// to the active pick.
    Select(String),
    /// Seek the resolved player to an absolute position in microseconds.
    Seek(i64),
}

/// Parse a command's payload. Unknown actions are `None`, which the caller
/// logs and drops: an old jwm and a new bridge — or the reverse — degrade
/// to ignoring each other's news rather than failing.
fn parse_request(payload: &Value) -> Option<MediaRequest> {
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
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
    match connection
        .call_method(
            Some(bus_name),
            PLAYER_PATH,
            Some(PLAYER_INTERFACE),
            method,
            &(),
        )
        .await
    {
        Ok(_) => log::debug!("{method} on {destination}"),
        Err(error) => log::warn!("{method} on {destination} failed: {error}"),
    }
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
        match connection
            .call_method(
                Some(bus_name.clone()),
                PLAYER_PATH,
                Some(PLAYER_INTERFACE),
                "SetPosition",
                &(path, position_us),
            )
            .await
        {
            Ok(_) => log::debug!("SetPosition {position_us} on {destination}"),
            Err(error) => log::warn!("SetPosition on {destination} failed: {error}"),
        }
        return;
    }
    // No track id: fall back to a relative Seek from the last Position.
    let offset = position_us.saturating_sub(player.position_us.unwrap_or(0));
    match connection
        .call_method(
            Some(bus_name),
            PLAYER_PATH,
            Some(PLAYER_INTERFACE),
            "Seek",
            &(offset,),
        )
        .await
    {
        Ok(_) => log::debug!("Seek {offset} on {destination}"),
        Err(error) => log::warn!("Seek on {destination} failed: {error}"),
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
                    MediaRequest::Select(player) => {
                        // Empty / malformed names clear the pin. The publish
                        // right after still drops a well-formed name nobody
                        // on the bus owns (while other players remain).
                        pinned = pin_suffix(&player);
                        pinned = publish(&connection, &ipc, pinned.as_deref()).await;
                        sync_stored_pin(&pin_path, pinned.as_deref(), &mut last_written);
                    }
                    MediaRequest::Seek(position_us) => {
                        if let Ok(dbus) = DBusProxy::new(&connection).await
                            && let Ok(names) = dbus.list_names().await
                        {
                            let mut players = Vec::new();
                            for name in names
                                .into_iter()
                                .filter(|name| name.as_str().starts_with(MPRIS_PREFIX))
                            {
                                if let Some(snapshot) = snapshot(&connection, &name).await {
                                    players.push(snapshot);
                                }
                            }
                            let (active, keep) = resolve_active(&players, pinned.as_deref());
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
                        if let Ok(dbus) = DBusProxy::new(&connection).await
                            && let Ok(names) = dbus.list_names().await
                        {
                            let mut players = Vec::new();
                            for name in names
                                .into_iter()
                                .filter(|name| name.as_str().starts_with(MPRIS_PREFIX))
                            {
                                if let Some(snapshot) = snapshot(&connection, &name).await {
                                    players.push(snapshot);
                                }
                            }
                            let (active, keep) = resolve_active(&players, pinned.as_deref());
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
    use super::*;

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
            args["player_details"],
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
            args["player_details"],
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
            args["player_details"],
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
