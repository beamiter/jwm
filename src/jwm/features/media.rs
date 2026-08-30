//! Backend-neutral media-player state.
//!
//! JWM does not speak MPRIS itself — `jwm-bridge` watches the session bus and
//! pushes the active player's state in over IPC, and JWM broadcasts control
//! requests back out the same way. This module owns what the shell needs from
//! that: the last known track, whether it is playing, and the pure formatting
//! the control-center row and the media OSD render.
//!
//! Keeping it pure means the row text, the OSD label, and the
//! "is this control even available" decisions are unit tested without a bus.

/// Longest track label the control center row shows before ellipsis.
const MAX_ROW_CHARS: usize = 44;
/// D-Bus bus names are at most 255 bytes; retaining more cannot identify a
/// real MPRIS player and only amplifies an untrusted bridge update.
const MAX_PLAYER_BYTES: usize = 255;
/// Keep metadata generous for long podcast titles while bounding the state,
/// OSD label, and status event derived from one bridge message.
const MAX_METADATA_BYTES: usize = 4 * 1024;

fn bounded_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    #[default]
    Stopped,
}

impl PlaybackStatus {
    /// Parse MPRIS's `PlaybackStatus` property. Unknown values read as
    /// stopped rather than failing the whole update.
    #[must_use]
    pub fn from_mpris(value: &str) -> Self {
        match value {
            "Playing" => Self::Playing,
            "Paused" => Self::Paused,
            _ => Self::Stopped,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
        }
    }

    /// Icon shown in the control center and the OSD.
    #[must_use]
    pub fn icon(self) -> &'static str {
        match self {
            Self::Playing => "\u{f04b}", // fa-play
            Self::Paused => "\u{f04c}",  // fa-pause
            Self::Stopped => "\u{f04d}", // fa-stop
        }
    }
}

/// What one control request asks the active player to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    PlayPause,
    Next,
    Previous,
    Stop,
}

impl MediaCommand {
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "play_pause" | "playpause" | "toggle" => Some(Self::PlayPause),
            "next" => Some(Self::Next),
            "previous" | "prev" => Some(Self::Previous),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PlayPause => "play_pause",
            Self::Next => "next",
            Self::Previous => "previous",
            Self::Stop => "stop",
        }
    }
}

/// The active player as the shell last heard about it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaState {
    /// MPRIS bus suffix, e.g. `spotify`. Identifies the player across updates.
    pub player: String,
    /// Human-readable player name, when it published one.
    pub identity: String,
    pub status: PlaybackStatus,
    pub title: String,
    pub artist: String,
    pub can_go_next: bool,
    pub can_go_previous: bool,
}

impl MediaState {
    /// `Title — Artist`, falling back to whichever half exists, then to the
    /// player's own name so the row is never blank.
    #[must_use]
    pub fn track_label(&self) -> String {
        let title = self.title.trim();
        let artist = self.artist.trim();
        match (title.is_empty(), artist.is_empty()) {
            (false, false) => format!("{title} \u{2014} {artist}"),
            (false, true) => title.to_string(),
            (true, false) => artist.to_string(),
            (true, true) => {
                let name = if self.identity.trim().is_empty() {
                    self.player.trim()
                } else {
                    self.identity.trim()
                };
                if name.is_empty() {
                    "Unknown track".to_string()
                } else {
                    name.to_string()
                }
            }
        }
    }

    /// Label for the OSD card: the status icon plus the track.
    #[must_use]
    pub fn osd_label(&self) -> String {
        format!("{}  {}", self.status.icon(), self.track_label())
    }

    /// Whether a play/pause request makes sense at all.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.player.trim().is_empty()
    }
}

/// Last known player, or none when every player went away.
#[derive(Debug, Default)]
pub struct MediaStatus {
    current: Option<MediaState>,
}

impl MediaStatus {
    #[must_use]
    pub fn get(&self) -> Option<&MediaState> {
        self.current.as_ref()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.current.is_none()
    }

    /// Replace the state, reporting whether this counts as a *track change* —
    /// a different player, or a different track on the same one. Volume-style
    /// churn (pause/resume of the same track) returns false so the OSD does
    /// not pop up on every property change.
    pub fn update(&mut self, state: Option<MediaState>) -> bool {
        let changed = match (&self.current, &state) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(previous), Some(next)) => {
                previous.player != next.player
                    || previous.title != next.title
                    || previous.artist != next.artist
            }
        };
        self.current = state;
        changed
    }
}

/// Truncate a label to the control-center row budget.
#[must_use]
pub fn clip_row_label(label: &str) -> String {
    if label.chars().count() <= MAX_ROW_CHARS {
        return label.to_string();
    }
    let mut out: String = label.chars().take(MAX_ROW_CHARS - 1).collect();
    out.push('\u{2026}');
    out
}

