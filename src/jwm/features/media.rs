//! Backend-neutral media-player state.
//!
//! JWM does not speak MPRIS itself — `jwm-bridge` watches the session bus and
//! pushes the active player's state in over IPC, and JWM broadcasts control
//! requests back out the same way. This module owns what the shell needs from
//! that: the last known track, whether it is playing, which players shared
//! the bus on the last sweep, and the pure formatting the control-center row
//! and the media OSD render.
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
/// Real sessions see a handful of MPRIS players at once; the cap keeps a
/// garbage list from an untrusted bridge from lingering in the state.
const MAX_PLAYERS: usize = 32;

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
    /// Player-reported track position in microseconds, as of the bridge's
    /// last poll. `None` when the player did not report one. Kept exactly as
    /// received — pauses do not advance it, and mid-track-change values can
    /// be garbage; only the display clamps, never the state.
    pub position_us: Option<i64>,
    /// `mpris:length` in microseconds. `None` when the track does not say —
    /// streams routinely do not.
    pub length_us: Option<i64>,
    /// Every MPRIS bus suffix the bridge's last sweep saw, in sweep order,
    /// this player included — the list the `p` key cycles through. Empty
    /// when the bridge is too old to send one, which reads exactly like a
    /// one-player session: no switch hint on the row, nothing for `p` to do.
    pub players: Vec<String>,
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

    /// The row's `2:41 / 4:05` suffix, when there is anything honest to show.
    ///
    /// Both halves must come from the player: a length without a position has
    /// no progress to show, and a position without a usable length (a stream)
    /// cannot be read as a fraction of anything — so either one missing means
    /// no suffix at all, never a placeholder. A position past the length is a
    /// stale read around a track change; it is clamped for this display only,
    /// never written back into the state. While paused the player does not
    /// advance `Position`, so the suffix simply holds the last polled value.
    #[must_use]
    pub fn position_label(&self) -> Option<String> {
        let length = self.length_us.filter(|length| *length > 0)?;
        let position = self.position_us?.clamp(0, length);
        Some(format!(
            "{} / {}",
            format_clock(position),
            format_clock(length)
        ))
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
    /// not pop up on every property change; position and length churn from
    /// the bridge's polling is deliberately not a change either, or the OSD
    /// would re-raise on every sweep.
    ///
    /// The state replaces wholesale, so a track change also resets the
    /// position to whatever the push that announced the new track carried —
    /// the previous track's counter can never linger into the new one.
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

/// `m:ss` under an hour, `h:mm:ss` past it — the same shape the recording
/// indicator's clock draws; that formatter lives backend-side of the
/// architecture boundary, so this tiny copy is the WM-side one. Negative
/// input, which players produce around track changes, reads as the start of
/// the track.
fn format_clock(micros: i64) -> String {
    let secs = micros.max(0) / 1_000_000;
    let (hours, minutes, seconds) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Truncate a label to the control-center row budget.
#[must_use]
pub fn clip_row_label(label: &str) -> String {
    clip_row_label_within(label, MAX_ROW_CHARS)
}

/// Truncate a label to a given budget; [`clip_row_label`] is this with the
/// row's full width. A position suffix takes its share out of the budget so
/// the row's total width — and the transport glyphs at its end — stays put.
#[must_use]
fn clip_row_label_within(label: &str, max_chars: usize) -> String {
    if label.chars().count() <= max_chars {
        return label.to_string();
    }
    let mut out: String = label.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// The text both now-playing rows share: the track label clipped to the
/// budget the position suffix leaves behind, and the suffix itself, so the
/// control center and the lock screen can never format the same state
/// differently.
fn row_text(state: &MediaState) -> (String, String) {
    let position = state
        .position_label()
        .map(|label| format!("  {label}"))
        .unwrap_or_default();
    let label = clip_row_label_within(
        &state.track_label(),
        MAX_ROW_CHARS.saturating_sub(position.chars().count()),
    );
    (label, position)
}

/// The player `p` would hand the row to: the one after `active` in the
/// bridge's reported list, wrapping back to the first — and the first when
/// `active` is not in the list at all, a stale read the bridge's next push
/// repairs. `None` with fewer than two players: there is nothing to switch
/// to, and the row shows no switch hint then either.
#[must_use]
pub fn next_player<'a>(players: &'a [String], active: &str) -> Option<&'a str> {
    if players.len() < 2 {
        return None;
    }
    let index = players
        .iter()
        .position(|player| player == active)
        .map_or(0, |index| index + 1);
    Some(players[index % players.len()].as_str())
}

/// What a pointer press on the control-center media row does. The keyboard
/// already owns Left/Right skip, Return play/pause, and `p` cycle; the
/// pointer mirrors each on the glyph that draws it. Track title and status
/// icon stay PlayPause so ordinary clicks are unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaRowClick {
    Previous,
    PlayPause,
    Next,
    Cycle,
}

/// The trailing switch hint `control_row` appends when `p` has somewhere to
/// go. `None` with fewer than two players — the row then has no clickable
/// switch zone.
#[must_use]
pub fn switch_hint(state: &MediaState) -> Option<String> {
    next_player(&state.players, &state.player)
        .map(|player| format!(" \u{b7} p {player}"))
}

