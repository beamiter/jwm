//! MPRIS state comes only from mpv events and replies, never queued commands.

use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Track {
    pub id: u64,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct State {
    pub alive: bool,
    pub loaded: bool,
    pub paused: bool,
    pub idle: bool,
    pub seekable: bool,
    pub position_us: i64,
    pub duration_us: Option<i64>,
    pub volume: f64,
    pub muted: bool,
    pub fullscreen: bool,
    pub rate: f64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub path: String,
    pub track_id: Option<u64>,
    pub playlist_pos: Option<usize>,
    pub playlist: Vec<Track>,
    pub loop_file: bool,
    pub loop_playlist: bool,
    pub seek_serial: u64,
    pub error_serial: u64,
    pub error: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            alive: true,
            loaded: false,
            paused: false,
            idle: true,
            seekable: false,
            position_us: 0,
            duration_us: None,
            volume: 1.0,
            muted: false,
            fullscreen: false,
            rate: 1.0,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            path: String::new(),
            track_id: None,
            playlist_pos: None,
            playlist: Vec::new(),
            loop_file: false,
            loop_playlist: false,
            seek_serial: 0,
            error_serial: 0,
            error: None,
        }
    }
}

pub fn microseconds(value: &Value) -> Option<i64> {
    let seconds = value.as_f64()?;
    (seconds.is_finite() && seconds >= 0.0 && seconds < i64::MAX as f64 / 1_000_000.0)
        .then_some((seconds * 1_000_000.0).round() as i64)
}

fn text(value: &Value) -> String {
    value
        .as_str()
        .unwrap_or_default()
        .chars()
        .take(4096)
        .collect()
}