/// The control-center row: status icon, track, and which transport controls
/// the player says it supports.
#[must_use]
pub fn control_row(state: &MediaState) -> String {
    let previous = if state.can_go_previous {
        "\u{f048}" // fa-step-backward
    } else {
        " "
    };
    let next = if state.can_go_next {
        "\u{f051}" // fa-step-forward
    } else {
        " "
    };
    format!(
        "{}  {}   {previous} {} {next}",
        "\u{f001}", // fa-music
        clip_row_label(&state.track_label()),
        state.status.icon(),
    )
}

impl crate::jwm::Jwm {
    /// Adopt a state push from the bridge. A track change raises the media
    /// OSD; pause/resume of the same track does not, so the card is not in the
    /// way during ordinary transport use.
    pub(crate) fn set_media_status(
        &mut self,
        backend: &mut dyn crate::backend::api::Backend,
        state: Option<MediaState>,
    ) {
        let payload = match &state {
            Some(state) => serde_json::json!({
                "player": state.player,
                "identity": state.identity,
                "status": state.status.as_str(),
                "title": state.title,
                "artist": state.artist,
                "can_go_next": state.can_go_next,
                "can_go_previous": state.can_go_previous,
            }),
            None => serde_json::json!({ "player": serde_json::Value::Null }),
        };
        let track_changed = self.features.media.update(state);
        if track_changed
            && let Some(current) = self.features.media.get()
            && current.status == PlaybackStatus::Playing
        {
            backend.compositor_show_media_osd(&current.osd_label());
        }
        self.refresh_open_control_center();
        self.broadcast_ipc_event("media/status", payload);
    }

    /// Ask the bridge to drive the active player. JWM never talks to MPRIS
    /// itself, so this is a broadcast; the error is for the caller's benefit
    /// when no player has ever reported in.
    pub(crate) fn send_media_command(&mut self, command: MediaCommand) -> Result<(), String> {
        if self
            .features
            .media
            .get()
            .is_none_or(|state| !state.is_active())
        {
            return Err("no media player is running".to_string());
        }
        self.broadcast_ipc_event(
            "media/command",
            serde_json::json!({ "action": command.as_str() }),
        );
        Ok(())
    }

    /// JSON snapshot for the `get_media_status` query.
    pub(crate) fn media_status_json(&self) -> serde_json::Value {
        match self.features.media.get() {
            Some(state) => serde_json::json!({
                "active": true,
                "player": state.player,
                "identity": state.identity,
                "status": state.status.as_str(),
                "title": state.title,
                "artist": state.artist,
                "label": state.track_label(),
                "can_go_next": state.can_go_next,
                "can_go_previous": state.can_go_previous,
            }),
            None => serde_json::json!({ "active": false }),
        }
    }
}

