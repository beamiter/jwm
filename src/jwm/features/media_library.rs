//! Local media discovery and bounded, shell-free player requests.
//!
//! Filesystem work runs on a worker, never on the compositor event thread.
//! The player owns decoding and exposes MPRIS to the existing bridge.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::connectivity::BackgroundJob;

pub const MAX_QUEUE_FILES: usize = 512;
const MAX_REQUEST_BYTES: usize = 128 * 1024;
const MAX_FILES: usize = 2_000;
const MAX_ENTRIES: usize = 20_000;
const MAX_DIRECTORIES: usize = 512;
const MAX_DEPTH: usize = 8;
const SCAN_BUDGET: Duration = Duration::from_secs(2);
pub const CACHE_AGE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Music,
    Video,
}

impl MediaKind {
    pub const ALL: [Self; 2] = [Self::Music, Self::Video];

    pub const fn index(self) -> usize {
        match self {
            Self::Music => 0,
            Self::Video => 1,
        }
    }

    pub const fn mode(self) -> &'static str {
        match self {
            Self::Music => "music",
            Self::Video => "video",
        }
    }

    pub const fn flag(self) -> &'static str {
        match self {
            Self::Music => "--music",
            Self::Video => "--video",
        }
    }

    pub fn accepts(self, path: &Path) -> bool {
        let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
            return false;
        };
        let extension = extension.to_ascii_lowercase();
        match self {
            Self::Music => matches!(
                extension.as_str(),
                "mp3"
                    | "flac"
                    | "wav"
                    | "ogg"
                    | "oga"
                    | "opus"
                    | "m4a"
                    | "aac"
                    | "aiff"
                    | "aif"
                    | "ape"
                    | "wv"
                    | "alac"
            ),
            Self::Video => matches!(
                extension.as_str(),
                "mp4"
                    | "mkv"
                    | "webm"
                    | "mov"
                    | "avi"
                    | "m4v"
                    | "mpeg"
                    | "mpg"
                    | "ts"
                    | "mts"
                    | "m2ts"
                    | "ogv"
                    | "wmv"
                    | "flv"
                    | "gif"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaEntry {
    pub path: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct MediaCatalog {
    pub kind: MediaKind,
    pub entries: Arc<[MediaEntry]>,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct MediaLibrary {
    pub catalogs: [Option<MediaCatalog>; 2],
    pub scans: [Option<BackgroundJob<MediaCatalog>>; 2],
    pub refreshed_at: [Option<Instant>; 2],
    pub launch: Option<BackgroundJob<Result<std::process::Command, String>>>,
}

fn display_text(value: &str) -> String {
    value
        .chars()
        .take(160)
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

/// Resolve user directories on the scan worker. Include the actual recorder's
/// default and configured locations as well as localized XDG user directories.
pub fn roots(kind: MediaKind, recording_directory: &str) -> Vec<PathBuf> {
    let home = dirs::home_dir();
    let mut roots = Vec::new();
    let (variable, folder, system) = match kind {
        MediaKind::Music => ("XDG_MUSIC_DIR", "Music", dirs::audio_dir()),
        MediaKind::Video => ("XDG_VIDEOS_DIR", "Videos", dirs::video_dir()),
    };
    if let Some(system) = system {
        roots.push(system);
    }
    if let Some(path) = std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        roots.push(path);
    }
    if let Some(home) = &home {
        roots.push(home.join(folder));
    }
    if let Ok(Some(path)) = super::recording_plan::configured_directory(
        recording_directory,
        "recording output directory",
        home.as_deref(),
    ) {
        roots.push(path);
    }
    roots
}

/// Iterative discovery does not follow directory symlinks. Count, depth and
/// time limits keep enormous folders bounded; a slow mount can occupy at most
/// the one admitted worker for this library, while the desktop stays usable.
pub fn scan(kind: MediaKind, roots: &[PathBuf]) -> MediaCatalog {
    let started = Instant::now();
    let mut directories = VecDeque::new();
    let mut seen_roots = BTreeSet::new();
    for root in roots {
        if let Ok(root) = root.canonicalize()
            && root.is_dir()
            && seen_roots.insert(root.clone())
        {
            directories.push_back((root, 0));
        }
    }
    let mut seen_files = BTreeSet::new();
    let mut seen_directories = BTreeSet::new();
    let mut entries = Vec::new();
    let mut examined = 0;
    let mut limited = false;
    let mut unreadable = 0;
    while let Some((directory, depth)) = directories.pop_front() {
        if !seen_directories.insert(directory.clone()) {
            continue;
        }
        if seen_directories.len() > MAX_DIRECTORIES || started.elapsed() >= SCAN_BUDGET {
            limited = true;
            break;
        }
        let Ok(children) = std::fs::read_dir(&directory) else {
            unreadable += 1;
            continue;
        };
        for entry in children {
            examined += 1;
            if examined > MAX_ENTRIES
                || entries.len() >= MAX_FILES
                || started.elapsed() >= SCAN_BUDGET
            {
                limited = true;
                break;
            }
            let Ok(entry) = entry else { continue };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                if !entry.file_name().to_string_lossy().starts_with('.') {
                    if depth < MAX_DEPTH && directories.len() < MAX_DIRECTORIES {
                        directories.push_back((path, depth + 1));
                    } else {
                        limited = true;
                    }
                }
            } else if file_type.is_file() && kind.accepts(&path) {
                let Some(path_text) = path.to_str() else {
                    continue;
                };
                if !seen_files.insert(path_text.to_string()) {
                    continue;
                }
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(path_text);
                let parent = directory
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                entries.push(MediaEntry {
                    path: path_text.to_string(),
                    label: format!("{}   ·   {}", display_text(name), display_text(parent)),
                });
            }
        }
        if examined > MAX_ENTRIES || entries.len() >= MAX_FILES || started.elapsed() >= SCAN_BUDGET
        {
            // Reaching a bound exactly at a directory boundary may omit queued folders.
            limited = true;
            break;
        }
    }
    entries.sort_by_cached_key(|entry| (entry.label.to_lowercase(), entry.path.clone()));
    let count = entries.len();
    let message = if limited {
        format!("{count} files · scan limit reached; open other files with jwm-player")
    } else if unreadable > 0 {
        format!("{count} files · {unreadable} folders could not be read")
    } else if count == 0 {
        match kind {
            MediaKind::Music => "Add files to Music or your recording folder; F5 refreshes".into(),
            MediaKind::Video => "Add files to Videos or your recording folder; F5 refreshes".into(),
        }
    } else {
        format!("{count} files · includes saved recordings")
    };
    MediaCatalog {
        kind,
        entries: entries.into(),
        message,
    }
}

/// Only construct argv here; validation/canonicalization of real files belongs
/// to the helper's worker, not an IPC handler on the compositor thread.
pub fn player_arguments(
    kind: Option<MediaKind>,
    paths: &[String],
    enqueue: bool,
) -> Result<Vec<String>, String> {
    if paths.is_empty() || paths.len() > MAX_QUEUE_FILES {
        return Err(format!(
            "choose between 1 and {MAX_QUEUE_FILES} media files"
        ));
    }
    let mut bytes = 0usize;
    for path in paths {
        bytes = bytes.saturating_add(path.len());
        if path.is_empty() || path.contains('\0') || bytes > MAX_REQUEST_BYTES {
            return Err(
                "media paths must be nonempty, NUL-free and at most 128 KiB in total".into(),
            );
        }
        if !Path::new(path).is_absolute() {
            return Err("open_media expects absolute local file paths".into());
        }
    }
    let mut args = Vec::with_capacity(paths.len() + 3);
    if let Some(kind) = kind {
        args.push(kind.flag().into());
    }
    if enqueue {
        args.push("--enqueue".into());
    }
    args.push("--".into());
    args.extend(paths.iter().cloned());
    Ok(args)
}

pub fn parse_open_request(
    args: &serde_json::Value,
) -> Result<(Option<MediaKind>, Vec<String>, bool), String> {
    let kind = match args.get("mode").and_then(serde_json::Value::as_str) {
        None if args.get("mode").is_none() => None,
        Some("auto") => None,
        Some("music") => Some(MediaKind::Music),
        Some("video") => Some(MediaKind::Video),
        _ => return Err("open_media: mode must be auto, music or video".into()),
    };
    let enqueue = match args.get("enqueue") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or("open_media: enqueue must be a boolean")?,
    };
    let paths = match (args.get("path"), args.get("paths")) {
        (Some(serde_json::Value::String(path)), None) => vec![path.clone()],
        (None, Some(serde_json::Value::Array(paths))) if paths.len() <= MAX_QUEUE_FILES => paths
            .iter()
            .map(|path| {
                path.as_str()
                    .map(str::to_owned)
                    .ok_or("open_media: paths must contain strings")
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err("open_media: provide path or paths (up to 512 files)".into()),
    };
    player_arguments(kind, &paths, enqueue)?;
    Ok((kind, paths, enqueue))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "jwm-media-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn discovery_splits_formats_deduplicates_roots_and_skips_symlink_cycles() {
        let dir = Fixture::new();
        let album = dir.0.join("album");
        std::fs::create_dir(&album).unwrap();
        for file in ["歌 曲.FLAC", "a\nquoted.mp3", "movie.MKV", "readme.txt"] {
            std::fs::write(album.join(file), []).unwrap();
        }
        std::os::unix::fs::symlink(&dir.0, album.join("loop")).unwrap();
        let music = scan(MediaKind::Music, &[dir.0.clone(), album]);
        assert_eq!(music.entries.len(), 2);
        assert!(
            music
                .entries
                .iter()
                .all(|entry| !entry.label.contains('\n'))
        );
        assert!(music.entries.iter().any(|entry| entry.path.contains('\n')));
        assert_eq!(
            scan(MediaKind::Video, std::slice::from_ref(&dir.0))
                .entries
                .len(),
            1
        );
    }

    #[test]
    fn queue_encoding_is_literal_and_rejects_bad_or_unbounded_requests() {
        let paths = vec!["/tmp/-音楽 '$(touch nope)'\n${file}.mp3".into()];
        assert_eq!(
            player_arguments(Some(MediaKind::Music), &paths, true).unwrap(),
            vec!["--music", "--enqueue", "--", paths[0].as_str()]
        );
        assert!(player_arguments(None, &[], false).is_err());
        assert!(player_arguments(None, &["relative.mp3".into()], false).is_err());
        assert!(player_arguments(None, &["/bad\0.mp3".into()], false).is_err());
        assert!(
            player_arguments(None, &vec!["/a.mp3".into(); MAX_QUEUE_FILES + 1], false).is_err()
        );
        assert!(
            player_arguments(
                None,
                &[format!("/{}", "a".repeat(MAX_REQUEST_BYTES))],
                false
            )
            .is_err()
        );
    }

    #[test]
    fn ipc_open_shape_is_strict() {
        use serde_json::json;
        assert!(
            parse_open_request(&json!({"path":"/a.mp3","mode":"music","enqueue":true}))
                .unwrap()
                .2
        );
        for value in [
            json!({}),
            json!({"paths":[]}),
            json!({"path":"/a","paths":["/b"]}),
            json!({"paths":[2]}),
            json!({"path":"/a","mode":null}),
            json!({"path":"/a","enqueue":"yes"}),
            json!({"path":"https://example.org/a.mp4"}),
        ] {
            assert!(parse_open_request(&value).is_err(), "accepted {value}");
        }
    }
}