impl State {
    pub fn status(&self) -> &'static str {
        if !self.alive || !self.loaded || self.idle {
            "Stopped"
        } else if self.paused {
            "Paused"
        } else {
            "Playing"
        }
    }

    pub fn track_path(&self) -> String {
        self.track_id.map_or_else(
            || "/org/mpris/MediaPlayer2/TrackList/NoTrack".into(),
            |id| format!("/org/jwm/Player/track_{id}"),
        )
    }

    pub fn can_next(&self) -> bool {
        self.playlist_pos
            .is_some_and(|pos| pos + 1 < self.playlist.len())
    }

    pub fn can_previous(&self) -> bool {
        self.playlist_pos.is_some_and(|pos| pos > 0)
    }

    pub fn loop_status(&self) -> &'static str {
        if self.loop_file {
            "Track"
        } else if self.loop_playlist {
            "Playlist"
        } else {
            "None"
        }
    }

    pub fn failure(&mut self, error: impl Into<String>) {
        self.error = Some(error.into().chars().take(1024).collect());
        self.error_serial = self.error_serial.wrapping_add(1);
    }

    pub fn event(&mut self, message: &Value) {
        match message.get("event").and_then(Value::as_str) {
            Some("property-change") => {
                self.property(
                    message
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    &message["data"],
                );
            }
            Some("start-file") => {
                self.track_id = message.get("playlist_entry_id").and_then(Value::as_u64);
                self.loaded = false;
                self.position_us = 0;
                self.duration_us = None;
                self.seekable = false;
                // Retain the last diagnostic until the publisher observes
                // its serial. A failed file may advance to the next track
                // before the next UI tick; clearing here would lose it.
            }
            Some("file-loaded") => {
                self.loaded = true;
                self.idle = false;
            }
            Some("end-file") => {
                self.loaded = false;
                self.seekable = false;
                self.position_us = 0;
                if message["reason"] == "error" {
                    self.failure(format!(
                        "Unable to play {}: {}",
                        self.path,
                        message["file_error"]
                            .as_str()
                            .unwrap_or("mpv could not decode the file")
                    ));
                }
            }
            // The worker publishes Seeked only after querying the post-seek
            // clock. Neither this event nor a normal file load guesses it.
            Some("shutdown") => {
                self.alive = false;
                self.loaded = false;
            }
            _ => {}
        }
    }

    pub fn property(&mut self, name: &str, value: &Value) {
        match name {
            "pause" => self.paused = value.as_bool().unwrap_or(false),
            "idle-active" => self.idle = value.as_bool().unwrap_or(true),
            "time-pos" => self.position_us = microseconds(value).unwrap_or(0),
            "duration" => self.duration_us = microseconds(value),
            "seekable" => self.seekable = value.as_bool().unwrap_or(false),
            "volume" => {
                if let Some(volume) = value.as_f64().filter(|v| v.is_finite()) {
                    self.volume = (volume / 100.0).clamp(0.0, 1.0);
                }
            }
            "mute" => self.muted = value.as_bool().unwrap_or(false),
            "fullscreen" => self.fullscreen = value.as_bool().unwrap_or(false),
            "speed" => {
                if let Some(rate) = value.as_f64().filter(|v| v.is_finite()) {
                    self.rate = rate;
                }
            }
            "media-title" => self.title = text(value),
            "path" => self.path = text(value),
            "metadata" => {
                let field = |key: &str| {
                    value.as_object().and_then(|map| {
                        map.iter()
                            .find(|(name, _)| name.eq_ignore_ascii_case(key))
                            .map(|(_, v)| v)
                    })
                };
                self.artist = field("artist").map(text).unwrap_or_default();
                self.album = field("album").map(text).unwrap_or_default();
            }
            "playlist-pos" => {
                self.playlist_pos = value.as_u64().and_then(|pos| usize::try_from(pos).ok())
            }
            "playlist" => {
                self.playlist = value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(super::cli::MAX_FILES)
                    .filter_map(|item| {
                        Some(Track {
                            id: item.get("id")?.as_u64()?,
                            path: item.get("filename")?.as_str()?.to_owned(),
                        })
                    })
                    .collect();
            }
            "loop-file" => self.loop_file = value == "inf" || value.as_i64().is_some_and(|n| n > 0),
            "loop-playlist" => {
                self.loop_playlist = value == "inf" || value.as_i64().is_some_and(|n| n > 0)
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn loaded_and_error_events_determine_status() {
        let mut state = State::default();
        state.event(&json!({"event":"start-file","playlist_entry_id":7}));
        assert_eq!(state.status(), "Stopped");
        state.event(&json!({"event":"file-loaded"}));
        assert_eq!(state.status(), "Playing");
        state.property("pause", &json!(true));
        assert_eq!(state.status(), "Paused");
        state.event(
            &json!({"event":"end-file","reason":"error","file_error":"unrecognized file format"}),
        );
        assert_eq!(state.status(), "Stopped");
        assert!(
            state
                .error
                .as_deref()
                .unwrap()
                .contains("unrecognized file format")
        );
        assert_eq!(state.track_path(), "/org/jwm/Player/track_7");
    }

    #[test]
    fn absent_and_negative_times_never_become_invalid_mpris_positions() {
        for value in [Value::Null, json!(-1), json!("unknown"), json!(1e30)] {
            assert_eq!(microseconds(&value), None);
        }
        assert_eq!(microseconds(&json!(1.234567)), Some(1_234_567));
        let mut state = State::default();
        state.property("duration", &json!(5));
        state.property("duration", &Value::Null);
        assert_eq!(state.duration_us, None);
    }

    #[test]
    fn playlist_bounds_and_case_insensitive_metadata() {
        let mut state = State::default();
        state.property(
            "playlist",
            &json!([{"id":1,"filename":"/a"},{"id":2,"filename":"/b"}]),
        );
        state.property("playlist-pos", &json!(0));
        assert!(state.can_next());
        assert!(!state.can_previous());
        state.property("playlist-pos", &json!(-1));
        assert!(!state.can_next());
        state.property("metadata", &json!({"ARTIST":"Artist","album":"Album"}));
        assert_eq!(state.artist, "Artist");
        assert_eq!(state.album, "Album");
    }
}