/// Pieces of [`control_row_prefix`] measured separately so pointer hit-tests
/// land on the same glyphs the panel drew. Kept in lockstep with
/// [`control_row`] — a drift here would click the wrong transport.
struct ControlRowParts {
    /// Music glyph + title + position, ending just before the previous glyph.
    before_previous: String,
    /// `before_previous` plus the previous glyph (or its blank stand-in).
    with_previous: String,
    /// Through the status icon and the space before next.
    before_next: String,
    /// Full prefix (through the next glyph), without the switch hint.
    prefix: String,
}

fn control_row_parts(state: &MediaState) -> ControlRowParts {
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
    let (label, position) = row_text(state);
    let before_previous = format!("{}  {label}{position}   ", "\u{f001}"); // fa-music
    let with_previous = format!("{before_previous}{previous}");
    let before_next = format!("{with_previous} {} ", state.status.icon());
    let prefix = format!("{before_next}{next}");
    ControlRowParts {
        before_previous,
        with_previous,
        before_next,
        prefix,
    }
}

/// The control-center row without the switch suffix — the prefix a press to
/// the left of the hint measures against. Kept in lockstep with
/// [`control_row`] so hit-testing never drifts from what was drawn.
#[must_use]
fn control_row_prefix(state: &MediaState) -> String {
    control_row_parts(state).prefix
}

/// The control-center row: status icon, track, and which transport controls
/// the player says it supports. When the player reports both a position and
/// a length, the row carries them as `2:41 / 4:05` after the track label.
/// With more than one player on the bus the row ends with a `· p ‹next›`
/// hint naming the player the `p` key would switch to; a one-player row —
/// or one fed by an old bridge — stays byte-identical to before.
#[must_use]
pub fn control_row(state: &MediaState) -> String {
    let switch = switch_hint(state).unwrap_or_default();
    format!("{}{switch}", control_row_prefix(state))
}

/// Pointer counterparts of Left / Return / Right / `p` on the media row.
/// `measure` is the panel font's advance in px (the same probe the slider
/// and notification chips use); `TEXT_PAD` matches the texture margin baked
/// into those measurements so each zone starts where the drawn glyph starts.
///
/// Zones, left to right: title → previous glyph → status → next glyph →
/// optional `· p ‹next›`. A blank stand-in for a disabled skip still maps to
/// PlayPause, so a press there never invents a command the player refused.
#[must_use]
pub fn click_action(
    text_x_px: f32,
    measure: impl Fn(&str) -> f32,
    state: &MediaState,
) -> MediaRowClick {
    if !text_x_px.is_finite() {
        return MediaRowClick::PlayPause;
    }
    // Same pad the slider/chip hit-tests subtract: measure_ui_text_width
    // includes it on both ends, so the drawn glyph begins here.
    const TEXT_PAD: f32 = 2.0;
    let parts = control_row_parts(state);
    let previous_start = measure(&parts.before_previous) - TEXT_PAD;
    let previous_end = measure(&parts.with_previous) - TEXT_PAD;
    let next_start = measure(&parts.before_next) - TEXT_PAD;
    let next_end = measure(&parts.prefix) - TEXT_PAD;

    if text_x_px >= previous_start && text_x_px < previous_end {
        return if state.can_go_previous {
            MediaRowClick::Previous
        } else {
            MediaRowClick::PlayPause
        };
    }
    if text_x_px >= next_start && text_x_px < next_end {
        return if state.can_go_next {
            MediaRowClick::Next
        } else {
            MediaRowClick::PlayPause
        };
    }
    if let Some(switch) = switch_hint(state) {
        let switch_start = next_end;
        let switch_end = measure(&format!("{}{switch}", parts.prefix)) - TEXT_PAD;
        if text_x_px >= switch_start && text_x_px < switch_end {
            return MediaRowClick::Cycle;
        }
    }
    MediaRowClick::PlayPause
}

/// The lock screen's now-playing row: the control-center row minus its
/// transport cluster. The lock reveals only what the session's own control
/// center already shows — title, artist, position — and never controls (the
/// transport keys already work while locked; they need no on-screen cluster).
/// The status icon keeps its trailing place, so a paused player reads paused
/// exactly as it does in the control center. The control row's `p` switch
/// hint is a control too: it never appears here, so multi-player state
/// formats byte-identically to single-player.
#[must_use]
pub fn lock_row(state: &MediaState) -> String {
    let (label, position) = row_text(state);
    format!(
        "{}  {label}{position}   {}",
        "\u{f001}", // fa-music
        state.status.icon(),
    )
}

