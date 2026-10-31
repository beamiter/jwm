//! Local files only. One parser is shared by CLI, OpenUri and D-Bus Open.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const MAX_FILES: usize = 512;
pub const MAX_INPUT_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Auto,
    Music,
    Video,
}

impl Mode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "music" => Ok(Self::Music),
            "video" => Ok(Self::Video),
            _ => Err("mode must be auto, music, or video".into()),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Music => "music",
            Self::Video => "video",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Open {
    pub files: Vec<String>,
    pub enqueue: bool,
    pub mode: Mode,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Cli {
    Open(Open),
    Help,
    Version,
}

pub const HELP: &str = "Usage: jwm-player [--music|--video] [--enqueue] [--] FILE...\n\
Play local music or video in JWM's native media window.\n\
With no FILE, open or raise the player. Further launches reuse the same player.\n\
  --music     Compact initial music window with embedded artwork when available\n\
  --video     Audio and video playback\n\
  --enqueue   Append files instead of replacing the playlist\n\
  --help      Show this help\n\
  --version   Show the helper version\n\
Requires mpv and a D-Bus session. Local file:// URIs are accepted.\n\
Ctrl+O: open files; Space: pause; arrows: seek; 9/0: volume; m: mute;\n\
Ctrl+P: playback modes; r: repeat off/one/all; Ctrl+R: reverse queue;\n\
Ctrl+Shift+S: shuffle; Ctrl+Shift+R: reverse audio/video playback;\n\
f: fullscreen; l: queue; s: subtitles; </>: track; q: quit;\n\
h: hide/restore bars; Ctrl+H: cycle Auto/Always/Hidden (music and video).";

pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Cli, String> {
    let mut open = Open::default();
    let mut files = Vec::new();
    let mut options = true;
    for argument in arguments {
        let argument = argument
            .into_string()
            .map_err(|_| "file paths must be valid UTF-8".to_string())?;
        if options {
            match argument.as_str() {
                "--" => {
                    options = false;
                    continue;
                }
                "--help" | "-h" => return Ok(Cli::Help),
                "--version" => return Ok(Cli::Version),
                "--enqueue" => {
                    open.enqueue = true;
                    continue;
                }
                "--music" | "--video" => {
                    let mode = if argument == "--music" {
                        Mode::Music
                    } else {
                        Mode::Video
                    };
                    if open.mode != Mode::Auto && open.mode != mode {
                        return Err("choose either --music or --video".into());
                    }
                    open.mode = mode;
                    continue;
                }
                flag if flag.starts_with('-') => {
                    return Err(format!(
                        "unknown option {flag:?}; use -- before a filename beginning with -"
                    ));
                }
                _ => {}
            }
        }
        files.push(argument);
    }
    open.files = normalize_files(&files)?;
    Ok(Cli::Open(open))
}

pub fn normalize_files(files: &[String]) -> Result<Vec<String>, String> {
    if files.len() > MAX_FILES {
        return Err(format!("a playlist may contain at most {MAX_FILES} files"));
    }
    if files
        .iter()
        .try_fold(0usize, |n, s| n.checked_add(s.len()))
        .is_none_or(|n| n > MAX_INPUT_BYTES)
    {
        return Err("file paths exceed the 128 KiB request limit".into());
    }
    let normalized: Vec<String> = files
        .iter()
        .map(|file| {
            let path = local_path(file)?;
            let path = std::fs::canonicalize(&path)
                .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
            let metadata = std::fs::metadata(&path)
                .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
            if !metadata.is_file() {
                return Err(format!("{} is not a regular file", path.display()));
            }
            path.into_os_string()
                .into_string()
                .map_err(|_| "file paths must be valid UTF-8".into())
        })
        .collect::<Result<_, _>>()?;
    if normalized
        .iter()
        .try_fold(0usize, |n, path| n.checked_add(path.len()))
        .is_none_or(|n| n > MAX_INPUT_BYTES)
    {
        return Err("resolved file paths exceed the 128 KiB request limit".into());
    }
    Ok(normalized)
}

fn local_path(value: &str) -> Result<PathBuf, String> {
    if value.is_empty() || value.contains('\0') {
        return Err("file path is empty or contains NUL".into());
    }
    if let Some(uri) = value.strip_prefix("file://") {
        let path = if uri.starts_with('/') {
            uri
        } else if let Some(path) = uri.strip_prefix("localhost/") {
            // Keep the leading slash removed by the authority parser.
            return decode_path(&format!("/{path}"));
        } else {
            return Err("only local file:// URIs are supported".into());
        };
        return decode_path(path);
    }
    // A colon is legal in a local filename, but URI-like inputs must never
    // turn into an mpv protocol (including file:, fd:, lavfi: and ytdl:).
    if !Path::new(value).is_absolute()
        && value.split_once(':').is_some_and(|(scheme, _)| {
            !scheme.is_empty()
                && scheme
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"+-.".contains(&c))
        })
    {
        return Err("only local paths and local file:// URIs are supported".into());
    }
    Ok(PathBuf::from(value))
}

fn decode_path(path: &str) -> Result<PathBuf, String> {
    if path.contains(['?', '#']) {
        return Err(
            "file URI query/fragment is not supported; percent-encode literal ? or #".into(),
        );
    }
    let mut bytes = Vec::with_capacity(path.len());
    let mut input = path.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let digit = |b: Option<u8>| b.and_then(|b| (b as char).to_digit(16));
        let high = digit(input.next()).ok_or("invalid file URI percent escape")?;
        let low = digit(input.next()).ok_or("invalid file URI percent escape")?;
        bytes.push((high * 16 + low) as u8);
    }
    let path = String::from_utf8(bytes).map_err(|_| "file URI must contain a UTF-8 path")?;
    if path.contains('\0') || !Path::new(&path).is_absolute() {
        return Err("file URI must identify an absolute local path without NUL".into());
    }
    Ok(PathBuf::from(path))
}

pub fn file_uri(path: &str) -> String {
    let mut uri = String::from("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(byte as char);
        } else {
            use std::fmt::Write;
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip_preserves_unicode_and_reserved_characters() {
        let path = "/tmp/歌曲 \"x\"\n${media-title}?#%.flac";
        assert_eq!(local_path(&file_uri(path)).unwrap(), PathBuf::from(path));
        assert_eq!(
            local_path("file://localhost/tmp/a%20b").unwrap(),
            PathBuf::from("/tmp/a b")
        );
    }

    #[test]
    fn remote_and_malformed_uris_are_rejected() {
        for value in [
            "https://example.com/a",
            "file://server/tmp/a",
            "file:///a%",
            "file:///a%GG",
            "file:///a%00",
            "file:///a#b",
            "fd:0",
            "lavfi:testsrc",
        ] {
            assert!(local_path(value).is_err(), "accepted {value}");
        }
        assert_eq!(local_path("/tmp/a:b").unwrap(), PathBuf::from("/tmp/a:b"));
        assert_eq!(local_path("./a:b").unwrap(), PathBuf::from("./a:b"));
    }

    #[test]
    fn option_conflicts_and_request_bounds_are_enforced() {
        assert!(parse(["--music", "--video"].map(OsString::from)).is_err());
        assert!(parse(["--bogus"].map(OsString::from)).is_err());
        assert_eq!(parse(["--help"].map(OsString::from)).unwrap(), Cli::Help);
        assert!(normalize_files(&vec!["a".into(); MAX_FILES + 1]).is_err());
        assert!(normalize_files(&["a".repeat(MAX_INPUT_BYTES + 1)]).is_err());
    }
}
