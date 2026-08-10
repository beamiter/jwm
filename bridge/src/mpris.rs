//! MPRIS player tracking, pushed into jwm's shell.
//!
//! The compositor has no bus connection, so this module is the eyes and hands
//! of its media row: it watches every `org.mpris.MediaPlayer2.*` name on the
//! session bus, pushes the *active* player's state in over IPC, and turns
//! jwm's `media/command` broadcasts back into method calls.
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
    })
}

/// Sweep every MPRIS player currently on the bus and push the active one to
/// jwm. Pushing `player: null` is how "every player went away" is reported.
async fn publish(connection: &Connection, ipc: &JwmIpc) {
    let Ok(dbus) = DBusProxy::new(connection).await else {
        return;
    };
    let Ok(names) = dbus.list_names().await else {
        return;
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

    let args = match pick_active(&players) {
        Some(active) => active.to_args(),
        None => serde_json::json!({ "player": Value::Null }),
    };
    let ipc = ipc.clone();
    let _ = tokio::task::spawn_blocking(move || ipc.command("set_media_status", args)).await;
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
    publish(&connection, &ipc).await;

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
                let action = event
                    .get("payload")
                    .and_then(|payload| payload.get("action"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(method) = method_for(action) else {
                    log::warn!("ignoring unknown media action {action:?}");
                    continue;
                };
                // Re-resolve the active player instead of trusting a cached
                // one: the user may have switched players since the last push.
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
                    if let Some(active) = pick_active(&players) {
                        call_active(&connection, &active.player, method).await;
                    }
                }
                publish(&connection, &ipc).await;
            }
            Some(_) = next_owner_change(&mut owner_changes) => {
                publish(&connection, &ipc).await;
            }
            _ = poll.tick() => {
                publish(&connection, &ipc).await;
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
}