/// One Players picker row: a filled marker for the player in use, hollow
/// otherwise — the same grammar the audio device picker uses.
#[must_use]
pub fn player_picker_row(name: &str, active: bool) -> String {
    let marker = if active {
        "\u{f192}" // fa-dot-circle-o
    } else {
        "\u{f10c}" // fa-circle-o
    };
    format!("{marker}  {name}")
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
                // Append-only: microseconds as reported, plus the display
                // label so bars do not each reimplement the clamping rules.
                "position_us": state.position_us,
                "length_us": state.length_us,
                "position_label": state.position_label(),
                // Append-only: the sweep's player list so a bar can show a
                // picker without scraping the control-center row. Old bars
                // ignore the field; a one-player session still sends the
                // list (possibly empty) rather than omitting it.
                "players": state.players,
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
        // An open Players picker follows the bridge's re-publish the way an
        // audio picker follows a device switch: the marker moves, and the
        // selection holds on the same suffix when it is still listed.
        if self.features.system_ui.is_media_players_picker()
            && let Some(current) = self.features.media.get().cloned()
        {
            self.features.system_ui.set_media_players(&current);
            self.mark_system_ui_dirty();
        }
        // The lock screen mirrors the now-playing row while it is up, with
        // the lock clock's discipline: the bridge re-pushes this state on
        // every sweep, so the setter reports only a change to what the row
        // shows, and only then is the overlay re-synced. A paused player's
        // sweeps format to the same row and cost one comparison; an
        // unlocked session returns before even that.
        if self
            .features
            .system_ui
            .set_lock_now_playing(self.features.media.get())
        {
            self.sync_system_ui(backend);
        }
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

    /// Ask the bridge to pin the row — and the transport keys with it — to
    /// `player` (an MPRIS bus suffix). When the bridge reported a non-empty
    /// list, the suffix must be on it; an empty list (old bridge) accepts any
    /// non-empty name. The bridge re-publishes the pinned player's state.
    pub(crate) fn select_media_player(&mut self, player: &str) -> Result<(), String> {
        let player = player.trim();
        if player.is_empty() {
            return Err("no media player selected".to_string());
        }
        {
            let state = self
                .features
                .media
                .get()
                .ok_or("no media player is running")?;
            if !state.players.is_empty() && !state.players.iter().any(|name| name == player) {
                return Err(format!("unknown media player: {player}"));
            }
        }
        self.broadcast_ipc_event(
            "media/command",
            serde_json::json!({ "action": "select_player", "player": player }),
        );
        Ok(())
    }

    /// Pin the Players picker's selection and return to the hub so the media
    /// row can show the newly chosen player once the bridge re-publishes.
    pub(crate) fn apply_selected_media_player(
        &mut self,
        backend: &mut dyn crate::backend::api::Backend,
    ) {
        let Some(player) = self
            .features
            .system_ui
            .selected_media_player()
            .map(str::to_string)
        else {
            return;
        };
        if let Err(error) = self.select_media_player(&player) {
            log::debug!("media players picker: {error}");
            return;
        }
        self.return_to_shell_hub(backend);
    }

    /// Ask the bridge to hand the row — and the transport keys with it — to
    /// the next player in its reported list. The bridge re-publishes the
    /// pinned player's state, which rebuilds this row and raises the media
    /// OSD like any track change. The error is for the caller's log when
    /// there is nothing to switch to: no player, or only one.
    pub(crate) fn cycle_media_player(&mut self) -> Result<(), String> {
        let next = {
            let state = self
                .features
                .media
                .get()
                .ok_or("no media player is running")?;
            next_player(&state.players, &state.player)
                .ok_or("no other media player is running")?
                .to_string()
        };
        self.select_media_player(&next)
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
                "position_us": state.position_us,
                "length_us": state.length_us,
                "position_label": state.position_label(),
                "players": state.players,
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
    // Microsecond counters are optional on the wire: an old bridge never
    // sends them, and a player that did not report one sends null. Anything
    // that is not an integer — or does not fit one — reads as unreported.
    let micros = |key: &str| args.get(key).and_then(serde_json::Value::as_i64);
    // The sweep's full player list rides the same push, append-only: an old
    // bridge never sends it, and anything but a list of names reads as a
    // one-player session.
    let players = args
        .get("players")
        .and_then(serde_json::Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|player| !player.is_empty())
                .take(MAX_PLAYERS)
                .map(|player| bounded_text(player, MAX_PLAYER_BYTES))
                .collect()
        })
        .unwrap_or_default();
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
        position_us: micros("position_us"),
        length_us: micros("length_us"),
        players,
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
            position_us: None,
            length_us: None,
            players: Vec::new(),
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
        assert_eq!(parsed.position_us, None, "old bridges send no counters");
        assert_eq!(parsed.length_us, None);
        assert!(parsed.players.is_empty(), "old bridges send no player list");
    }

    #[test]
    fn state_args_parse_the_sweeps_player_list_append_only() {
        let parsed = parse_state_args(&serde_json::json!({
            "player": "mpv",
            "players": ["mpv", "spotify"],
        }))
        .expect("player parses");
        assert_eq!(
            parsed.players,
            vec!["mpv".to_string(), "spotify".to_string()]
        );

        // An empty list reads like a missing key: one-player behavior.
        let parsed = parse_state_args(&serde_json::json!({
            "player": "mpv",
            "players": [],
        }))
        .expect("player parses");
        assert!(parsed.players.is_empty());

        // Entries that are not names are dropped, not trusted, and the
        // active player is not forced into the list.
        let parsed = parse_state_args(&serde_json::json!({
            "player": "mpv",
            "players": ["mpv", 7, null, "  ", "spotify"],
        }))
        .expect("player parses");
        assert_eq!(
            parsed.players,
            vec!["mpv".to_string(), "spotify".to_string()]
        );
    }

    #[test]
    fn the_p_key_cycles_the_reported_players_in_sweep_order() {
        let players = || {
            vec![
                "mpv".to_string(),
                "spotify".to_string(),
                "firefox".to_string(),
            ]
        };
        assert_eq!(next_player(&players(), "mpv"), Some("spotify"));
        assert_eq!(next_player(&players(), "spotify"), Some("firefox"));
        assert_eq!(
            next_player(&players(), "firefox"),
            Some("mpv"),
            "wraps around"
        );
        // A stale active player — the sweep moved on — starts from the first.
        assert_eq!(next_player(&players(), "gone"), Some("mpv"));
    }

    #[test]
    fn the_p_key_has_nothing_to_cycle_with_fewer_than_two_players() {
        assert_eq!(next_player(&[], "mpv"), None);
        assert_eq!(next_player(&["mpv".to_string()], "mpv"), None);
    }

    #[test]
    fn the_row_advertises_the_p_key_only_with_another_player_to_switch_to() {
        let mut multi = state("Track", "Artist");
        multi.players = vec!["spotify".to_string(), "mpv".to_string()];
        let row = control_row(&multi);
        assert!(row.contains("\u{b7} p mpv"), "{row}");

        // One player — or an old bridge that sends no list — keeps the row
        // byte-identical to before switching existed.
        let mut single = state("Track", "Artist");
        single.players = vec!["spotify".to_string()];
        assert_eq!(control_row(&single), control_row(&state("Track", "Artist")));
        assert!(!control_row(&single).contains('\u{b7}'));
    }

    #[test]
    fn the_lock_row_never_grows_the_player_switch_hint() {
        let mut multi = state("Blue in Green", "Miles Davis");
        multi.players = vec!["spotify".to_string(), "mpv".to_string()];
        // Multi-player state formats byte-identically to single-player on
        // the lock screen: the hint is a control, and the lock shows none.
        assert_eq!(
            lock_row(&multi),
            lock_row(&state("Blue in Green", "Miles Davis"))
        );
        assert_eq!(
            lock_row(&multi),
            "\u{f001}  Blue in Green \u{2014} Miles Davis   \u{f04b}"
        );
    }

    /// Monospace stand-in for the panel font: one unit per char so the switch
    /// zone is exactly `prefix.len()` units wide after the TEXT_PAD cancel.
    fn mono(text: &str) -> f32 {
        // measure_ui_text_width includes TEXT_PAD on both ends; mimic that
        // so click_action's `- TEXT_PAD` lands on the glyph boundary.
        text.chars().count() as f32 + 4.0
    }

    #[test]
    fn a_press_on_the_switch_hint_cycles_and_the_title_plays() {
        let mut multi = state("Track", "Artist");
        multi.players = vec!["spotify".to_string(), "mpv".to_string()];
        let parts = control_row_parts(&multi);
        let switch = switch_hint(&multi).expect("multi-player has a switch");
        let switch_start = mono(&parts.prefix) - 2.0;
        let switch_end = mono(&format!("{}{switch}", parts.prefix)) - 2.0;

        assert_eq!(
            click_action(switch_start, mono, &multi),
            MediaRowClick::Cycle,
            "the first pixel of the hint cycles"
        );
        assert_eq!(
            click_action((switch_start + switch_end) * 0.5, mono, &multi),
            MediaRowClick::Cycle
        );
        assert_eq!(
            click_action(0.0, mono, &multi),
            MediaRowClick::PlayPause,
            "the title still plays"
        );
        assert_eq!(
            click_action(switch_end, mono, &multi),
            MediaRowClick::PlayPause,
            "past the hint is PlayPause (exclusive end)"
        );
    }

    #[test]
    fn a_press_on_the_transport_glyphs_skips() {
        let single = state("Track", "Artist");
        let parts = control_row_parts(&single);
        let previous_start = mono(&parts.before_previous) - 2.0;
        let previous_end = mono(&parts.with_previous) - 2.0;
        let next_start = mono(&parts.before_next) - 2.0;
        let next_end = mono(&parts.prefix) - 2.0;

        assert_eq!(
            click_action(previous_start, mono, &single),
            MediaRowClick::Previous,
            "the previous glyph skips back"
        );
        assert_eq!(
            click_action((previous_start + previous_end) * 0.5, mono, &single),
            MediaRowClick::Previous
        );
        assert_eq!(
            click_action(next_start, mono, &single),
            MediaRowClick::Next,
            "the next glyph skips forward"
        );
        assert_eq!(
            click_action((next_start + next_end) * 0.5, mono, &single),
            MediaRowClick::Next
        );
        assert_eq!(
            click_action(0.0, mono, &single),
            MediaRowClick::PlayPause,
            "the title still plays"
        );
        // Status icon sits between previous and next.
        let status_x = (previous_end + next_start) * 0.5;
        assert_eq!(
            click_action(status_x, mono, &single),
            MediaRowClick::PlayPause,
            "the status icon plays"
        );
    }

    #[test]
    fn a_blank_skip_stand_in_plays_instead_of_skipping() {
        let mut only_next = state("Track", "Artist");
        only_next.can_go_previous = false;
        let parts = control_row_parts(&only_next);
        let previous_start = mono(&parts.before_previous) - 2.0;
        assert_eq!(
            click_action(previous_start, mono, &only_next),
            MediaRowClick::PlayPause,
            "a hidden previous glyph must not invent a Previous command"
        );
        assert_eq!(
            click_action(mono(&parts.before_next) - 2.0, mono, &only_next),
            MediaRowClick::Next
        );
    }

    #[test]
    fn a_single_player_row_without_a_hint_never_cycles() {
        let single = state("Track", "Artist");
        assert!(switch_hint(&single).is_none());
        for x in [0.0, -1.0, f32::NAN, 500.0] {
            assert_ne!(
                click_action(x, mono, &single),
                MediaRowClick::Cycle,
                "x={x}"
            );
        }
    }

    #[test]
    fn control_row_is_prefix_plus_switch_hint() {
        let mut multi = state("Track", "Artist");
        multi.players = vec!["spotify".to_string(), "mpv".to_string()];
        assert_eq!(
            control_row(&multi),
            format!(
                "{}{}",
                control_row_prefix(&multi),
                switch_hint(&multi).unwrap()
            )
        );
        let single = state("Track", "Artist");
        assert_eq!(control_row(&single), control_row_prefix(&single));
    }

    #[test]
    fn state_args_parse_the_position_and_length_the_bridge_polls() {
        let parsed = parse_state_args(&serde_json::json!({
            "player": "mpv",
            "position_us": 161_000_000i64,
            "length_us": 245_000_000i64,
        }))
        .expect("player parses");
        assert_eq!(parsed.position_us, Some(161_000_000));
        assert_eq!(parsed.length_us, Some(245_000_000));
        assert_eq!(parsed.position_label().as_deref(), Some("2:41 / 4:05"));
    }

    #[test]
    fn state_args_read_null_or_garbage_counters_as_unreported() {
        let parsed = parse_state_args(&serde_json::json!({
            "player": "mpv",
            "position_us": null,
            "length_us": "forever",
        }))
        .expect("player parses");
        assert_eq!(parsed.position_us, None);
        assert_eq!(parsed.length_us, None);
        assert_eq!(parsed.position_label(), None);
    }

    #[test]
    fn the_clock_is_minute_seconds_until_an_hour() {
        assert_eq!(format_clock(0), "0:00");
        assert_eq!(format_clock(59_999_999), "0:59", "rounds down, not over");
        assert_eq!(format_clock(161_000_000), "2:41");
        assert_eq!(format_clock(245_000_000), "4:05");
        assert_eq!(format_clock(3_599_000_000), "59:59");
        assert_eq!(format_clock(3_600_000_000), "1:00:00");
        assert_eq!(format_clock(5_025_000_000), "1:23:45");
        assert_eq!(format_clock(-5_000_000), "0:00", "garbage reads as zero");
    }

    #[test]
    fn the_position_label_needs_both_halves_of_the_fraction() {
        let mut both = state("Track", "Artist");
        both.position_us = Some(161_000_000);
        both.length_us = Some(245_000_000);
        assert_eq!(both.position_label().as_deref(), Some("2:41 / 4:05"));

        let mut no_position = state("Track", "Artist");
        no_position.length_us = Some(245_000_000);
        assert_eq!(
            no_position.position_label(),
            None,
            "a length without a position shows no placeholder"
        );

        let mut no_length = state("Track", "Artist");
        no_length.position_us = Some(161_000_000);
        assert_eq!(no_length.position_label(), None);

        let mut stream = state("Track", "Artist");
        stream.position_us = Some(161_000_000);
        stream.length_us = Some(0);
        assert_eq!(stream.position_label(), None, "streams report no length");
    }

    #[test]
    fn the_displayed_position_is_clamped_but_the_state_keeps_what_was_said() {
        // Mid-track-change the position can be the previous track's: past the
        // new length. The row shows the end of the track, and the state keeps
        // the player's value untouched for the next update to compare.
        let mut stale = state("Track", "Artist");
        stale.position_us = Some(300_000_000);
        stale.length_us = Some(245_000_000);
        assert_eq!(stale.position_label().as_deref(), Some("4:05 / 4:05"));
        assert_eq!(stale.position_us, Some(300_000_000));

        let mut negative = state("Track", "Artist");
        negative.position_us = Some(-1_000_000);
        negative.length_us = Some(245_000_000);
        assert_eq!(negative.position_label().as_deref(), Some("0:00 / 4:05"));
        assert_eq!(negative.position_us, Some(-1_000_000));
    }

    #[test]
    fn the_row_carries_the_position_suffix_within_its_budget() {
        let mut timed = state("Track", "Artist");
        timed.position_us = Some(161_000_000);
        timed.length_us = Some(245_000_000);
        let row = control_row(&timed);
        assert!(row.contains("Track \u{2014} Artist  2:41 / 4:05"), "{row}");
        assert!(row.contains('\u{f04b}'), "transport glyphs survive");

        // The suffix's width comes out of the label's budget, so a long title
        // plus the suffix is exactly as wide as a long title alone was.
        let mut wordy = state(&"x".repeat(MAX_ROW_CHARS + 10), "");
        wordy.position_us = Some(161_000_000);
        wordy.length_us = Some(245_000_000);
        let row = control_row(&wordy);
        assert!(row.contains('\u{2026}'), "the label still clips");
        assert!(row.contains("2:41 / 4:05"), "the suffix is never clipped");

        // Nothing reported, nothing shown — no placeholder dashes.
        assert!(!control_row(&state("Track", "Artist")).contains(" / "));
    }

    #[test]
    fn position_churn_is_not_a_track_change() {
        let mut status = MediaStatus::default();
        let mut first = state("Track", "Artist");
        first.position_us = Some(10_000_000);
        first.length_us = Some(245_000_000);
        assert!(status.update(Some(first)));

        // The 3-second sweep pushes a fresh position for the same track; the
        // OSD must not re-raise for it.
        let mut later = state("Track", "Artist");
        later.position_us = Some(13_000_000);
        later.length_us = Some(245_000_000);
        assert!(!status.update(Some(later)));
        assert_eq!(status.get().unwrap().position_us, Some(13_000_000));
    }

    #[test]
    fn a_track_change_resets_the_position_to_the_new_push() {
        let mut status = MediaStatus::default();
        let mut first = state("First", "Artist");
        first.position_us = Some(200_000_000);
        first.length_us = Some(245_000_000);
        status.update(Some(first));

        // The push announcing the next track came before the player reported
        // its position: the old track's counter must not linger.
        let mut next = state("Second", "Artist");
        next.length_us = Some(180_000_000);
        assert!(status.update(Some(next)));
        let current = status.get().unwrap();
        assert_eq!(current.position_us, None);
        assert_eq!(current.position_label(), None);
    }

    #[test]
    fn the_lock_row_is_the_control_row_minus_its_transport_cluster() {
        let mut timed = state("Blue in Green", "Miles Davis");
        timed.position_us = Some(161_000_000);
        timed.length_us = Some(245_000_000);
        let row = lock_row(&timed);
        assert_eq!(
            row,
            "\u{f001}  Blue in Green \u{2014} Miles Davis  2:41 / 4:05   \u{f04b}"
        );
        // No transport glyphs: the lock screen shows what is playing, never
        // the controls.
        assert!(!row.contains('\u{f048}'));
        assert!(!row.contains('\u{f051}'));
        // The grammar is shared, not paraphrased: the control row extends
        // the lock row's text with its cluster.
        let prefix = row.strip_suffix('\u{f04b}').expect("status icon trails");
        assert!(control_row(&timed).starts_with(prefix));

        // Paused reads exactly as the control center's paused does; the
        // position holds its last poll.
        let mut paused = timed.clone();
        paused.status = PlaybackStatus::Paused;
        assert_eq!(
            lock_row(&paused),
            "\u{f001}  Blue in Green \u{2014} Miles Davis  2:41 / 4:05   \u{f04c}"
        );

        // Nothing reported, nothing shown — no suffix, no placeholder.
        assert_eq!(
            lock_row(&state("Track", "Artist")),
            "\u{f001}  Track \u{2014} Artist   \u{f04b}"
        );
    }

    #[test]
    fn the_lock_row_clips_the_label_and_keeps_the_suffix() {
        let mut wordy = state(&"x".repeat(MAX_ROW_CHARS + 10), "");
        wordy.position_us = Some(161_000_000);
        wordy.length_us = Some(245_000_000);
        let row = lock_row(&wordy);
        assert!(row.contains('\u{2026}'), "the label still clips");
        assert!(row.contains("2:41 / 4:05"), "the suffix is never clipped");
        assert!(row.ends_with('\u{f04b}'), "the status icon survives");
    }

    /// A backend built from the shared dummy ops, counting the WM's
    /// system-UI pushes and keeping the last overlay, so the lock screen's
    /// re-sync discipline can be asserted.
    struct SystemUiSpyBackend {
        window_ops: crate::backend::wayland_dummy_ops::DummyWindowOps,
        input_ops: crate::backend::wayland_dummy_ops::DummyInputOps,
        property_ops: crate::backend::wayland_dummy_ops::DummyPropertyOps,
        output_ops: crate::backend::wayland_dummy_ops::DummyOutputOps,
        key_ops: crate::backend::wayland_dummy_ops::DummyKeyOps,
        cursor_provider: crate::backend::wayland_dummy_ops::DummyCursorProvider,
        color_allocator: crate::backend::wayland_dummy_ops::DummyColorAllocator,
        overlay_pushes: usize,
        last_overlay: Option<crate::backend::api::SystemUiOverlay>,
        /// Every media OSD card the WM asked for, in order.
        media_osd_labels: Vec<String>,
    }

    impl SystemUiSpyBackend {
        fn new() -> Self {
            Self {
                window_ops: crate::backend::wayland_dummy_ops::DummyWindowOps,
                input_ops: crate::backend::wayland_dummy_ops::DummyInputOps,
                property_ops: crate::backend::wayland_dummy_ops::DummyPropertyOps,
                output_ops: crate::backend::wayland_dummy_ops::DummyOutputOps,
                key_ops: crate::backend::wayland_dummy_ops::DummyKeyOps,
                cursor_provider: crate::backend::wayland_dummy_ops::DummyCursorProvider,
                color_allocator: crate::backend::wayland_dummy_ops::DummyColorAllocator,
                overlay_pushes: 0,
                last_overlay: None,
                media_osd_labels: Vec::new(),
            }
        }
    }

    impl crate::backend::api::CompositorBenchmark for SystemUiSpyBackend {}
    impl crate::backend::api::BackendDiagnostics for SystemUiSpyBackend {}
    impl crate::backend::api::CompositorControl for SystemUiSpyBackend {}
    impl crate::backend::api::CompositorMedia for SystemUiSpyBackend {}
    impl crate::backend::api::CompositorWorkspaceEffects for SystemUiSpyBackend {
        fn compositor_set_system_ui(
            &mut self,
            overlay: Option<crate::backend::api::SystemUiOverlay>,
        ) {
            self.overlay_pushes += 1;
            self.last_overlay = overlay;
        }

        fn compositor_show_media_osd(&mut self, label: &str) {
            self.media_osd_labels.push(label.to_string());
        }
    }
    impl crate::backend::api::CompositorWindowEffects for SystemUiSpyBackend {}
    impl crate::backend::api::CompositorAnnotation for SystemUiSpyBackend {}
    impl crate::backend::api::DisplayControl for SystemUiSpyBackend {}
    impl crate::backend::api::RenderScheduler for SystemUiSpyBackend {}

    impl crate::backend::api::Backend for SystemUiSpyBackend {
        fn capabilities(&self) -> crate::backend::api::Capabilities {
            crate::backend::api::Capabilities::default()
        }

        fn root_window(&self) -> Option<crate::backend::common_define::WindowId> {
            Some(crate::backend::common_define::WindowId::from_raw(0))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn check_existing_wm(&self) -> Result<(), crate::backend::error::BackendError> {
            Ok(())
        }

        fn window_ops(&self) -> &dyn crate::backend::api::WindowOps {
            &self.window_ops
        }

        fn input_ops(&self) -> &dyn crate::backend::api::InputOps {
            &self.input_ops
        }

        fn property_ops(&self) -> &dyn crate::backend::api::PropertyOps {
            &self.property_ops
        }

        fn output_ops(&self) -> &dyn crate::backend::api::OutputOps {
            &self.output_ops
        }

        fn key_ops(&self) -> &dyn crate::backend::api::KeyOps {
            &self.key_ops
        }

        fn key_ops_mut(&mut self) -> &mut dyn crate::backend::api::KeyOps {
            &mut self.key_ops
        }

        fn cursor_provider(&mut self) -> &mut dyn crate::backend::api::CursorProvider {
            &mut self.cursor_provider
        }

        fn color_allocator(&mut self) -> &mut dyn crate::backend::api::ColorAllocator {
            &mut self.color_allocator
        }

        fn run(
            &mut self,
            _handler: &mut dyn crate::backend::api::EventHandler,
        ) -> Result<(), crate::backend::error::BackendError> {
            Ok(())
        }
    }

    fn playing_at(position_secs: i64) -> MediaState {
        MediaState {
            status: PlaybackStatus::Playing,
            position_us: Some(position_secs * 1_000_000),
            length_us: Some(245_000_000),
            ..state("Blue in Green", "Miles Davis")
        }
    }

    #[test]
    fn the_lock_screen_resyncs_only_when_the_pushed_row_changes() {
        let mut backend = SystemUiSpyBackend::new();
        let mut jwm = crate::Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");

        // Unlocked: a push updates the state but syncs no lock overlay.
        jwm.set_media_status(&mut backend, Some(playing_at(161)));
        assert_eq!(backend.overlay_pushes, 0);

        // Lock the screen; the next push is the first one the lock sees, and
        // the row appears with it.
        jwm.features.system_ui = crate::jwm::features::SystemUiState::lock();
        jwm.set_media_status(&mut backend, Some(playing_at(161)));
        assert_eq!(backend.overlay_pushes, 1);
        let overlay = backend.last_overlay.as_ref().expect("a locked overlay");
        assert!(overlay.locked);
        assert_eq!(
            overlay.items.last().map(String::as_str),
            Some("\u{f001}  Blue in Green \u{2014} Miles Davis  2:41 / 4:05   \u{f04b}")
        );

        // The sweep re-pushing an unchanged state costs no re-sync.
        jwm.set_media_status(&mut backend, Some(playing_at(161)));
        assert_eq!(backend.overlay_pushes, 1);
        // Pausing changes the icon once; a paused track's frozen position
        // then holds the row across every later sweep.
        let mut paused = playing_at(161);
        paused.status = PlaybackStatus::Paused;
        jwm.set_media_status(&mut backend, Some(paused.clone()));
        assert_eq!(backend.overlay_pushes, 2);
        jwm.set_media_status(&mut backend, Some(paused));
        assert_eq!(backend.overlay_pushes, 2);
        // A playing track's position advances every sweep, and the label
        // changes with it: an honest repaint each time.
        jwm.set_media_status(&mut backend, Some(playing_at(164)));
        assert_eq!(backend.overlay_pushes, 3);
        let overlay = backend.last_overlay.as_ref().expect("a locked overlay");
        assert!(
            overlay
                .items
                .last()
                .is_some_and(|row| row.contains("2:44 / 4:05"))
        );

        // The player going away drops the row in one re-sync.
        jwm.set_media_status(&mut backend, None);
        assert_eq!(backend.overlay_pushes, 4);
        let overlay = backend.last_overlay.as_ref().expect("a locked overlay");
        assert!(!overlay.items.iter().any(|row| row.contains('\u{f001}')));
        jwm.set_media_status(&mut backend, None);
        assert_eq!(backend.overlay_pushes, 4);

        // After the unlock the pushes are free again: the row lived inside
        // the lock state, so nothing extra needed clearing.
        jwm.features.system_ui = crate::jwm::features::SystemUiState::Inactive;
        jwm.set_media_status(&mut backend, Some(playing_at(167)));
        assert_eq!(backend.overlay_pushes, 4);
    }

    #[test]
    fn a_player_switch_raises_the_media_osd_like_a_track_change() {
        let mut backend = SystemUiSpyBackend::new();
        let mut jwm = crate::Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");

        // The bridge's answer to `select_player` is a re-publish naming the
        // pinned player: a different player on the push counts as a track
        // change, so a switch raises the OSD through the existing path.
        let mut mpv = state("Track", "Artist");
        mpv.player = "mpv".into();
        jwm.set_media_status(&mut backend, Some(mpv));
        jwm.set_media_status(&mut backend, Some(state("Track", "Artist")));
        assert_eq!(backend.media_osd_labels.len(), 2, "one card per switch");
        assert!(
            backend
                .media_osd_labels
                .iter()
                .all(|label| label.contains("Track"))
        );
        // Re-pushing the switched-to player is churn, not a switch.
        jwm.set_media_status(&mut backend, Some(state("Track", "Artist")));
        assert_eq!(backend.media_osd_labels.len(), 2);
    }

    #[test]
    fn cycling_the_player_is_a_no_op_without_another_player() {
        let mut backend = SystemUiSpyBackend::new();
        let mut jwm = crate::Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");

        // No player yet, then one player: `p` reports there is nothing to
        // switch to and nothing is broadcast.
        assert!(jwm.cycle_media_player().is_err());
        jwm.set_media_status(&mut backend, Some(state("Track", "Artist")));
        assert!(jwm.cycle_media_player().is_err());

        // Two players on the bus: the broadcast goes out (to no IPC client
        // in this test), and the bridge's re-publish is what moves the row.
        let mut multi = state("Track", "Artist");
        multi.players = vec!["spotify".to_string(), "mpv".to_string()];
        jwm.set_media_status(&mut backend, Some(multi));
        assert!(jwm.cycle_media_player().is_ok());
    }

    #[test]
    fn player_picker_row_marks_the_active_player() {
        assert!(player_picker_row("mpv", true).starts_with('\u{f192}'));
        assert!(player_picker_row("spotify", false).starts_with('\u{f10c}'));
        assert!(player_picker_row("mpv", true).ends_with("mpv"));
    }

    #[test]
    fn select_media_player_broadcasts_the_select_player_command() {
        // The payload shape is the bridge's select_player contract; pins are
        // assembled at runtime so this test cannot match its own source.
        const SOURCE: &str = include_str!("media.rs");
        let body = SOURCE
            .split_once("fn select_media_player(")
            .expect("select_media_player")
            .1
            .split_once("fn apply_selected_media_player(")
            .expect("the function that follows it")
            .0;
        assert!(
            body.contains(r#""action": "select_player""#),
            "select_media_player lost the select_player action"
        );
        assert!(
            body.contains(r#""player": player"#),
            "select_media_player no longer names the chosen suffix"
        );

        let mut backend = SystemUiSpyBackend::new();
        let mut jwm = crate::Jwm::new_with_runtime_backend(&mut backend, "test").expect("test jwm");
        assert!(jwm.select_media_player("mpv").is_err());

        let mut multi = state("Track", "Artist");
        multi.players = vec!["spotify".to_string(), "mpv".to_string()];
        jwm.set_media_status(&mut backend, Some(multi));
        assert!(jwm.select_media_player("").is_err());
        assert!(jwm.select_media_player("vlc").is_err());
        assert!(jwm.select_media_player("mpv").is_ok());
        // Cycle rides the same path.
        assert!(jwm.cycle_media_player().is_ok());
    }
}
