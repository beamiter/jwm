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

use super::clipboard_staging_budget::{self, Session};
use crate::backend::clipboard_x11::FileFulfilReceipt;
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
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
/// Most source entries one copy may scan, including skipped entries.
/// This also bounds the number of entries the resulting archive may carry.
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
            Self::TooManyEntries => write!(
                f,
                "scanning it exceeds {MAX_ENTRIES} source entries (including skipped entries)"
            ),
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
        Self::build_with_scan_limit(paths, limit, MAX_ENTRIES)
    }

    fn build_with_scan_limit(
        paths: &[PathBuf],
        limit: u64,
        mut scan_left: u32,
    ) -> Result<Self, ManifestError> {
        let mut manifest = Self {
            entries: Vec::new(),
            stream_bytes: 0,
            file_bytes: 0,
            items: 0,
            skipped: 0,
        };
        let mut used_names = std::collections::HashSet::new();
        for path in paths {
            charge_scan_entry(&mut scan_left)?;
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
            manifest.add(path, name, &metadata, 1, limit, &mut scan_left)?;
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
        scan_left: &mut u32,
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
            if open_source_file(source).is_err() {
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
        // Charge before retaining each directory entry, not after collecting
        // and sorting the entire directory. The shared budget also bounds
        // pending siblings at every recursion depth and skipped/error entries.
        let mut collected = collect_scanned_children(children, scan_left)?;
        collected.sort_by_key(fs::DirEntry::file_name);
        self.stream_bytes += (HEADER_FIXED_LEN + name.len()) as u64;
        self.entries.push(Entry {
            name: name.clone(),
            source: source.to_path_buf(),
            kind: EntryKind::Dir,
            mode,
            mtime,
        });
        for child in collected {
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
            self.add(
                &child.path(),
                child_name,
                &child_metadata,
                depth + 1,
                limit,
                scan_left,
            )?;
        }
        Ok(())
    }
}

fn collect_scanned_children<T>(
    children: impl IntoIterator<Item = io::Result<T>>,
    scan_left: &mut u32,
) -> Result<Vec<T>, ManifestError> {
    let mut collected = Vec::new();
    for child in children {
        charge_scan_entry(scan_left)?;
        if let Ok(child) = child {
            collected.push(child);
        }
    }
    Ok(collected)
}

fn charge_scan_entry(remaining: &mut u32) -> Result<(), ManifestError> {
    *remaining = remaining
        .checked_sub(1)
        .ok_or(ManifestError::TooManyEntries)?;
    Ok(())
}

/// Two selected items may share a base name (`a/x` and `b/x`).
fn unique_name(base: &[u8], used: &mut std::collections::HashSet<Vec<u8>>) -> Vec<u8> {
    if used.insert(base.to_vec()) {
        return base.to_vec();
    }
    for index in 2.. {
        let suffix = format!(" ({index})");
        let mut end = base.len().min(MAX_COMPONENT_LEN - suffix.len());
        // Names are raw Unix bytes. Preserve that contract, but do not turn a
        // valid UTF-8 name into an invalid one when making room for the suffix.
        if let Ok(text) = std::str::from_utf8(base) {
            while !text.is_char_boundary(end) {
                end -= 1;
            }
        }
        let mut candidate = base[..end].to_vec();
        candidate.extend_from_slice(suffix.as_bytes());
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
                let file = open_source_file(&entry.source).map_err(|error| {
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

/// Paths may change between the manifest walk and streaming. A FIFO swapped
/// in at either open must not block the transfer worker waiting for a writer.
/// Follow explicitly copied symlinks as before, but validate the opened object
/// rather than relying on the earlier path metadata. O_NONBLOCK has no effect
/// on ordinary disk files; it does not bound filesystem or device-driver I/O.
fn open_source_file(path: &Path) -> io::Result<File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("clipboard source is no longer a regular file"));
    }
    Ok(file)
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
    directory: File,
    header: Vec<u8>,
    file: Option<(File, u64, i64)>,
    entries_left: u32,
    top_level: Vec<PathBuf>,
}

impl ArchiveReceiver {
    /// `root` must be a new, empty directory this process created.
    pub fn new(root: PathBuf, entries: u32) -> io::Result<Self> {
        let directory = clipboard_staging_budget::open_private_directory(&root, false)?;
        Ok(Self {
            root,
            directory,
            header: Vec::with_capacity(HEADER_FIXED_LEN + 256),
            file: None,
            entries_left: entries,
            top_level: Vec::new(),
        })
    }

    pub fn feed(&mut self, mut data: &[u8]) -> io::Result<()> {
        self.verify_root()?;
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

    fn verify_root(&self) -> io::Result<()> {
        let current = clipboard_staging_budget::open_private_directory(&self.root, false)?;
        let current = current.metadata()?;
        let pinned = self.directory.metadata()?;
        if current.dev() != pinned.dev() || current.ino() != pinned.ino() {
            return Err(invalid("received clipboard directory was replaced"));
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
        // Resolve every directory through a pinned, no-follow descriptor. The
        // separately retained ordinary path is used only for clipboard URIs.
        let mut parent = self.directory.try_clone()?;
        let components: Vec<_> = relative.components().collect();
        for component in &components[..components.len() - 1] {
            let path = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()))
                .join(component.as_os_str());
            parent = clipboard_staging_budget::open_directory(&path)?;
        }
        let path = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(
            components
                .last()
                .expect("validated nonempty name")
                .as_os_str(),
        );
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
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
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
            self.top_level.push(self.root.join(&relative));
        }
        self.header = header;
        self.header.clear();
        Ok(())
    }

    /// The top-level paths, once the stream ended exactly on an entry
    /// boundary with every announced entry present.
    pub fn finish(self) -> io::Result<Vec<PathBuf>> {
        self.verify_root()?;
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
    budget: Session,
    next: u32,
    completed: VecDeque<(PathBuf, FileFulfilReceipt)>,
    in_progress: Option<PathBuf>,
}

impl Staging {
    pub fn create() -> io::Result<Self> {
        Self::create_in(&staging_root()?)
    }

    /// [`Self::create`] under an explicit root, for tests.
    pub(crate) fn create_in(root: &Path) -> io::Result<Self> {
        let budget = Session::create(root)?;
        Ok(Self {
            budget,
            next: 1,
            completed: VecDeque::new(),
            in_progress: None,
        })
    }

    /// A new empty directory for one incoming copy.
    pub fn begin(&mut self, archive_bytes: u64) -> io::Result<PathBuf> {
        self.discard_in_progress();
        let copy = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| invalid("staging copy identifier exhausted"))?;
        let directory = self.budget.reserve(copy, archive_bytes)?;
        self.in_progress = Some(directory.clone());
        Ok(directory)
    }

    /// The in-progress copy is complete; forget the oldest beyond the few kept.
    #[cfg(test)]
    pub fn complete(&mut self) {
        self.complete_with_receipt(FileFulfilReceipt::default());
    }

    pub(crate) fn complete_with_receipt(&mut self, receipt: FileFulfilReceipt) {
        self.reclaim_rejected();
        if let Some(directory) = self.in_progress.take() {
            self.completed.push_back((directory, receipt));
        }
        while self.completed.len() > KEPT_TRANSFERS {
            if let Some((oldest, _)) = self.completed.pop_front() {
                self.delete(&oldest);
            }
        }
    }

    pub fn discard_in_progress(&mut self) {
        if let Some(directory) = self.in_progress.take() {
            self.delete(&directory);
        }
    }

    /// Only the transfer worker performs filesystem cleanup. A receipt is
    /// attached to our own directory at completion, never to a backend path or
    /// reused wire token. Unknown and potentially published copies stay under
    /// the existing retention policy, including on backend failure.
    pub(crate) fn reclaim_rejected(&mut self) {
        let budget = &self.budget;
        self.completed.retain(|(directory, receipt)| {
            if receipt.rejected_unpublished() {
                let _ = delete_reserved(budget, directory);
                false
            } else {
                true
            }
        });
    }

    fn delete(&self, directory: &Path) {
        // Cleanup is best effort under a nonblocking root lock. Failure may
        // leave files, but the durable ledger retains their full reservation.
        let _ = delete_reserved(&self.budget, directory);
    }

    /// Bytes an unprivileged writer may still use on the staging filesystem.
    pub fn available_bytes(&self) -> io::Result<u64> {
        self.budget.available_bytes()
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        self.reclaim_rejected();
        self.discard_in_progress();
        // The newest copy may still be on the clipboard; keep it for a paste
        // after the session, until the next session sweeps it away.
        while self.completed.len() > 1 {
            if let Some((oldest, _)) = self.completed.pop_front() {
                self.delete(&oldest);
            }
        }
        if self.completed.is_empty() {
            let _ = self.budget.remove_if_empty();
        }
    }
}

pub fn staging_root() -> io::Result<PathBuf> {
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

fn delete_reserved(budget: &Session, directory: &Path) -> io::Result<()> {
    let copy = directory
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.parse::<u32>().ok())
        .ok_or_else(|| invalid("invalid reserved staging path"))?;
    budget.delete(copy)
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
    use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "jwm-clipfiles-{label}-{}-{:08x}",
                std::process::id(),
                rand::random::<u32>()
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
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

    fn replace_with_fifo(path: &Path) {
        fs::remove_file(path).unwrap();
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a live, NUL-terminated path with no interior NUL.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    }

    // Release a mistakenly blocking old implementation before asserting, so a
    // regression fails promptly rather than leaving a stuck test worker.
    fn fifo_attempt<T: Send + 'static>(
        path: &Path,
        attempt: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || tx.send(attempt()).unwrap());
        let result = rx.recv_timeout(Duration::from_secs(2));
        let blocked = result.is_err();
        let result = match result {
            Ok(value) => value,
            Err(_) => {
                let mut rescue = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(path)
                    .unwrap();
                // Keep a writer present even if the worker was descheduled
                // before open; satisfy the original eight-byte file read too.
                rescue.write_all(b"original").unwrap();
                rx.recv_timeout(Duration::from_secs(5))
                    .expect("rescued FIFO worker must finish")
            }
        };
        worker.join().unwrap();
        assert!(
            !blocked,
            "opening a replaced FIFO must not wait for a writer"
        );
        result
    }

    #[test]
    fn source_replaced_by_fifo_after_manifest_fails_without_blocking() {
        let source = TempDir::new("replaced-fifo");
        let path = source.0.join("copied");
        fs::write(&path, b"original").unwrap();
        let manifest = Manifest::build(std::slice::from_ref(&path), u64::MAX).unwrap();
        replace_with_fifo(&path);
        let error = fifo_attempt(&path, move || {
            ManifestReader::new(manifest).next_chunk(4096)
        });
        assert_eq!(error.unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(fs::symlink_metadata(path).unwrap().file_type().is_fifo());
    }

    #[test]
    fn stale_manifest_metadata_cannot_block_on_replaced_fifo() {
        let source = TempDir::new("walk-fifo");
        let path = source.0.join("copied");
        fs::write(&path, b"original").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let mut manifest = Manifest::build(std::slice::from_ref(&path), u64::MAX).unwrap();
        let before = manifest.entry_count();
        replace_with_fifo(&path);
        let source_path = path.clone();
        let manifest = fifo_attempt(&path, move || {
            let mut scan_left = MAX_ENTRIES;
            manifest
                .add(
                    &source_path,
                    b"changed".to_vec(),
                    &metadata,
                    1,
                    u64::MAX,
                    &mut scan_left,
                )
                .unwrap();
            manifest
        });
        assert_eq!(manifest.entry_count(), before);
        assert_eq!(manifest.skipped, 1);
    }

    #[test]
    fn source_open_keeps_copied_symlinks_and_rejects_other_types() {
        let source = TempDir::new("source-types");
        let regular = source.0.join("regular");
        fs::write(&regular, b"contents").unwrap();
        let link = source.0.join("link");
        std::os::unix::fs::symlink(&regular, &link).unwrap();
        let mut contents = Vec::new();
        open_source_file(&link)
            .unwrap()
            .read_to_end(&mut contents)
            .unwrap();
        assert_eq!(contents, b"contents");
        for path in [source.0.as_path(), Path::new("/dev/null")] {
            assert_eq!(
                open_source_file(path).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        let manifest = Manifest::build(&[link], u64::MAX).unwrap();
        let destination = TempDir::new("source-link-dst");
        let mut receiver =
            ArchiveReceiver::new(destination.0.clone(), manifest.entry_count()).unwrap();
        receiver.feed(&stream(manifest, 31)).unwrap();
        let top = receiver.finish().unwrap();
        assert_eq!(fs::read(&top[0]).unwrap(), b"contents");
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
        // File creation observes the receiver's umask, including its execute bits.
        let local_executable_bits = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(source.0.join("local-mode-probe"))
            .unwrap()
            .metadata()
            .unwrap()
            .mode()
            & 0o111;
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
            let mut receiver = ArchiveReceiver::new(destination.0.clone(), entries).unwrap();
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
            assert_eq!(
                mode & 0o111,
                local_executable_bits,
                "executable bits permitted by local file creation survive"
            );
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
        let mut receiver =
            ArchiveReceiver::new(destination.0.clone(), manifest.entry_count()).unwrap();
        receiver.feed(&stream(manifest, 4096)).unwrap();
        let top = receiver.finish().unwrap();
        assert_eq!(top[1], destination.0.join("x (2)"));
        assert_eq!(fs::read(&top[1]).unwrap(), b"second");
    }

    #[test]
    fn duplicate_max_length_names_round_trip_without_losing_contents() {
        for name in [
            vec![b'x'; MAX_COMPONENT_LEN],
            "界".repeat(85).into_bytes(),
            vec![0xff; MAX_COMPONENT_LEN],
        ] {
            let source = TempDir::new("long-dup");
            let mut paths = Vec::new();
            for index in 0..12 {
                let parent = source.0.join(index.to_string());
                fs::create_dir(&parent).unwrap();
                let path = parent.join(OsStr::from_bytes(&name));
                fs::write(&path, index.to_string()).unwrap();
                paths.push(path);
            }
            let manifest = Manifest::build(&paths, u64::MAX).unwrap();
            let destination = TempDir::new("long-dup-dst");
            let mut receiver =
                ArchiveReceiver::new(destination.0.clone(), manifest.entry_count()).unwrap();
            for chunk in stream(manifest, 31).chunks(7) {
                receiver.feed(chunk).unwrap();
            }
            let top = receiver.finish().unwrap();
            assert_eq!(top.len(), paths.len());
            for (index, path) in top.iter().enumerate() {
                assert_eq!(fs::read(path).unwrap(), index.to_string().as_bytes());
                let result = path.file_name().unwrap().as_bytes();
                assert!(result.len() <= MAX_COMPONENT_LEN);
                if std::str::from_utf8(&name).is_ok() {
                    assert!(std::str::from_utf8(result).is_ok());
                }
            }
            assert_eq!(top[0].file_name().unwrap().as_bytes(), name);
        }
    }

    #[test]
    fn truncated_duplicate_names_still_avoid_preexisting_suffixes() {
        let base = vec![b'x'; MAX_COMPONENT_LEN];
        let mut used = std::collections::HashSet::new();
        let mut reserved = base[..MAX_COMPONENT_LEN - 4].to_vec();
        reserved.extend_from_slice(b" (2)");
        assert_eq!(unique_name(&reserved, &mut used), reserved);
        assert_eq!(unique_name(&base, &mut used), base);
        let name = unique_name(&base, &mut used);
        assert_eq!(name.len(), MAX_COMPONENT_LEN);
        assert!(name.ends_with(b" (3)"));
        assert_eq!(used.len(), 3);
    }

    #[test]
    fn duplicate_long_directories_keep_their_children_separate() {
        let source = TempDir::new("long-dir");
        let base = "d".repeat(MAX_COMPONENT_LEN);
        let paths: Vec<_> = ["a", "b"]
            .iter()
            .map(|parent| source.0.join(parent).join(&base))
            .collect();
        for (index, path) in paths.iter().enumerate() {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("child"), index.to_string()).unwrap();
        }
        let manifest = Manifest::build(&paths, u64::MAX).unwrap();
        let destination = TempDir::new("long-dir-dst");
        let mut receiver =
            ArchiveReceiver::new(destination.0.clone(), manifest.entry_count()).unwrap();
        receiver.feed(&stream(manifest, 17)).unwrap();
        let top = receiver.finish().unwrap();
        assert_eq!(top.len(), 2);
        for (index, path) in top.iter().enumerate() {
            assert_eq!(
                fs::read(path.join("child")).unwrap(),
                index.to_string().as_bytes()
            );
        }
    }

    #[test]
    fn different_long_bases_with_the_same_shortened_prefix_do_not_collide() {
        let mut first = vec![b'x'; MAX_COMPONENT_LEN];
        let mut second = first.clone();
        first[MAX_COMPONENT_LEN - 1] = b'a';
        second[MAX_COMPONENT_LEN - 1] = b'b';
        let mut used = std::collections::HashSet::new();
        assert_eq!(unique_name(&first, &mut used), first);
        assert_eq!(unique_name(&second, &mut used), second);
        let a = unique_name(&first, &mut used);
        let b = unique_name(&second, &mut used);
        assert!(a.ends_with(b" (2)"));
        assert!(b.ends_with(b" (3)"));
        assert_ne!(a, b);
        assert!(validate_name(&a).is_ok());
        assert!(validate_name(&b).is_ok());
    }

    #[test]
    fn child_scan_stops_before_collecting_an_unbounded_iterator() {
        let mut observed = 0;
        let children = std::iter::from_fn(|| {
            observed += 1;
            assert!(observed <= 4, "read past the first excess entry");
            Some(Ok(observed))
        });
        let mut remaining = 3;
        assert_eq!(
            collect_scanned_children(children, &mut remaining).unwrap_err(),
            ManifestError::TooManyEntries
        );
        assert_eq!(observed, 4);
        assert_eq!(remaining, 0);
    }

    #[test]
    fn directory_read_errors_use_scan_budget_without_being_retained() {
        let children = [Ok(1), Err(io::Error::other("unreadable entry")), Ok(2)];
        let mut remaining = 3;
        assert_eq!(
            collect_scanned_children(children, &mut remaining).unwrap(),
            vec![1, 2]
        );
        assert_eq!(remaining, 0);
        let errors = std::iter::repeat_with(|| Err::<(), _>(io::Error::other("unreadable")));
        assert_eq!(
            collect_scanned_children(errors, &mut 2).unwrap_err(),
            ManifestError::TooManyEntries
        );
    }

    #[test]
    fn source_scan_limit_accepts_the_boundary_and_rejects_excess_children() {
        let source = TempDir::new("scan-boundary");
        for name in ["c", "a", "b"] {
            fs::write(source.0.join(name), name).unwrap();
        }
        let paths = std::slice::from_ref(&source.0);
        let manifest = Manifest::build_with_scan_limit(paths, u64::MAX, 4).unwrap();
        assert_eq!(manifest.entry_count(), 4);
        assert_eq!(manifest.entries[1].name.last(), Some(&b'a'));
        assert_eq!(manifest.entries[3].name.last(), Some(&b'c'));
        assert_eq!(
            Manifest::build_with_scan_limit(paths, u64::MAX, 3).unwrap_err(),
            ManifestError::TooManyEntries
        );
    }

    #[test]
    fn source_scan_budget_is_shared_across_directories_and_roots() {
        let source = TempDir::new("scan-shared");
        for name in ["a", "b"] {
            let directory = source.0.join(name);
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("file"), b"contents").unwrap();
        }
        let paths = std::slice::from_ref(&source.0);
        assert_eq!(
            Manifest::build_with_scan_limit(paths, u64::MAX, 5)
                .unwrap()
                .entry_count(),
            5
        );
        assert_eq!(
            Manifest::build_with_scan_limit(paths, u64::MAX, 4).unwrap_err(),
            ManifestError::TooManyEntries
        );
        let roots = [source.0.join("a"), source.0.join("b")];
        assert!(Manifest::build_with_scan_limit(&roots, u64::MAX, 4).is_ok());
        assert_eq!(
            Manifest::build_with_scan_limit(&roots, u64::MAX, 3).unwrap_err(),
            ManifestError::TooManyEntries
        );
    }

    #[test]
    fn skipped_sources_cannot_bypass_the_scan_budget() {
        let source = TempDir::new("scan-skipped");
        fs::write(source.0.join("file"), b"contents").unwrap();
        for name in ["a", "b", "c"] {
            std::os::unix::fs::symlink("missing", source.0.join(name)).unwrap();
        }
        let paths = std::slice::from_ref(&source.0);
        let manifest = Manifest::build_with_scan_limit(paths, u64::MAX, 5).unwrap();
        assert_eq!(manifest.entry_count(), 2);
        assert_eq!(manifest.skipped, 3);
        assert_eq!(
            Manifest::build_with_scan_limit(paths, u64::MAX, 4).unwrap_err(),
            ManifestError::TooManyEntries
        );
        let missing = vec![source.0.join("missing"); 3];
        assert_eq!(
            Manifest::build_with_scan_limit(&missing, u64::MAX, 2).unwrap_err(),
            ManifestError::TooManyEntries
        );
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
            let mut receiver = ArchiveReceiver::new(destination.0.clone(), 4).unwrap();
            let error = receiver.feed(&entry(KIND_FILE, name, 0)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{name:?}");
        }
        // A file named twice, or under a parent that was never sent.
        let destination = TempDir::new("twice");
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), 4).unwrap();
        receiver.feed(&entry(KIND_FILE, b"x", 0)).unwrap();
        assert!(receiver.feed(&entry(KIND_FILE, b"x", 0)).is_err());
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), 4).unwrap();
        assert!(receiver.feed(&entry(KIND_FILE, b"nope/x", 0)).is_err());
    }

    #[test]
    fn receiver_never_follows_replaced_ancestors_or_root() {
        let destination = TempDir::new("pinned-receiver");
        let outside = TempDir::new("outside-receiver");
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), 2).unwrap();
        receiver.feed(&entry(KIND_DIR, b"nested", 0)).unwrap();
        fs::remove_dir(destination.0.join("nested")).unwrap();
        std::os::unix::fs::symlink(&outside.0, destination.0.join("nested")).unwrap();
        assert!(
            receiver
                .feed(&entry(KIND_FILE, b"nested/escape", 0))
                .is_err()
        );
        assert!(!outside.0.join("escape").exists());

        let root = destination.0.join("root");
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let mut receiver = ArchiveReceiver::new(root.clone(), 1).unwrap();
        fs::rename(&root, destination.0.join("old-root")).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        assert!(receiver.feed(&entry(KIND_FILE, b"file", 0)).is_err());
        assert!(!root.join("file").exists());
        assert!(!destination.0.join("old-root/file").exists());
    }

    #[test]
    fn a_stream_must_match_what_was_announced() {
        let destination = TempDir::new("count");
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), 1).unwrap();
        receiver.feed(&entry(KIND_FILE, b"a", 0)).unwrap();
        assert!(receiver.feed(&entry(KIND_FILE, b"b", 0)).is_err());

        let destination = TempDir::new("short");
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), 2).unwrap();
        receiver.feed(&entry(KIND_FILE, b"a", 0)).unwrap();
        assert!(receiver.finish().is_err(), "an entry is missing");

        let destination = TempDir::new("partial");
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), 1).unwrap();
        let mut bytes = entry(KIND_FILE, b"a", 4);
        bytes.extend_from_slice(b"ab");
        receiver.feed(&bytes).unwrap();
        assert!(receiver.finish().is_err(), "the file is incomplete");

        let destination = TempDir::new("kind");
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), 1).unwrap();
        assert!(receiver.feed(&entry(9, b"a", 0)).is_err());
        let mut receiver = ArchiveReceiver::new(destination.0.clone(), 1).unwrap();
        assert!(receiver.feed(&entry(KIND_DIR, b"d", 5)).is_err());
    }

    #[test]
    fn unpublished_receipt_reclaims_only_its_exact_session_directory() {
        let cache = TempDir::new("receipts");
        let root = cache.0.join("staging");
        let mut first = Staging::create_in(&root).unwrap();
        let mut second = Staging::create_in(&root).unwrap();
        let old_receipt = FileFulfilReceipt::default();
        let old = first.begin(1024).unwrap();
        first.complete_with_receipt(old_receipt.clone());
        let newer_receipt = FileFulfilReceipt::default();
        let newer = first.begin(1024).unwrap();
        first.complete_with_receipt(newer_receipt);
        let other = second.begin(1024).unwrap();
        second.complete();
        old_receipt.reject_unpublished();
        first.reclaim_rejected();
        second.reclaim_rejected();
        assert!(!old.exists());
        assert!(newer.exists() && other.exists());
        // Late/repeated completion cannot address a newer copy, even if the
        // protocol reused the same token, or another session used directory 1.
        old_receipt.reject_unpublished();
        first.reclaim_rejected();
        assert!(newer.exists() && other.exists());
    }

    #[test]
    fn accepted_unknown_and_late_receipts_preserve_newest_copy() {
        let cache = TempDir::new("retain-receipts");
        let root = cache.0.join("staging");
        for accepted in [false, true] {
            let mut staging = Staging::create_in(&root).unwrap();
            let receipt = FileFulfilReceipt::default();
            let copy = staging.begin(1024).unwrap();
            staging.complete_with_receipt(receipt.clone());
            if accepted {
                receipt.accepted_or_may_publish();
            }
            drop(receipt);
            staging.reclaim_rejected();
            drop(staging);
            assert!(
                copy.exists(),
                "dropping an unknown or accepted receipt is not rejection"
            );
        }
        let mut staging = Staging::create_in(&root).unwrap();
        let late = FileFulfilReceipt::default();
        let copy = staging.begin(1024).unwrap();
        staging.complete_with_receipt(late.clone());
        drop(staging);
        late.reject_unpublished();
        assert!(
            copy.exists(),
            "no filesystem work runs on backend receipt resolution"
        );
    }

    #[test]
    fn staging_keeps_a_few_copies_and_sweeps_exited_sessions() {
        let cache = TempDir::new("stage");
        let root = cache.0.join("remote-clipboard");
        // A session left behind by a process that has exited.
        fs::create_dir_all(root.join("s999999999-deadbeef/1")).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(
            root.join("s999999999-deadbeef"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::set_permissions(
            root.join("s999999999-deadbeef/1"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let mut staging = Staging::create_in(&root).unwrap();
        assert!(!root.join("s999999999-deadbeef").exists());
        let mut kept = Vec::new();
        for _ in 0..5 {
            let directory = staging.begin(1024).unwrap();
            fs::write(directory.join("f"), b"x").unwrap();
            staging.complete();
            kept.push(directory);
        }
        assert!(!kept[0].exists() && !kept[1].exists());
        assert!(kept[2].exists() && kept[4].exists());
        let abandoned = staging.begin(1024).unwrap();
        staging.discard_in_progress();
        assert!(!abandoned.exists());
        assert!(staging.available_bytes().unwrap() > 0);
        let session = staging.budget.path();
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
