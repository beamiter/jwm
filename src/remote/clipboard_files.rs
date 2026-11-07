//! Copied files as one byte stream, and where received files are kept.
//!
//! A file manager's copy names files; the other machine needs their bytes.
//! The sender walks the copied paths once, up front, into a [`Manifest`] —
//! sizes and names only — so the transfer can announce its exact length and
//! be refused before anything is read. [`ManifestReader`] then turns the
//! manifest into a stream of entries:
//!
//! ```text
//! entry := kind (u8: 1 dir, 2 file) || name_len (u16) || mode (u32)
//!          || mtime (i64, Unix seconds) || size (u64) || name || contents
//! ```
//!
//! `name` is a relative, `/`-separated path of raw bytes; a directory always
//! precedes its contents. [`ArchiveReceiver`] recreates the tree inside a
//! fresh private directory, so nothing the peer sends can name a path outside
//! it: every component is validated, nothing is ever followed through a
//! symlink (none are sent or created), and every file and directory is
//! created new.
//!
//! What is deliberately not carried: symlinks inside a copied directory (a
//! copied symlink itself is followed), devices, sockets and FIFOs, owners, and
//! set-id bits. Only the executable bits of a mode survive.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

const KIND_DIR: u8 = 1;
const KIND_FILE: u8 = 2;
const HEADER_FIXED_LEN: usize = 1 + 2 + 4 + 8 + 8;
/// Longest relative name one entry may carry, matching Linux `PATH_MAX`.
const MAX_NAME_LEN: usize = 4096;
const MAX_COMPONENT_LEN: usize = 255;
const MAX_DEPTH: usize = 64;
/// Most entries, directories included, that one copy may carry.
pub const MAX_ENTRIES: u32 = 100_000;
/// Mode bits a received entry may keep: permissions without set-id/sticky.
const MODE_MASK: u32 = 0o755;

