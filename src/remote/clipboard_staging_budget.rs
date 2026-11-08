//! Local, opt-in admission accounting for received clipboard files.
//!
//! This is a logical reservation limit, not a disk quota. Every cooperating
//! writer, even with no cap, journals the entire announced archive before
//! creating a copy. Charges survive completion and uncertain outcomes; only
//! verified deletion releases them. The ordinary clipboard retention policy is
//! the only eviction policy. Old binaries and hostile same-user writers cannot
//! be constrained by this advisory protocol.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

const LEDGER: &str = ".staging-budget.json";
const MARKER: &str = ".staging-budget-v1";
const MARKER_BYTES: &[u8] = b"jwm clipboard staging accounting v1\n";
const MAX_METADATA_BYTES: usize = 4 * 1024 * 1024;
const MAX_SESSIONS: usize = 1024;
const MAX_RESERVATIONS: usize = 16_384;
const MAX_ROOT_ENTRIES: usize = MAX_SESSIONS + 64;
const MAX_TREE_ENTRIES: usize = 1_000_000;
const MAX_TREE_DEPTH: usize = 64;
const MAX_RELATIVE_BYTES: usize = 4096;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    device: u64,
    inode: u64,
}
impl Identity {
    fn of(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
    fn file(file: &File) -> io::Result<Self> {
        Ok(Self::of(&file.metadata()?))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reservation {
    copy: u32,
    bytes: u64,
    /// None is the durable reservation-before-mkdir crash boundary.
    directory: Option<Identity>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionRecord {
    name: String,
    directory: Identity,
    copies: Vec<Reservation>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u32,
    root: Identity,
    limit_bytes: Option<u64>,
    /// Until first explicit activation, old uncapped writers may coexist.
    legacy_pending: bool,
    sessions: Vec<SessionRecord>,
}

/// A point-in-time local snapshot. Inspecting it never creates or repairs files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub initialized: bool,
    pub limit_bytes: Option<u64>,
    pub reserved_bytes: u64,
    pub reservations: usize,
}
impl Ledger {
    fn status(&self, initialized: bool) -> io::Result<Status> {
        if self.version != 1
            || self.sessions.len() > MAX_SESSIONS
            || self.limit_bytes == Some(0)
            || (self.legacy_pending && self.limit_bytes.is_some())
        {
            return Err(invalid("invalid or unsupported clipboard staging ledger"));
        }
        let mut names = BTreeSet::new();
        let mut reserved_bytes = 0_u64;
        let mut reservations = 0_usize;
        for session in &self.sessions {
            session_pid(&session.name)?;
            if !names.insert(&session.name) {
                return Err(invalid("duplicate staging session"));
            }
            let mut copies = BTreeSet::new();
            for copy in &session.copies {
                if copy.copy == 0 || !copies.insert(copy.copy) {
                    return Err(invalid("invalid or duplicate staging reservation"));
                }
                reservations = reservations
                    .checked_add(1)
                    .ok_or_else(|| invalid("reservation count overflow"))?;
                if reservations > MAX_RESERVATIONS {
                    return Err(invalid("too many clipboard staging reservations"));
                }
                reserved_bytes = reserved_bytes
                    .checked_add(copy.bytes)
                    .ok_or_else(|| invalid("clipboard staging byte count overflow"))?;
            }
        }
        if self.limit_bytes.is_some_and(|limit| reserved_bytes > limit) {
            return Err(invalid("clipboard staging reservations exceed saved limit"));
        }
        Ok(Status {
            initialized,
            limit_bytes: self.limit_bytes,
            reserved_bytes,
            reservations,
        })
    }
}

fn descriptor_path(directory: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}

/// Walk each ancestor through a pinned descriptor, never through a symlink.
/// Only the final directory must be private; existing ancestors are not changed.
pub(crate) fn open_private_directory(path: &Path, create: bool) -> io::Result<File> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    if absolute
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(invalid("staging directory cannot contain '..'"));
    }
    let mut directory = File::open("/")?;
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                let child = descriptor_path(&directory).join(name);
                directory = match open_directory(&child) {
                    Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                        match fs::DirBuilder::new().mode(0o700).create(&child) {
                            Ok(()) => directory.sync_all()?,
                            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                            Err(error) => return Err(error),
                        }
                        open_directory(&child)?
                    }
                    result => result?,
                };
            }
            _ => return Err(invalid("unsafe staging directory")),
        }
    }
    private_directory(&directory)?;
    Ok(directory)
}