/// Parse the `set_media_status` arguments. `player` missing or null clears the
/// state; that is how the bridge reports "every player went away".
#[must_use]
pub fn parse_state_args(args: &serde_json::Value) -> Option<MediaState> {
    let player = args.get("player")?.as_str()?.trim();
    if player.is_empty() {
        return None;
    }
    let text = |key: &str| {
        let value = args
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        bounded_text(value, MAX_METADATA_BYTES)
    };
    let flag = |key: &str| {
        args.get(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    Some(MediaState {
        player: bounded_text(player, MAX_PLAYER_BYTES),
        identity: text("identity"),
        status: PlaybackStatus::from_mpris(
            args.get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Stopped"),
        ),
        title: text("title"),
        artist: text("artist"),
        can_go_next: flag("can_go_next"),
        can_go_previous: flag("can_go_previous"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(title: &str, artist: &str) -> MediaState {
        MediaState {
            player: "spotify".into(),
            identity: "Spotify".into(),
            status: PlaybackStatus::Playing,
            title: title.into(),
            artist: artist.into(),
            can_go_next: true,
            can_go_previous: true,
        }
    }

    #[test]
    fn mpris_status_parses_known_values_and_defaults_to_stopped() {
        assert_eq!(
            PlaybackStatus::from_mpris("Playing"),
            PlaybackStatus::Playing
        );
        assert_eq!(PlaybackStatus::from_mpris("Paused"), PlaybackStatus::Paused);
        assert_eq!(
            PlaybackStatus::from_mpris("Stopped"),
            PlaybackStatus::Stopped
        );
        assert_eq!(
            PlaybackStatus::from_mpris("nonsense"),
            PlaybackStatus::Stopped
        );
    }

    #[test]
    fn command_names_cover_the_aliases_bars_use() {
        assert_eq!(
            MediaCommand::from_name("play_pause"),
            Some(MediaCommand::PlayPause)
        );
        assert_eq!(
            MediaCommand::from_name("toggle"),
            Some(MediaCommand::PlayPause)
        );
        assert_eq!(
            MediaCommand::from_name("prev"),
            Some(MediaCommand::Previous)
        );
        assert_eq!(MediaCommand::from_name("rewind"), None);
    }

    #[test]
    fn track_label_joins_title_and_artist() {
        assert_eq!(
            state("Blue in Green", "Miles Davis").track_label(),
            "Blue in Green \u{2014} Miles Davis"
        );
    }

    #[test]
    fn track_label_falls_back_through_artist_then_player_identity() {
        assert_eq!(state("Solo", "").track_label(), "Solo");
        assert_eq!(state("", "Miles Davis").track_label(), "Miles Davis");
        assert_eq!(state("", "").track_label(), "Spotify");

        let mut anonymous = state("", "");
        anonymous.identity = String::new();
        assert_eq!(anonymous.track_label(), "spotify");
    }

    #[test]
    fn a_player_without_any_name_still_labels_the_row() {
        let mut nameless = state("", "");
        nameless.identity = String::new();
        nameless.player = String::new();
        assert_eq!(nameless.track_label(), "Unknown track");
    }

    #[test]
    fn long_labels_are_clipped_for_the_row() {
        let clipped = clip_row_label(&"x".repeat(MAX_ROW_CHARS + 10));
        assert_eq!(clipped.chars().count(), MAX_ROW_CHARS);
        assert!(clipped.ends_with('\u{2026}'));
    }

    #[test]
    fn the_row_marks_unavailable_transport_controls() {
        let mut only_next = state("Track", "Artist");
        only_next.can_go_previous = false;
        let row = control_row(&only_next);

        assert!(row.contains('\u{f051}'), "next arrow present");
        assert!(!row.contains('\u{f048}'), "previous arrow hidden");
    }

    #[test]
    fn a_new_track_counts_as_a_change_but_a_pause_does_not() {
        let mut status = MediaStatus::default();
        assert!(status.update(Some(state("First", "Artist"))));

        let mut paused = state("First", "Artist");
        paused.status = PlaybackStatus::Paused;
        assert!(!status.update(Some(paused)));

        assert!(status.update(Some(state("Second", "Artist"))));
    }

    #[test]
    fn switching_players_counts_as_a_change() {
        let mut status = MediaStatus::default();
        status.update(Some(state("Track", "Artist")));

        let mut other = state("Track", "Artist");
        other.player = "mpv".into();
        assert!(status.update(Some(other)));
    }

    #[test]
    fn clearing_the_player_empties_the_status_without_an_osd() {
        let mut status = MediaStatus::default();
        status.update(Some(state("Track", "Artist")));
        assert!(!status.update(None));
        assert!(status.is_empty());
        assert!(status.get().is_none());
    }

    #[test]
    fn the_osd_label_carries_the_status_icon() {
        let label = state("Track", "Artist").osd_label();
        assert!(label.starts_with('\u{f04b}'));
        assert!(label.ends_with("Track \u{2014} Artist"));
    }

    #[test]
    fn a_player_without_a_bus_name_is_not_active() {
        assert!(state("a", "b").is_active());
        assert!(!MediaState::default().is_active());
    }

    #[test]
    fn state_args_round_trip_the_fields_the_bridge_sends() {
        let parsed = parse_state_args(&serde_json::json!({
            "player": "spotify",
            "identity": "Spotify",
            "status": "Playing",
            "title": "Blue in Green",
            "artist": "Miles Davis",
            "can_go_next": true,
            "can_go_previous": false,
        }))
        .expect("a named player parses");

        assert_eq!(parsed.player, "spotify");
        assert_eq!(parsed.status, PlaybackStatus::Playing);
        assert_eq!(parsed.track_label(), "Blue in Green \u{2014} Miles Davis");
        assert!(parsed.can_go_next);
        assert!(!parsed.can_go_previous);
    }

    #[test]
    fn state_args_bound_external_text_without_splitting_utf8() {
        let player = "p".repeat(MAX_PLAYER_BYTES + 10);
        let metadata = "€".repeat(MAX_METADATA_BYTES / 3 + 10);
        let parsed = parse_state_args(&serde_json::json!({
            "player": player,
            "identity": metadata,
            "title": metadata,
            "artist": metadata,
        }))
        .expect("bounded player parses");

        assert_eq!(parsed.player.len(), MAX_PLAYER_BYTES);
        for value in [&parsed.identity, &parsed.title, &parsed.artist] {
            assert_eq!(value.len(), MAX_METADATA_BYTES - MAX_METADATA_BYTES % 3);
        }
        assert!(parsed.track_label().len() <= MAX_METADATA_BYTES * 2 + 5);
    }

    #[test]
    fn state_args_without_a_player_clear_the_state() {
        assert!(parse_state_args(&serde_json::json!({})).is_none());
        assert!(parse_state_args(&serde_json::json!({ "player": null })).is_none());
        assert!(parse_state_args(&serde_json::json!({ "player": "  " })).is_none());
    }

    #[test]
    fn state_args_tolerate_missing_optional_fields() {
        let parsed =
            parse_state_args(&serde_json::json!({ "player": "mpv" })).expect("player parses");

        assert_eq!(parsed.status, PlaybackStatus::Stopped);
        assert_eq!(parsed.title, "");
        assert!(!parsed.can_go_next);
    }
}