/// Why a copy cannot be shared.
#[derive(Debug, PartialEq, Eq)]
pub enum ManifestError {
    /// The copy is larger than the limit (bytes counted so far).
    TooLarge {
        at_least: u64,
        limit: u64,
    },
    TooManyEntries,
    /// Nothing copied could be read.
    Nothing,
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { at_least, limit } => write!(
                f,
                "it is larger than the {} file limit (at least {}); see --clipboard-file-limit",
                human_bytes(*limit),
                human_bytes(*at_least)
            ),
            Self::TooManyEntries => write!(f, "it holds more than {MAX_ENTRIES} files and folders"),
            Self::Nothing => f.write_str("none of the copied files could be read"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EntryKind {
    Dir,
    File { size: u64 },
}

#[derive(Debug, Clone)]
struct Entry {
    name: Vec<u8>,
    source: PathBuf,
    kind: EntryKind,
    mode: u32,
    mtime: i64,
}

/// Everything one copy will send, measured before any byte is read.
#[derive(Debug, Clone)]
pub struct Manifest {
    entries: Vec<Entry>,
    /// Exact length of the entry stream.
    pub stream_bytes: u64,
    /// Sum of file contents, for messages.
    pub file_bytes: u64,
    /// Top-level items — what the person selected.
    pub items: u32,
    /// Entries left out: unreadable files, nested symlinks, devices.
    pub skipped: u32,
}

impl Manifest {
    #[must_use]
    pub fn entry_count(&self) -> u32 {
        u32::try_from(self.entries.len()).unwrap_or(u32::MAX)
    }

    /// Walk the copied paths. `limit` bounds the file contents.
    pub fn build(paths: &[PathBuf], limit: u64) -> Result<Self, ManifestError> {
        let mut manifest = Self {
            entries: Vec::new(),
            stream_bytes: 0,
            file_bytes: 0,
            items: 0,
            skipped: 0,
        };
        let mut used_names = std::collections::HashSet::new();
        for path in paths {
            // A copied symlink is followed: the person picked what it names.
            let Ok(metadata) = fs::metadata(path) else {
                manifest.skipped += 1;
                continue;
            };
            let Some(base) = path.file_name() else {
                manifest.skipped += 1;
                continue;
            };
            let name = unique_name(base.as_bytes(), &mut used_names);
            let before = manifest.entries.len();
            manifest.add(path, name, &metadata, 1, limit)?;
            if manifest.entries.len() > before {
                manifest.items += 1;
            }
        }
        if manifest.entries.is_empty() {
            return Err(ManifestError::Nothing);
        }
        Ok(manifest)
    }

    fn add(
        &mut self,
        source: &Path,
        name: Vec<u8>,
        metadata: &fs::Metadata,
        depth: usize,
        limit: u64,
    ) -> Result<(), ManifestError> {
        if depth > MAX_DEPTH || name.len() > MAX_NAME_LEN {
            self.skipped += 1;
            return Ok(());
        }
        if self.entries.len() >= MAX_ENTRIES as usize {
            return Err(ManifestError::TooManyEntries);
        }
        let mode = metadata.mode() & MODE_MASK;
        let mtime = metadata.mtime();
        if metadata.is_file() {
            // Unreadable files are left out now rather than failing the whole
            // transfer halfway through.
            if File::open(source).is_err() {
                self.skipped += 1;
                return Ok(());
            }
            let size = metadata.len();
            self.file_bytes = self.file_bytes.saturating_add(size);
            if self.file_bytes > limit {
                return Err(ManifestError::TooLarge {
                    at_least: self.file_bytes,
                    limit,
                });
            }
            self.stream_bytes += (HEADER_FIXED_LEN + name.len()) as u64 + size;
            self.entries.push(Entry {
                name,
                source: source.to_path_buf(),
                kind: EntryKind::File { size },
                mode,
                mtime,
            });
            return Ok(());
        }
        if !metadata.is_dir() {
            self.skipped += 1;
            return Ok(());
        }
        let Ok(children) = fs::read_dir(source) else {
            self.skipped += 1;
            return Ok(());
        };
        let mut children: Vec<_> = children.filter_map(Result::ok).collect();
        children.sort_by_key(fs::DirEntry::file_name);
        self.stream_bytes += (HEADER_FIXED_LEN + name.len()) as u64;
        self.entries.push(Entry {
            name: name.clone(),
            source: source.to_path_buf(),
            kind: EntryKind::Dir,
            mode,
            mtime,
        });
        for child in children {
            // Inside a copied folder nothing is followed: a link could point
            // anywhere, including back up the tree.
            let Ok(child_metadata) = fs::symlink_metadata(child.path()) else {
                self.skipped += 1;
                continue;
            };
            if child_metadata.file_type().is_symlink() {
                self.skipped += 1;
                continue;
            }
            let mut child_name = name.clone();
            child_name.push(b'/');
            child_name.extend_from_slice(child.file_name().as_bytes());
            self.add(&child.path(), child_name, &child_metadata, depth + 1, limit)?;
        }
        Ok(())
    }
}

/// Two selected items may share a base name (`a/x` and `b/x`).
fn unique_name(base: &[u8], used: &mut std::collections::HashSet<Vec<u8>>) -> Vec<u8> {
    if used.insert(base.to_vec()) {
        return base.to_vec();
    }
    for index in 2.. {
        let mut candidate = base.to_vec();
        candidate.extend_from_slice(format!(" ({index})").as_bytes());
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("an unbounded counter always finds a free name")
}

/// Produces a manifest's entry stream in bounded chunks.
pub struct ManifestReader {
    entries: std::vec::IntoIter<Entry>,
    header: Vec<u8>,
    header_sent: usize,
    file: Option<(File, u64, PathBuf)>,
}

impl ManifestReader {
    #[must_use]
    pub fn new(manifest: Manifest) -> Self {
        Self {
            entries: manifest.entries.into_iter(),
            header: Vec::new(),
            header_sent: 0,
            file: None,
        }
    }

    /// The next up-to-`max` bytes; empty once the stream is complete.
    ///
    /// A file that shrank or vanished since the walk is an error: the stream
    /// already promised its length.
    pub fn next_chunk(&mut self, max: usize) -> io::Result<Vec<u8>> {
        let mut chunk = Vec::with_capacity(max);
        while chunk.len() < max {
            if self.header_sent < self.header.len() {
                let take = (self.header.len() - self.header_sent).min(max - chunk.len());
                chunk.extend_from_slice(&self.header[self.header_sent..self.header_sent + take]);
                self.header_sent += take;
                continue;
            }
            if let Some((file, remaining, path)) = self.file.as_mut() {
                if *remaining == 0 {
                    self.file = None;
                    continue;
                }
                let want = (*remaining).min((max - chunk.len()) as u64) as usize;
                let start = chunk.len();
                chunk.resize(start + want, 0);
                let read = file.read(&mut chunk[start..])?;
                chunk.truncate(start + read);
                if read == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("{} shrank while it was being shared", path.display()),
                    ));
                }
                *remaining -= read as u64;
                continue;
            }
            let Some(entry) = self.entries.next() else {
                break;
            };
            let size = match entry.kind {
                EntryKind::Dir => 0,
                EntryKind::File { size } => size,
            };
            self.header = encode_header(&entry, size);
            self.header_sent = 0;
            if let EntryKind::File { size } = entry.kind {
                let file = File::open(&entry.source).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("{} could not be read: {error}", entry.source.display()),
                    )
                })?;
                self.file = Some((file, size, entry.source));
            }
        }
        Ok(chunk)
    }
}

