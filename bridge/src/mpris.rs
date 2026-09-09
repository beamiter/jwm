//! MPRIS player tracking, pushed into jwm's shell.
//!
//! The compositor has no bus connection, so this module is the eyes and hands
//! of its media row: it watches every `org.mpris.MediaPlayer2.*` name on the
//! session bus, pushes the *active* player's state in over IPC, and turns
//! jwm's `media/command` broadcasts back into method calls. When several
//! players are on the bus the push also names them all, and jwm can pin the
//! row to one of them with a `select_player` command; the pin holds while
//! its bus name is alive and clears itself when the player goes away.
//!
//! Player selection and metadata extraction are pure functions so the rules
//! (a playing player outranks a paused one; `xesam:artist` is a list) are unit
//! tested without a bus.

use std::collections::HashMap;

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
    /// The `Position` property in microseconds. It is a live counter, not
    /// metadata, and players only emit `Seeked` for it — so it is read on the
    /// same sweep as everything else. `None` when the player did not report
    /// one; the value is passed through as-is, garbage included, and the
    /// display side decides what is sane to show.
    pub position_us: Option<i64>,
    /// `mpris:length` from the track metadata, microseconds. `None` when the
    /// track does not say — streams routinely do not.
    pub length_us: Option<i64>,
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
            // reads a missing key (old bridge) as `None`.
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
/// alive comes back cleared, so a player that quit cannot hold the row.
fn resolve_active<'a>(
    players: &'a [PlayerSnapshot],
    pinned: Option<&str>,
) -> (Option<&'a PlayerSnapshot>, Option<String>) {
    if let Some(pin) = pinned
        && let Some(player) = players.iter().find(|player| player.player == pin)
    {
        return (Some(player), Some(pin.to_string()));
    }
    (pick_active(players), None)
}

/// The `set_media_status` payload for one sweep: the resolved player's
/// state, plus every bus suffix the sweep saw in sweep order, so jwm can
/// offer player switching without a second channel. The list is an
/// append-only key — an old jwm ignores it, and a new jwm reading a missing
/// one (an old bridge) treats the session as single-player.
fn publish_args(players: &[PlayerSnapshot], pinned: Option<&str>) -> (Value, Option<String>) {
    let (active, pin) = resolve_active(players, pinned);
    let args = match active {
        Some(active) => {
            let mut args = active.to_args();
            args["players"] = players
                .iter()
                .map(|player| Value::String(player.player.clone()))
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
        // `Position` rides the same GetAll as everything else: it does not
        // emit PropertiesChanged reliably, so the sweep's re-read is the
        // update mechanism — a separate subscription would add per-player
        // proxies for nothing.
        position_us: player.get("Position").and_then(i64_of),
        length_us: length_from_metadata(&metadata),
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
    // The player jwm pinned the row to, while its bus name stays alive.
    let mut pinned: Option<String> = None;
    pinned = publish(&connection, &ipc, pinned.as_deref()).await;

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
                        // The publish right after validates the name against
                        // the live sweep: a player nobody owns clears the
                        // pin again on the spot.
                        pinned = (!player.is_empty()).then_some(player);
                        pinned = publish(&connection, &ipc, pinned.as_deref()).await;
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
                    }
                }
            }
            Some(_) = next_owner_change(&mut owner_changes) => {
                pinned = publish(&connection, &ipc, pinned.as_deref()).await;
            }
            _ = poll.tick() => {
                pinned = publish(&connection, &ipc, pinned.as_deref()).await;
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
            position_us: None,
            length_us: None,
        }
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
        assert_eq!(pin, None, "no pin was given, none comes back");

        // The list is the sweep's, not the resolved player's: a pin does not
        // reorder or trim it.
        let (args, _) = publish_args(&players, Some("mpv"));
        assert_eq!(args["players"], serde_json::json!(["mpv", "spotify"]));
        // A player-less sweep keeps the null signal, with no list key at all.
        let (args, pin) = publish_args(&[], Some("mpv"));
        assert_eq!(args["player"], Value::Null);
        assert!(args.get("players").is_none());
        assert_eq!(pin, None);
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
        // Unknown actions stay ignorable, so old and new ends tolerate each
        // other in either direction.
        assert_eq!(
            parse_request(&serde_json::json!({"action": "rewind"})),
            None
        );
        assert_eq!(parse_request(&serde_json::json!({})), None);
    }
}