pub(crate) fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}
fn private_directory(directory: &File) -> io::Result<()> {
    let metadata = directory.metadata()?;
    // SAFETY: geteuid only reads this process's effective uid.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "clipboard staging directory must be private and owned by you",
        ));
    }
    Ok(())
}
fn same_directory(directory: &File, expected: Identity) -> io::Result<()> {
    if Identity::file(directory)? != expected {
        return Err(invalid("clipboard staging directory was replaced"));
    }
    Ok(())
}
fn owned_regular(file: &File, max: usize) -> io::Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid only reads this process's effective uid.
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > max as u64
    {
        return Err(invalid("unsafe or oversized clipboard staging metadata"));
    }
    Ok(())
}
fn read_metadata(path: &Path, max: usize) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    owned_regular(&file, max)?;
    let mut bytes = Vec::new();
    file.take((max + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(invalid("oversized staging metadata"));
    }
    Ok(bytes)
}
fn session_pid(name: &str) -> io::Result<u32> {
    let (pid, random) = name
        .strip_prefix('s')
        .and_then(|rest| rest.split_once('-'))
        .ok_or_else(|| invalid("unexpected staging session name"))?;
    let number = pid
        .parse::<u32>()
        .map_err(|_| invalid("invalid staging session pid"))?;
    if number == 0
        || number.to_string() != pid
        || random.len() != 8
        || !random.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid("invalid staging session name"));
    }
    Ok(number)
}
fn definitely_exited(pid: u32) -> bool {
    // Permission errors and all other uncertainty mean live. PID reuse retains
    // too much, never too little, matching the pre-budget retention policy.
    fs::metadata(format!("/proc/{pid}")).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}
fn copy_number(name: &std::ffi::OsStr) -> io::Result<u32> {
    let value = name
        .to_str()
        .ok_or_else(|| invalid("invalid copy directory name"))?;
    let number = value
        .parse::<u32>()
        .map_err(|_| invalid("invalid copy directory number"))?;
    if number == 0 || number.to_string() != value {
        return Err(invalid("invalid copy directory number"));
    }
    Ok(number)
}