fn encode_header(entry: &Entry, size: u64) -> Vec<u8> {
    let mut header = Vec::with_capacity(HEADER_FIXED_LEN + entry.name.len());
    header.push(match entry.kind {
        EntryKind::Dir => KIND_DIR,
        EntryKind::File { .. } => KIND_FILE,
    });
    // `add` keeps names within MAX_NAME_LEN, which fits a u16.
    header.extend_from_slice(&(entry.name.len() as u16).to_be_bytes());
    header.extend_from_slice(&entry.mode.to_be_bytes());
    header.extend_from_slice(&entry.mtime.to_be_bytes());
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&entry.name);
    header
}

/// Recreates a received entry stream under one fresh directory.
pub struct ArchiveReceiver {
    root: PathBuf,
    header: Vec<u8>,
    file: Option<(File, u64, i64)>,
    entries_left: u32,
    top_level: Vec<PathBuf>,
}

impl ArchiveReceiver {
    /// `root` must be a new, empty directory this process created.
    #[must_use]
    pub fn new(root: PathBuf, entries: u32) -> Self {
        Self {
            root,
            header: Vec::with_capacity(HEADER_FIXED_LEN + 256),
            file: None,
            entries_left: entries,
            top_level: Vec::new(),
        }
    }

    pub fn feed(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            if let Some((file, remaining, mtime)) = self.file.as_mut() {
                let take = (*remaining).min(data.len() as u64) as usize;
                file.write_all(&data[..take])?;
                *remaining -= take as u64;
                data = &data[take..];
                if *remaining == 0 {
                    let mtime = *mtime;
                    let (file, _, _) = self.file.take().expect("file checked above");
                    finish_file(&file, mtime);
                }
                continue;
            }
            let wanted = if self.header.len() < HEADER_FIXED_LEN {
                HEADER_FIXED_LEN
            } else {
                HEADER_FIXED_LEN + usize::from(u16::from_be_bytes([self.header[1], self.header[2]]))
            };
            let take = (wanted - self.header.len()).min(data.len());
            self.header.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.header.len() == HEADER_FIXED_LEN {
                let name_len = usize::from(u16::from_be_bytes([self.header[1], self.header[2]]));
                if name_len == 0 || name_len > MAX_NAME_LEN {
                    return Err(invalid("received an entry with an invalid name length"));
                }
            }
            if self.header.len() > HEADER_FIXED_LEN && self.header.len() == wanted {
                self.open_entry()?;
            }
        }
        Ok(())
    }

    fn open_entry(&mut self) -> io::Result<()> {
        let header = std::mem::take(&mut self.header);
        if self.entries_left == 0 {
            return Err(invalid("received more entries than announced"));
        }
        self.entries_left -= 1;
        let kind = header[0];
        let mode = u32::from_be_bytes(header[3..7].try_into().expect("fixed slice"));
        let mtime = i64::from_be_bytes(header[7..15].try_into().expect("fixed slice"));
        let size = u64::from_be_bytes(header[15..23].try_into().expect("fixed slice"));
        let name = &header[HEADER_FIXED_LEN..];
        let relative = validate_name(name)?;
        let path = self.root.join(&relative);
        match kind {
            KIND_DIR if size == 0 => {
                fs::DirBuilder::new()
                    .mode((mode & MODE_MASK) | 0o700)
                    .create(&path)?;
            }
            KIND_FILE => {
                let file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode((mode & MODE_MASK) | 0o600)
                    .open(&path)?;
                if size == 0 {
                    finish_file(&file, mtime);
                } else {
                    self.file = Some((file, size, mtime));
                }
            }
            _ => return Err(invalid("received an entry of an unknown kind")),
        }
        if !name.contains(&b'/') {
            self.top_level.push(path);
        }
        self.header = header;
        self.header.clear();
        Ok(())
    }

    /// The top-level paths, once the stream ended exactly on an entry
    /// boundary with every announced entry present.
    pub fn finish(self) -> io::Result<Vec<PathBuf>> {
        if self.file.is_some() || !self.header.is_empty() {
            return Err(invalid("the transfer ended in the middle of an entry"));
        }
        if self.entries_left != 0 {
            return Err(invalid("the transfer ended before every entry arrived"));
        }
        if self.top_level.is_empty() {
            return Err(invalid("the transfer carried no files"));
        }
        Ok(self.top_level)
    }
}

fn finish_file(file: &File, mtime: i64) {
    let modified = if mtime >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(mtime.unsigned_abs()))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(mtime.unsigned_abs()))
    };
    // Cosmetic: a file manager shows it. Failure leaves the receive time.
    if let Some(modified) = modified {
        let _ = file.set_modified(modified);
    }
}

/// A relative path whose every component is an ordinary name.
fn validate_name(name: &[u8]) -> io::Result<PathBuf> {
    let mut depth = 0;
    for component in name.split(|byte| *byte == b'/') {
        depth += 1;
        if component.is_empty()
            || component == b"."
            || component == b".."
            || component.len() > MAX_COMPONENT_LEN
            || component.contains(&0)
            || depth > MAX_DEPTH
        {
            return Err(invalid("received an entry with an unsafe name"));
        }
    }
    Ok(PathBuf::from(OsStr::from_bytes(name)))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

/// Received copies kept so far this session, newest last.
const KEPT_TRANSFERS: usize = 3;

/// Where received files live until they are pasted.
///
/// `$XDG_CACHE_HOME/jwm/remote-clipboard/s<pid>-<random>/<n>/`: on disk
/// rather than in the RAM-backed runtime directory, since a copied folder can
/// be gigabytes. A file manager pastes by copying out of here, so the newest
/// few copies are kept for the session and the newest survives it; a later
/// session removes directories whose process has exited.
pub struct Staging {
    session: PathBuf,
    next: u32,
    completed: VecDeque<PathBuf>,
    in_progress: Option<PathBuf>,
}

impl Staging {
    pub fn create() -> io::Result<Self> {
        Self::create_in(&staging_root()?)
    }

    /// [`Self::create`] under an explicit root, for tests.
    pub(crate) fn create_in(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root.parent().unwrap_or(root))?;
        match fs::DirBuilder::new().mode(0o700).create(root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(root)?;
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} must be a private directory owned by you",
                    root.display()
                ),
            ));
        }
        sweep_exited_sessions(root);
        let session = root.join(format!(
            "s{}-{:08x}",
            std::process::id(),
            rand::random::<u32>()
        ));
        fs::DirBuilder::new().mode(0o700).create(&session)?;
        Ok(Self {
            session,
            next: 1,
            completed: VecDeque::new(),
            in_progress: None,
        })
    }

    /// A new empty directory for one incoming copy.
    pub fn begin(&mut self) -> io::Result<PathBuf> {
        self.discard_in_progress();
        let directory = self.session.join(self.next.to_string());
        self.next += 1;
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        self.in_progress = Some(directory.clone());
        Ok(directory)
    }

    /// The in-progress copy is complete; forget the oldest beyond the few kept.
    pub fn complete(&mut self) {
        if let Some(directory) = self.in_progress.take() {
            self.completed.push_back(directory);
        }
        while self.completed.len() > KEPT_TRANSFERS {
            if let Some(oldest) = self.completed.pop_front() {
                let _ = fs::remove_dir_all(oldest);
            }
        }
    }

    pub fn discard_in_progress(&mut self) {
        if let Some(directory) = self.in_progress.take() {
            let _ = fs::remove_dir_all(directory);
        }
    }

    /// Terminal backend failure released the clipboard, so no completed copy
    /// remains owned for post-session paste. Also remove admitted-but-unapplied copies.
    pub(crate) fn discard_all(&mut self) {
        self.discard_in_progress();
        for directory in self.completed.drain(..) {
            let _ = fs::remove_dir_all(directory);
        }
    }

    /// Bytes an unprivileged writer may still use on the staging filesystem.
    pub fn available_bytes(&self) -> io::Result<u64> {
        let stats = nix::sys::statvfs::statvfs(&self.session).map_err(io::Error::from)?;
        #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
        Ok(u64::from(stats.blocks_available()).saturating_mul(u64::from(stats.fragment_size())))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        self.discard_in_progress();
        // The newest copy may still be on the clipboard; keep it for a paste
        // after the session, until the next session sweeps it away.
        while self.completed.len() > 1 {
            if let Some(oldest) = self.completed.pop_front() {
                let _ = fs::remove_dir_all(oldest);
            }
        }
        if self.completed.is_empty() {
            let _ = fs::remove_dir(&self.session);
        }
    }
}