struct LockedRoot {
    path: PathBuf,
    directory: File,
}
impl LockedRoot {
    fn open(path: &Path, create: bool, expected: Option<Identity>) -> io::Result<Self> {
        let directory = open_private_directory(path, create)?;
        if let Some(identity) = expected {
            same_directory(&directory, identity)?;
        }
        // SAFETY: directory owns a valid live descriptor. flock retains no
        // pointers. Drop explicitly unlocks this transaction before closing.
        if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "clipboard staging root is busy or inaccessible; retry after the other local staging operation finishes: {error}"
                ),
            ));
        }
        let root = Self {
            path: path.to_owned(),
            directory,
        };
        root.verify_path()?;
        Ok(root)
    }
    fn root(&self) -> PathBuf {
        descriptor_path(&self.directory)
    }
    fn verify_path(&self) -> io::Result<()> {
        same_directory(
            &open_private_directory(&self.path, false)?,
            Identity::file(&self.directory)?,
        )
    }
    fn load(&self) -> io::Result<Option<Ledger>> {
        let marker = read_metadata(&self.root().join(MARKER), MARKER_BYTES.len());
        let bytes = read_metadata(&self.root().join(LEDGER), MAX_METADATA_BYTES);
        let ledger = match (marker, bytes) {
            (Err(a), Err(b))
                if a.kind() == io::ErrorKind::NotFound && b.kind() == io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            (Ok(marker), Ok(bytes)) if marker == MARKER_BYTES => {
                serde_json::from_slice::<Ledger>(&bytes).map_err(io::Error::other)?
            }
            _ => {
                return Err(invalid(
                    "clipboard staging metadata missing or unsafe; refusing to reset accounting",
                ));
            }
        };
        if ledger.root != Identity::file(&self.directory)? {
            return Err(invalid("staging ledger belongs to a different root"));
        }
        ledger.status(true)?;
        self.validate_inventory(&ledger)?;
        Ok(Some(ledger))
    }
    fn root_names(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for (count, entry) in fs::read_dir(self.root())?.enumerate() {
            if count >= MAX_ROOT_ENTRIES {
                return Err(invalid("too many staging root entries"));
            }
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| invalid("unexpected staging root name"))?;
            if name == LEDGER || name == MARKER {
                continue;
            }
            if let Some(random) = name.strip_prefix(".budget-write-") {
                if random.len() != 32 || !random.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(invalid("unexpected staging temporary file"));
                }
                // A crash may leave a bounded temporary file. Never interpret
                // it as policy or automatically discard it during status.
                read_metadata(&self.root().join(&name), MAX_METADATA_BYTES)?;
                continue;
            }
            session_pid(&name)?;
            names.push(name);
        }
        Ok(names)
    }
    fn validate_inventory(&self, ledger: &Ledger) -> io::Result<()> {
        for name in self.root_names()? {
            let record = ledger.sessions.iter().find(|s| s.name == name);
            let directory = open_directory(&self.root().join(&name))?;
            private_directory(&directory)?;
            let Some(record) = record else {
                if ledger.legacy_pending {
                    continue;
                }
                return Err(invalid(
                    "untracked staging session; stop old writers before using the budget",
                ));
            };
            same_directory(&directory, record.directory)?;
            for (count, entry) in fs::read_dir(descriptor_path(&directory))?.enumerate() {
                if count >= MAX_RESERVATIONS {
                    return Err(invalid("too many staging copies"));
                }
                let entry = entry?;
                let number = copy_number(&entry.file_name())?;
                let copy = record
                    .copies
                    .iter()
                    .find(|c| c.copy == number)
                    .ok_or_else(|| invalid("untracked clipboard copy"))?;
                let child = open_directory(&entry.path())?;
                private_directory(&child)?;
                if let Some(identity) = copy.directory {
                    same_directory(&child, identity)?;
                }
            }
        }
        Ok(())
    }
    fn import(&self, allow_live: bool) -> io::Result<Ledger> {
        let mut ledger = Ledger {
            version: 1,
            root: Identity::file(&self.directory)?,
            limit_bytes: None,
            legacy_pending: true,
            sessions: Vec::new(),
        };
        self.import_untracked(&mut ledger, allow_live)?;
        Ok(ledger)
    }
    fn import_untracked(&self, ledger: &mut Ledger, allow_live: bool) -> io::Result<()> {
        let mut scanned = 0;
        for name in self.root_names()? {
            if ledger.sessions.iter().any(|session| session.name == name) {
                continue;
            }
            if !ledger.legacy_pending {
                return Err(invalid("untracked session in initialized staging root"));
            }
            if ledger.sessions.len() >= MAX_SESSIONS {
                return Err(invalid("too many staging sessions"));
            }
            if !definitely_exited(session_pid(&name)?) {
                if allow_live {
                    continue;
                }
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "untracked live staging writer; stop old clipboard sessions before enabling the cap or inspecting complete accounting",
                ));
            }
            let directory = open_directory(&self.root().join(&name))?;
            private_directory(&directory)?;
            let mut record = SessionRecord {
                name,
                directory: Identity::file(&directory)?,
                copies: Vec::new(),
            };
            for (count, entry) in fs::read_dir(descriptor_path(&directory))?.enumerate() {
                if count >= MAX_RESERVATIONS {
                    return Err(invalid("too many legacy clipboard copies"));
                }
                let entry = entry?;
                let copy = copy_number(&entry.file_name())?;
                let child = open_directory(&entry.path())?;
                private_directory(&child)?;
                let bytes = inspect_tree(&child, 0, 0, &mut scanned)?;
                record.copies.push(Reservation {
                    copy,
                    bytes,
                    directory: Some(Identity::file(&child)?),
                });
            }
            ledger.sessions.push(record);
            ledger.status(false)?;
        }
        Ok(())
    }
    fn save(&self, ledger: &Ledger) -> io::Result<()> {
        self.verify_path()?;
        ledger.status(true)?;
        let bytes = serde_json::to_vec(ledger).map_err(io::Error::other)?;
        if bytes.len() > MAX_METADATA_BYTES {
            return Err(invalid("staging ledger exceeds 4 MiB"));
        }
        let temporary = self
            .root()
            .join(format!(".budget-write-{:032x}", rand::random::<u128>()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        let result = (|| {
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, self.root().join(LEDGER))?;
            self.directory.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
    fn initialize(&self, ledger: &Ledger) -> io::Result<()> {
        // The marker is durable first: interruption cannot turn an initialized
        // root with a lost ledger into a seemingly fresh unlimited root.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.root().join(MARKER))?;
        file.write_all(MARKER_BYTES)?;
        file.sync_all()?;
        self.directory.sync_all()?;
        self.save(ledger)
    }
    fn load_or_initialize(&self) -> io::Result<Ledger> {
        if let Some(ledger) = self.load()? {
            return Ok(ledger);
        }
        let ledger = self.import(true)?;
        self.initialize(&ledger)?;
        Ok(ledger)
    }
    fn sweep_exited(&self, ledger: &mut Ledger) -> io::Result<()> {
        let names: Vec<_> = ledger
            .sessions
            .iter()
            .filter(|s| session_pid(&s.name).is_ok_and(definitely_exited))
            .map(|s| s.name.clone())
            .collect();
        let mut inspected = 0;
        let mut removed = 0;
        for name in names {
            let index = ledger
                .sessions
                .iter()
                .position(|s| s.name == name)
                .expect("session exists");
            let record = &ledger.sessions[index];
            match open_directory(&self.root().join(&name)) {
                Ok(directory) => {
                    same_directory(&directory, record.directory)?;
                    // Preflight the entire subtree. No symlink, hardlink, or
                    // special entry is followed, and deletion is bounded too.
                    for copy in &record.copies {
                        match open_directory(
                            &descriptor_path(&directory).join(copy.copy.to_string()),
                        ) {
                            Ok(child) => {
                                inspect_tree(&child, 0, 0, &mut inspected)?;
                            }
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error),
                        }
                    }
                    remove_tree(&self.directory, &name, Some(record.directory), &mut removed)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            self.directory.sync_all()?;
            verify_absent(&self.root().join(&name))?;
            ledger.sessions.remove(index);
            self.save(ledger)?;
        }
        Ok(())
    }
}

impl Drop for LockedRoot {
    fn drop(&mut self) {
        // Closing only our descriptor is insufficient: an unrelated concurrent
        // fork can inherit the same open-file description until exec closes its
        // CLOEXEC copy. Explicit unlock ends our transaction immediately even
        // while such a child still holds a duplicate. No work uses this guard
        // after Drop, and each cooperating transaction opens its own descriptor.
        // SAFETY: directory still owns the live descriptor; flock retains no
        // pointers. Unlock failure conservatively leaves the lock until close.
        let _ = unsafe { libc::flock(self.directory.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Inspect without initializing, sweeping, repairing, or updating metadata.
pub fn status(root: &Path) -> io::Result<Status> {
    let store = match LockedRoot::open(root, false, None) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Status {
                initialized: false,
                limit_bytes: None,
                reserved_bytes: 0,
                reservations: 0,
            });
        }
        result => result?,
    };
    match store.load()? {
        Some(mut ledger) => {
            store.import_untracked(&mut ledger, false)?;
            ledger.status(true)
        }
        None => store.import(false)?.status(false),
    }
}

/// Persist an explicit nonzero cap. No existing file is evicted to make it fit.
pub fn set_limit(root: &Path, limit_bytes: u64) -> io::Result<Status> {
    if limit_bytes == 0 {
        return Err(invalid(
            "staging limit must be positive; use disable to remove the cap",
        ));
    }
    let store = LockedRoot::open(root, true, None)?;
    let existing = store.load()?;
    let initialized = existing.is_some();
    let mut ledger = match existing {
        Some(ledger) => ledger,
        None => store.import(false)?,
    };
    store.import_untracked(&mut ledger, false)?;
    let reserved = ledger.status(initialized)?.reserved_bytes;
    if reserved > limit_bytes {
        return Err(invalid(&format!(
            "requested staging limit {limit_bytes} is below {reserved} currently reserved bytes; previous cap unchanged; no files removed"
        )));
    }
    ledger.limit_bytes = Some(limit_bytes);
    ledger.legacy_pending = false;
    if initialized {
        store.save(&ledger)?;
    } else {
        store.initialize(&ledger)?;
    }
    ledger.status(true)
}

/// Disable only the cap, retaining the ledger, reservations, and staged files.
pub fn disable(root: &Path) -> io::Result<Status> {
    let store = match LockedRoot::open(root, false, None) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return status(root),
        result => result?,
    };
    match store.load()? {
        Some(mut ledger) => {
            store.import_untracked(&mut ledger, false)?;
            ledger.limit_bytes = None;
            store.save(&ledger)?;
            ledger.status(true)
        }
        None => store.import(false)?.status(false),
    }
}

/// A cooperating session pins its root and own directory across operations.
pub(crate) struct Session {
    root: PathBuf,
    root_directory: File,
    directory: File,
    name: String,
}
impl Session {
    pub(crate) fn create(root: &Path) -> io::Result<Self> {
        let store = LockedRoot::open(root, true, None)?;
        let mut ledger = store.load_or_initialize()?;
        store.import_untracked(&mut ledger, true)?;
        store.save(&ledger)?;
        store.sweep_exited(&mut ledger)?;
        if ledger.sessions.len() >= MAX_SESSIONS {
            return Err(invalid("too many staging sessions"));
        }
        let name = format!("s{}-{:08x}", std::process::id(), rand::random::<u32>());
        fs::DirBuilder::new()
            .mode(0o700)
            .create(store.root().join(&name))?;
        let directory = open_directory(&store.root().join(&name))?;
        store.directory.sync_all()?;
        ledger.sessions.push(SessionRecord {
            name: name.clone(),
            directory: Identity::file(&directory)?,
            copies: Vec::new(),
        });
        // A crash before this save leaves an untracked session, which fails
        // closed instead of admitting bytes whose provenance is uncertain.
        store.save(&ledger)?;
        let root_directory = open_private_directory(root, false)?;
        same_directory(&root_directory, Identity::file(&store.directory)?)?;
        Ok(Self {
            root: root.to_owned(),
            root_directory,
            directory,
            name,
        })
    }
    pub(crate) fn path(&self) -> PathBuf {
        self.root.join(&self.name)
    }
    fn lock(&self) -> io::Result<(LockedRoot, Ledger)> {
        let store = LockedRoot::open(
            &self.root,
            false,
            Some(Identity::file(&self.root_directory)?),
        )?;
        let ledger = store
            .load()?
            .ok_or_else(|| invalid("initialized staging ledger disappeared"))?;
        let record = ledger
            .sessions
            .iter()
            .find(|s| s.name == self.name)
            .ok_or_else(|| invalid("staging session disappeared from ledger"))?;
        same_directory(&self.directory, record.directory)?;
        let current = open_directory(&store.root().join(&self.name))?;
        same_directory(&current, record.directory)?;
        Ok((store, ledger))
    }
    pub(crate) fn reserve(&self, copy: u32, bytes: u64) -> io::Result<PathBuf> {
        let (store, mut ledger) = self.lock()?;
        let status = ledger.status(true)?;
        let total = status
            .reserved_bytes
            .checked_add(bytes)
            .ok_or_else(|| invalid("staging reservation byte count overflow"))?;
        if status.reservations >= MAX_RESERVATIONS {
            return Err(invalid("too many staging reservations"));
        }
        if ledger.limit_bytes.is_some_and(|limit| total > limit) {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!(
                    "local clipboard staging cap would be exceeded ({bytes} requested, {} reserved); see clipboard-staging status",
                    status.reserved_bytes
                ),
            ));
        }
        let record = ledger
            .sessions
            .iter_mut()
            .find(|s| s.name == self.name)
            .expect("session validated");
        if copy == 0 || record.copies.iter().any(|c| c.copy == copy) {
            return Err(invalid("staging copy identifier already reserved"));
        }
        record.copies.push(Reservation {
            copy,
            bytes,
            directory: None,
        });
        store.save(&ledger)?;
        // Reservation is durable BEFORE mkdir and before the caller can
        // announce a clipboard target. No absent-path reconciliation runs here.
        let path = descriptor_path(&self.directory).join(copy.to_string());
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        self.directory.sync_all()?;
        let directory = open_directory(&path)?;
        ledger
            .sessions
            .iter_mut()
            .find(|s| s.name == self.name)
            .expect("session validated")
            .copies
            .last_mut()
            .expect("reservation inserted")
            .directory = Some(Identity::file(&directory)?);
        store.save(&ledger)?;
        Ok(self.path().join(copy.to_string()))
    }
    pub(crate) fn delete(&self, copy: u32) -> io::Result<()> {
        let (store, mut ledger) = self.lock()?;
        let session = ledger
            .sessions
            .iter()
            .position(|s| s.name == self.name)
            .expect("session validated");
        let index = ledger.sessions[session]
            .copies
            .iter()
            .position(|c| c.copy == copy)
            .ok_or_else(|| invalid("copy has no staging reservation"))?;
        let reservation = &ledger.sessions[session].copies[index];
        let name = copy.to_string();
        match open_directory(&descriptor_path(&self.directory).join(&name)) {
            Ok(directory) => {
                if let Some(identity) = reservation.directory {
                    same_directory(&directory, identity)?;
                }
                inspect_tree(&directory, 0, 0, &mut 0)?;
                remove_tree(&self.directory, &name, reservation.directory, &mut 0)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.directory.sync_all()?;
        verify_absent(&descriptor_path(&self.directory).join(&name))?;
        ledger.sessions[session].copies.remove(index);
        store.save(&ledger)
    }
    pub(crate) fn remove_if_empty(&self) -> io::Result<()> {
        let (store, mut ledger) = self.lock()?;
        let index = ledger
            .sessions
            .iter()
            .position(|s| s.name == self.name)
            .expect("session validated");
        if !ledger.sessions[index].copies.is_empty() {
            return Ok(());
        }
        fs::remove_dir(store.root().join(&self.name))?;
        store.directory.sync_all()?;
        verify_absent(&store.root().join(&self.name))?;
        ledger.sessions.remove(index);
        store.save(&ledger)
    }
    pub(crate) fn available_bytes(&self) -> io::Result<u64> {
        self.lock()?;
        let stats = nix::sys::statvfs::statvfs(&descriptor_path(&self.directory))
            .map_err(io::Error::from)?;
        #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
        u64::from(stats.blocks_available())
            .checked_mul(u64::from(stats.fragment_size()))
            .ok_or_else(|| invalid("filesystem free byte count overflow"))
    }
}

fn verify_absent(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(invalid("staging deletion could not be verified")),
    }
}

/// Bounded descriptor-relative inventory. Count logical archive bytes: every
/// entry's contents plus its 23-byte header and full relative pathname.
fn inspect_tree(
    directory: &File,
    depth: usize,
    prefix: usize,
    scanned: &mut usize,
) -> io::Result<u64> {
    if depth > MAX_TREE_DEPTH {
        return Err(invalid("staging tree exceeds depth bound"));
    }
    let mut bytes = 0_u64;
    for entry in fs::read_dir(descriptor_path(directory))? {
        *scanned = scanned
            .checked_add(1)
            .ok_or_else(|| invalid("staging inventory overflow"))?;
        if *scanned > MAX_TREE_ENTRIES {
            return Err(invalid("staging inventory exceeds entry bound"));
        }
        let entry = entry?;
        let name = entry.file_name();
        let relative = prefix
            .checked_add(name.as_bytes().len())
            .ok_or_else(|| invalid("staging name overflow"))?;
        if relative > MAX_RELATIVE_BYTES {
            return Err(invalid("staging relative name exceeds bound"));
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(entry.path())?;
        let metadata = file.metadata()?;
        // SAFETY: geteuid only reads the effective uid.
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.dev() != directory.metadata()?.dev()
        {
            return Err(invalid("unsafe staging tree owner or mount"));
        }
        let contents = if metadata.is_dir() {
            inspect_tree(&file, depth + 1, relative + 1, scanned)?
        } else if metadata.is_file() && metadata.nlink() == 1 {
            metadata.len()
        } else {
            return Err(invalid("staging tree contains a link or special file"));
        };
        bytes = bytes
            .checked_add(23)
            .and_then(|v| v.checked_add(relative as u64))
            .and_then(|v| v.checked_add(contents))
            .ok_or_else(|| invalid("staging archive byte count overflow"))?;
    }
    Ok(bytes)
}

fn remove_tree(
    parent: &File,
    name: &str,
    expected: Option<Identity>,
    scanned: &mut usize,
) -> io::Result<()> {
    remove_tree_os(parent, std::ffi::OsStr::new(name), expected, 0, scanned)
}
fn remove_tree_os(
    parent: &File,
    name: &std::ffi::OsStr,
    expected: Option<Identity>,
    depth: usize,
    scanned: &mut usize,
) -> io::Result<()> {
    if depth > MAX_TREE_DEPTH + 2 {
        return Err(invalid("staging cleanup exceeds depth bound"));
    }
    let path = descriptor_path(parent).join(name);
    let directory = open_directory(&path)?;
    let identity = Identity::file(&directory)?;
    if let Some(expected) = expected {
        same_directory(&directory, expected)?;
    }
    for entry in fs::read_dir(descriptor_path(&directory))? {
        *scanned = scanned
            .checked_add(1)
            .ok_or_else(|| invalid("staging cleanup count overflow"))?;
        if *scanned > MAX_TREE_ENTRIES {
            return Err(invalid("staging cleanup exceeds entry bound"));
        }
        let entry = entry?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(entry.path())?;
        let metadata = file.metadata()?;
        if metadata.is_dir() {
            remove_tree_os(
                &directory,
                &entry.file_name(),
                Some(Identity::of(&metadata)),
                depth + 1,
                scanned,
            )?;
        } else if metadata.is_file() && metadata.nlink() == 1 {
            fs::remove_file(entry.path())?;
        } else {
            return Err(invalid("unsafe entry during staging cleanup"));
        }
    }
    // Verify the parent name still identifies the pinned directory before
    // unlinking it. We never recurse through a replaced pathname.
    same_directory(&open_directory(&path)?, identity)?;
    directory.sync_all()?;
    fs::remove_dir(path)
}

#[cfg(test)]
mod tests;