fn staging_root() -> io::Result<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "neither XDG_CACHE_HOME nor HOME is set; nowhere to keep received files",
            )
        })?;
    Ok(cache.join("jwm").join("remote-clipboard"))
}

/// Remove session directories whose process no longer exists.
fn sweep_exited_sessions(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix('s'))
            .and_then(|rest| rest.split_once('-'))
            .and_then(|(pid, _)| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() || Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        let path = entry.path();
        // remove_dir_all never follows a symlink; still, only sweep real
        // directories this naming scheme could have produced.
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
            let _ = fs::remove_dir_all(path);
        }
    }
}

/// `1.5 MiB`, `312 KiB`, `12 bytes`.
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Parse `--clipboard-file-limit`: bytes, or a number with K/M/G/T (binary).
pub fn parse_size(value: &str) -> Result<u64, String> {
    let value = value.trim();
    let (digits, multiplier) = match value
        .char_indices()
        .find(|(_, ch)| !ch.is_ascii_digit() && *ch != '.')
    {
        None => (value, 1_u64),
        Some((index, _)) => {
            let unit = value[index..].trim().to_ascii_uppercase();
            let unit = unit.trim_end_matches("IB").trim_end_matches('B');
            let multiplier = match unit {
                "" => 1,
                "K" => 1 << 10,
                "M" => 1 << 20,
                "G" => 1 << 30,
                "T" => 1 << 40,
                _ => return Err(format!("unknown size unit in {value:?}; use K, M, G or T")),
            };
            (&value[..index], multiplier)
        }
    };
    let number: f64 = digits
        .parse()
        .map_err(|_| format!("{value:?} is not a size such as 512M or 2G"))?;
    if !number.is_finite() || number < 0.0 {
        return Err(format!("{value:?} is not a size such as 512M or 2G"));
    }
    let bytes = number * multiplier as f64;
    if bytes > u64::MAX as f64 {
        return Err(format!("{value:?} is too large"));
    }
    Ok(bytes as u64)
}

/// Current time as Unix seconds, for tests and messages.
#[cfg(test)]
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "jwm-clipfiles-{label}-{}-{:08x}",
                std::process::id(),
                rand::random::<u32>()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn stream(manifest: Manifest, chunk: usize) -> Vec<u8> {
        let expected = manifest.stream_bytes;
        let mut reader = ManifestReader::new(manifest);
        let mut bytes = Vec::new();
        loop {
            let next = reader.next_chunk(chunk).unwrap();
            if next.is_empty() {
                break;
            }
            assert!(next.len() <= chunk);
            bytes.extend_from_slice(&next);
        }
        assert_eq!(
            bytes.len() as u64,
            expected,
            "stream length was announced exactly"
        );
        bytes
    }

    #[test]
    fn a_copied_tree_arrives_intact_whatever_the_chunking() {
        let source = TempDir::new("src");
        let tree = source.0.join("project");
        fs::create_dir_all(tree.join("sub/deeper")).unwrap();
        fs::write(tree.join("readme.txt"), b"hello").unwrap();
        fs::write(tree.join("sub/empty"), b"").unwrap();
        let big: Vec<u8> = (0..300_000_u32).map(|n| (n % 251) as u8).collect();
        fs::write(tree.join("sub/deeper/big.bin"), &big).unwrap();
        let script = tree.join("run.sh");
        fs::write(&script, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o4755)).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", tree.join("link")).unwrap();
        let single = source.0.join("note.md");
        fs::write(&single, b"# note").unwrap();

        let paths = vec![tree.clone(), single.clone()];
        let manifest = Manifest::build(&paths, u64::MAX).unwrap();
        assert_eq!(manifest.items, 2);
        assert_eq!(manifest.skipped, 1, "the nested symlink is left out");
        assert_eq!(manifest.file_bytes, 5 + big.len() as u64 + 10 + 6);
        let entries = manifest.entry_count();

        for chunk in [1, 7, 64 * 1024, 1 << 20] {
            let destination = TempDir::new("dst");
            let mut receiver = ArchiveReceiver::new(destination.0.clone(), entries);
            let bytes = stream(Manifest::build(&paths, u64::MAX).unwrap(), chunk);
            for piece in bytes.chunks(chunk.max(3) - 2) {
                receiver.feed(piece).unwrap();
            }
            let top = receiver.finish().unwrap();
            assert_eq!(
                top,
                vec![destination.0.join("project"), destination.0.join("note.md")]
            );
            let root = destination.0.join("project");
            assert_eq!(fs::read(root.join("readme.txt")).unwrap(), b"hello");
            assert_eq!(fs::read(root.join("sub/empty")).unwrap(), b"");
            assert_eq!(fs::read(root.join("sub/deeper/big.bin")).unwrap(), big);
            assert_eq!(fs::read(destination.0.join("note.md")).unwrap(), b"# note");
            let mode = fs::metadata(root.join("run.sh")).unwrap().mode() & 0o7777;
            assert_eq!(mode & 0o111, 0o111, "executable bits survive");
            assert_eq!(mode & 0o7000, 0, "set-id bits never do");
            assert!(fs::symlink_metadata(root.join("link")).is_err());
            let modified = fs::metadata(root.join("readme.txt")).unwrap().mtime();
            assert!((modified - now_unix()).abs() < 3600);
        }
    }

    #[test]
    fn duplicate_top_level_names_are_kept_apart() {
        let source = TempDir::new("dup");
        fs::create_dir_all(source.0.join("a")).unwrap();
        fs::create_dir_all(source.0.join("b")).unwrap();
        fs::write(source.0.join("a/x"), b"first").unwrap();
        fs::write(source.0.join("b/x"), b"second").unwrap();
        let paths = vec![source.0.join("a/x"), source.0.join("b/x")];
        let manifest = Manifest::build(&paths, u64::MAX).unwrap();
        let destination = TempDir::new("dupdst");
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), manifest.entry_count());
        receiver.feed(&stream(manifest, 4096)).unwrap();
        let top = receiver.finish().unwrap();
        assert_eq!(top[1], destination.0.join("x (2)"));
        assert_eq!(fs::read(&top[1]).unwrap(), b"second");
    }

    #[test]
    fn limits_are_checked_before_anything_is_read() {
        let source = TempDir::new("limit");
        fs::write(source.0.join("a"), vec![0; 1000]).unwrap();
        fs::write(source.0.join("b"), vec![0; 1000]).unwrap();
        let paths = vec![source.0.join("a"), source.0.join("b")];
        assert_eq!(
            Manifest::build(&paths, 1500).unwrap_err(),
            ManifestError::TooLarge {
                at_least: 2000,
                limit: 1500
            }
        );
        assert!(Manifest::build(&paths, 2000).is_ok());
        assert_eq!(
            Manifest::build(&[source.0.join("missing")], u64::MAX).unwrap_err(),
            ManifestError::Nothing
        );
    }

    #[test]
    fn a_file_that_shrinks_mid_transfer_fails_rather_than_lying() {
        let source = TempDir::new("shrink");
        let path = source.0.join("log");
        fs::write(&path, vec![1; 10_000]).unwrap();
        let manifest = Manifest::build(std::slice::from_ref(&path), u64::MAX).unwrap();
        fs::write(&path, vec![1; 10]).unwrap();
        let mut reader = ManifestReader::new(manifest);
        let error = loop {
            match reader.next_chunk(1024) {
                Ok(chunk) if chunk.is_empty() => panic!("stream ended short without an error"),
                Ok(_) => {}
                Err(error) => break error,
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    fn entry(kind: u8, name: &[u8], size: u64) -> Vec<u8> {
        let mut bytes = vec![kind];
        bytes.extend_from_slice(&(name.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&0o644_u32.to_be_bytes());
        bytes.extend_from_slice(&0_i64.to_be_bytes());
        bytes.extend_from_slice(&size.to_be_bytes());
        bytes.extend_from_slice(name);
        bytes
    }

    #[test]
    fn hostile_names_never_leave_the_staging_directory() {
        for name in [
            &b"../escape"[..],
            b"/etc/passwd",
            b"a/../../escape",
            b"a//b",
            b".",
            b"a/./b",
            b"nul\0byte",
            b"trailing/",
        ] {
            let destination = TempDir::new("hostile");
            let mut receiver = ArchiveReceiver::new(destination.0.clone(), 4);
            let error = receiver.feed(&entry(KIND_FILE, name, 0)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{name:?}");
        }
        // A file named twice, or under a parent that was never sent.
        let destination = TempDir::new("twice");
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), 4);
        receiver.feed(&entry(KIND_FILE, b"x", 0)).unwrap();
        assert!(receiver.feed(&entry(KIND_FILE, b"x", 0)).is_err());
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), 4);
        assert!(receiver.feed(&entry(KIND_FILE, b"nope/x", 0)).is_err());
    }

    #[test]
    fn a_stream_must_match_what_was_announced() {
        let destination = TempDir::new("count");
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), 1);
        receiver.feed(&entry(KIND_FILE, b"a", 0)).unwrap();
        assert!(receiver.feed(&entry(KIND_FILE, b"b", 0)).is_err());

        let destination = TempDir::new("short");
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), 2);
        receiver.feed(&entry(KIND_FILE, b"a", 0)).unwrap();
        assert!(receiver.finish().is_err(), "an entry is missing");

        let destination = TempDir::new("partial");
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), 1);
        let mut bytes = entry(KIND_FILE, b"a", 4);
        bytes.extend_from_slice(b"ab");
        receiver.feed(&bytes).unwrap();
        assert!(receiver.finish().is_err(), "the file is incomplete");

        let destination = TempDir::new("kind");
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), 1);
        assert!(receiver.feed(&entry(9, b"a", 0)).is_err());
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), 1);
        assert!(receiver.feed(&entry(KIND_DIR, b"d", 5)).is_err());
    }

    #[test]
    fn staging_keeps_a_few_copies_and_sweeps_exited_sessions() {
        let cache = TempDir::new("stage");
        let root = cache.0.join("remote-clipboard");
        // A session left behind by a process that has exited.
        fs::create_dir_all(root.join("s999999999-deadbeef/1")).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let mut staging = Staging::create_in(&root).unwrap();
        assert!(!root.join("s999999999-deadbeef").exists());
        let mut kept = Vec::new();
        for _ in 0..5 {
            let directory = staging.begin().unwrap();
            fs::write(directory.join("f"), b"x").unwrap();
            staging.complete();
            kept.push(directory);
        }
        assert!(!kept[0].exists() && !kept[1].exists());
        assert!(kept[2].exists() && kept[4].exists());
        let abandoned = staging.begin().unwrap();
        staging.discard_in_progress();
        assert!(!abandoned.exists());
        assert!(staging.available_bytes().unwrap() > 0);
        let session = staging.session.clone();
        drop(staging);
        assert!(kept[4].exists(), "the newest copy outlives the session");
        assert!(!kept[3].exists());
        assert!(session.exists());
    }

    #[test]
    fn staging_refuses_a_shared_root() {
        let cache = TempDir::new("shared");
        let root = cache.0.join("remote-clipboard");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            Staging::create_in(&root).err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn sizes_parse_and_print() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("2G").unwrap(), 2 << 30);
        assert_eq!(parse_size("2GiB").unwrap(), 2 << 30);
        assert_eq!(parse_size("1.5g").unwrap(), 3 << 29);
        assert_eq!(parse_size("10 KB").unwrap(), 10 << 10);
        assert!(parse_size("ten").is_err());
        assert!(parse_size("5X").is_err());
        assert!(parse_size("-1").is_err());
        assert_eq!(human_bytes(12), "12 bytes");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(2 << 30), "2.0 GiB");
        assert_eq!(human_bytes(300 << 20), "300 MiB");
    }
}
