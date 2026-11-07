//! Explicitly saved connection settings, never credentials or automatic reconnects.
//!
//! Storage is versioned and bounded. The caller owns CLI semantic validation.
//! Extra source grants are deliberately not persisted; the CLI rejects saving
//! them instead of silently dropping permissions. Unknown fields fail closed.
//! A pinned, private directory and an advisory lock serialize atomic updates.
use super::clipboard_policy::{ClipboardDirection, ClipboardMode};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

pub const MAX_PROFILES: usize = 128;
pub const MAX_PROFILE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub version: u32,
    pub connection: Connection,
    pub viewer: Viewer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
pub enum Connection {
    Connect {
        address: String,
        key_file: Option<PathBuf>,
    },
    Ssh {
        destination: String,
        monitor: Option<String>,
        fps: Option<u16>,
        jpeg_quality: Option<u8>,
        max_width: Option<u16>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Viewer {
    /// Explicit local startup size only; no observed geometry is persisted.
    pub window_size: Option<super::window_size::WindowSize>,
    pub display: Option<String>,
    // Required booleans: a missing permission is an error, never a new grant.
    pub view_only: bool,
    pub grab_input: bool,
    pub escape_key: String,
    pub clipboard: bool,
    /// Missing scope retains legacy behavior, but never enables consent.
    #[serde(default)]
    pub clipboard_mode: ClipboardMode,
    #[serde(default)]
    pub clipboard_direction: ClipboardDirection,
    pub clipboard_file_limit: u64,
    /// Optional decoded-frame caps; omitted fields retain the hard-cap defaults.
    pub frame_max_width: Option<u16>,
    pub frame_max_height: Option<u16>,
    pub frame_max_pixels: Option<u64>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(invalid(
            "profile name must be 1..64 ASCII letters, digits, '-' or '_'",
        ));
    }
    Ok(())
}

impl Profile {
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_PROFILE_BYTES {
            return Err(invalid("profile exceeds 16 KiB"));
        }
        let profile: Self = serde_json::from_slice(bytes).map_err(io::Error::other)?;
        if profile.version != 1 {
            return Err(invalid("unsupported profile schema version"));
        }
        Ok(profile)
    }
}

pub struct Store {
    directory: File,
}

impl Store {
    /// Open an existing private directory. No implicit mkdir/chmod and no links,
    /// including ancestor links. Walk using pinned descriptors to avoid path races.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_inner(path, false)
    }

    /// Explicitly create missing path components privately; never repair modes.
    pub fn init(path: &Path) -> io::Result<Self> {
        Self::open_inner(path, true)
    }

    fn open_inner(path: &Path, create: bool) -> io::Result<Self> {
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        if absolute
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(invalid(
                "profile directory cannot contain '..' or a platform prefix",
            ));
        }
        let mut directory = File::open("/")?;
        for component in absolute.components() {
            match component {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(name) => {
                    let child = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
                        .join(name);
                    let open = || {
                        OpenOptions::new()
                            .read(true)
                            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                            .open(&child)
                    };
                    directory = match open() {
                        Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                            match fs::DirBuilder::new().mode(0o700).create(&child) {
                                Ok(()) => directory.sync_all()?,
                                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                                Err(error) => return Err(error),
                            }
                            open()?
                        }
                        result => result?,
                    };
                }
                _ => return Err(invalid("profile directory cannot contain '..'")),
            }
        }
        let metadata = directory.metadata()?;
        // SAFETY: geteuid has no preconditions and only reads the process identity.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "profile directory must be owned by you and private (mode 0700); permissions were not changed",
            ));
        }
        // SAFETY: the descriptor is live and owned by directory. flock does not
        // retain pointers. Closing the directory releases this advisory lock.
        if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { directory })
    }

    fn root(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
    }
    fn path(&self, name: &str) -> io::Result<PathBuf> {
        validate_name(name)?;
        Ok(self.root().join(format!("{name}.json")))
    }

    pub fn list(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        // Bound all entries, including unexpected files, not only valid profiles.
        for (index, entry) in fs::read_dir(self.root())?.enumerate() {
            if index >= MAX_PROFILES * 2 {
                return Err(invalid("profile directory exceeds 256 total entries"));
            }
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid("invalid profile filename"))?;
            if let Some(suffix) = name.strip_prefix(".write-") {
                if suffix.len() != 32 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(invalid("unexpected temporary filename"));
                }
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(self.root().join(&name))?;
                let metadata = file.metadata()?;
                if !metadata.is_file()
                    || metadata.nlink() != 1
                    || metadata.mode() & 0o077 != 0
                    || metadata.uid() != self.directory.metadata()?.uid()
                    || metadata.len() > MAX_PROFILE_BYTES as u64
                {
                    return Err(invalid("unsafe temporary profile file"));
                }
                // A writer cannot be active while this directory lock is held.
                // Ignore verified crash leftovers without deleting user data.
                continue;
            }
            if names.len() >= MAX_PROFILES {
                return Err(invalid("profile directory exceeds 128 profiles"));
            }
            let name = name
                .strip_suffix(".json")
                .ok_or_else(|| invalid("unexpected file in profile directory"))?;
            validate_name(name)?;
            self.load(name)?;
            names.push(name.to_owned());
        }
        names.sort();
        Ok(names)
    }

    pub fn load(&self, name: &str) -> io::Result<Profile> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(self.path(name)?)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != self.directory.metadata()?.uid()
            || metadata.mode() & 0o077 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "profile must be a private owned regular file without links",
            ));
        }
        let mut bytes = Vec::new();
        file.take((MAX_PROFILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        Profile::decode(&bytes)
    }

    pub fn save(&self, name: &str, profile: &Profile, replace: bool) -> io::Result<()> {
        let path = self.path(name)?;
        let bytes = serde_json::to_vec_pretty(profile).map_err(io::Error::other)?;
        Profile::decode(&bytes)?;
        let names = self.list()?;
        let exists = names.iter().any(|item| item == name);
        if exists && !replace {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "profile exists; pass --replace explicitly",
            ));
        }
        if !exists && names.len() >= MAX_PROFILES {
            return Err(invalid("at most 128 profiles may be saved"));
        }
        let temporary = self
            .root()
            .join(format!(".write-{:032x}", rand::random::<u128>()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        let result = (|| {
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            self.directory.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    pub fn remove(&self, name: &str) -> io::Result<()> {
        self.load(name)?;
        fs::remove_file(self.path(name)?)?;
        self.directory.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("jwm-profiles-{:032x}", rand::random::<u128>()));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn sample() -> Profile {
        Profile {
            version: 1,
            connection: Connection::Ssh {
                destination: "user@desk".into(),
                monitor: Some("DP-0".into()),
                fps: Some(30),
                jpeg_quality: Some(85),
                max_width: Some(0),
            },
            viewer: Viewer {
                window_size: None,
                display: None,
                view_only: true,
                grab_input: false,
                escape_key: "F12".into(),
                clipboard: false,
                clipboard_mode: ClipboardMode::All,
                clipboard_direction: ClipboardDirection::Both,
                clipboard_file_limit: 0,
                frame_max_width: None,
                frame_max_height: None,
                frame_max_pixels: None,
            },
        }
    }

    #[test]
    fn optional_frame_limits_preserve_legacy_profiles_and_unknown_field_rejection() {
        let mut value = serde_json::to_value(sample()).unwrap();
        for field in ["frame_max_width", "frame_max_height", "frame_max_pixels"] {
            value["viewer"].as_object_mut().unwrap().remove(field);
        }
        assert_eq!(
            Profile::decode(&serde_json::to_vec(&value).unwrap()).unwrap(),
            sample()
        );
        let mut limited = sample();
        limited.viewer.frame_max_width = Some(1024);
        limited.viewer.frame_max_height = Some(768);
        limited.viewer.frame_max_pixels = Some(500000);
        assert_eq!(
            Profile::decode(&serde_json::to_vec(&limited).unwrap()).unwrap(),
            limited
        );
        value["viewer"]["frame_limit_typo"] = 100.into();
        assert!(Profile::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn explicit_init_and_crash_leftovers_preserve_safe_first_use() {
        let temp = Temp::new();
        let path = temp.0.join("new/profiles");
        assert!(Store::open(&path).is_err());
        let store = Store::init(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        let stale = path.join(format!(".write-{:032x}", 1));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&stale)
            .unwrap();
        file.write_all(b"partial").unwrap();
        store.save("desk", &sample(), false).unwrap();
        assert_eq!(store.list().unwrap(), ["desk"]);
        assert!(stale.exists());
        fs::set_permissions(&stale, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.list().is_err());
        drop(store);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Store::init(&path).is_err());
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o755);
    }

    #[test]
    fn roundtrip_replace_remove_and_private_modes() {
        let temp = Temp::new();
        let store = Store::open(&temp.0).unwrap();
        store.save("desk", &sample(), false).unwrap();
        assert_eq!(store.list().unwrap(), ["desk"]);
        assert_eq!(store.load("desk").unwrap(), sample());
        assert_eq!(
            fs::metadata(temp.0.join("desk.json")).unwrap().mode() & 0o777,
            0o600
        );
        assert!(store.save("desk", &sample(), false).is_err());
        let mut updated = sample();
        updated.viewer.clipboard = true;
        store.save("desk", &updated, true).unwrap();
        assert_eq!(store.load("desk").unwrap(), updated);
        assert_eq!(fs::read_dir(&temp.0).unwrap().count(), 1);
        store.remove("desk").unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(store.remove("desk").is_err());
    }

    #[test]
    fn clipboard_scope_defaults_never_create_consent_and_reject_unknown_values() {
        let mut value = serde_json::to_value(sample()).unwrap();
        value["viewer"]
            .as_object_mut()
            .unwrap()
            .remove("clipboard_mode");
        value["viewer"]
            .as_object_mut()
            .unwrap()
            .remove("clipboard_direction");
        let decoded = Profile::decode(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(!decoded.viewer.clipboard);
        assert_eq!(decoded.viewer.clipboard_mode, ClipboardMode::All);
        assert_eq!(decoded.viewer.clipboard_direction, ClipboardDirection::Both);
        value["viewer"]["clipboard_mode"] = "text".into();
        value["viewer"]["clipboard_direction"] = "from-host".into();
        let decoded = Profile::decode(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(!decoded.viewer.clipboard);
        assert_eq!(decoded.viewer.clipboard_mode, ClipboardMode::Text);
        assert_eq!(
            decoded.viewer.clipboard_direction,
            ClipboardDirection::FromHost
        );
        assert_eq!(
            Profile::decode(&serde_json::to_vec(&decoded).unwrap()).unwrap(),
            decoded
        );
        value["viewer"]["clipboard_mode"] = "images".into();
        assert!(Profile::decode(&serde_json::to_vec(&value).unwrap()).is_err());
        value["viewer"]["clipboard_mode"] = "all".into();
        value["viewer"]["clipboard_direction"] = "outgoing".into();
        assert!(Profile::decode(&serde_json::to_vec(&value).unwrap()).is_err());
        value["viewer"]["clipboard_direction"] = "both".into();
        value["viewer"].as_object_mut().unwrap().remove("clipboard");
        assert!(Profile::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn names_schemas_and_bytes_are_bounded() {
        for name in [
            "",
            ".",
            "..",
            "../desk",
            "a/b",
            "a.json",
            "a\n",
            "桌面",
            &"x".repeat(65),
        ] {
            assert!(validate_name(name).is_err());
        }
        assert!(validate_name("desk-01_test").is_ok());
        let mut value = serde_json::to_value(sample()).unwrap();
        value["version"] = 2.into();
        assert!(Profile::decode(&serde_json::to_vec(&value).unwrap()).is_err());
        value["version"] = 1.into();
        value["password"] = "secret".into();
        assert!(Profile::decode(&serde_json::to_vec(&value).unwrap()).is_err());
        value.as_object_mut().unwrap().remove("password");
        value["viewer"].as_object_mut().unwrap().remove("view_only");
        assert!(Profile::decode(&serde_json::to_vec(&value).unwrap()).is_err());
        assert!(Profile::decode(&vec![b' '; MAX_PROFILE_BYTES + 1]).is_err());
    }

    #[test]
    fn unsafe_directories_links_files_and_locks_fail_closed() {
        let temp = Temp::new();
        let target = temp.0.join("target");
        fs::DirBuilder::new().mode(0o700).create(&target).unwrap();
        symlink(&target, temp.0.join("alias")).unwrap();
        assert!(Store::open(&temp.0.join("alias")).is_err());
        fs::DirBuilder::new()
            .mode(0o700)
            .create(target.join("child"))
            .unwrap();
        assert!(Store::open(&temp.0.join("alias/child")).is_err());
        let store = Store::open(&target.join("child")).unwrap();
        assert!(Store::open(&target.join("child")).is_err());
        store.save("desk", &sample(), false).unwrap();
        let path = target.join("child/desk.json");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.load("desk").is_err());
        assert!(store.save("desk", &sample(), true).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&path, target.join("copy")).unwrap();
        assert!(store.load("desk").is_err());
        fs::remove_file(target.join("copy")).unwrap();
        fs::remove_file(&path).unwrap();
        symlink(target.join("missing"), &path).unwrap();
        assert!(store.load("desk").is_err());
        assert!(store.save("desk", &sample(), true).is_err());
        assert!(store.remove("desk").is_err());
        drop(store);
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Store::open(&target).is_err());
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o755);
    }

    #[test]
    fn count_and_disk_read_limits_and_failed_writes_preserve_data() {
        let temp = Temp::new();
        let store = Store::open(&temp.0).unwrap();
        let bytes = serde_json::to_vec(&sample()).unwrap();
        for i in 0..MAX_PROFILES {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(temp.0.join(format!("desk{i}.json")))
                .unwrap();
            file.write_all(&bytes).unwrap();
        }
        assert_eq!(store.list().unwrap().len(), MAX_PROFILES);
        assert!(store.save("overflow", &sample(), false).is_err());
        store.save("desk0", &sample(), true).unwrap();
        let mut huge = sample();
        huge.viewer.escape_key = "x".repeat(MAX_PROFILE_BYTES);
        assert!(store.save("desk0", &huge, true).is_err());
        assert_eq!(store.load("desk0").unwrap(), sample());
        fs::write(temp.0.join("desk0.json"), vec![b' '; MAX_PROFILE_BYTES + 1]).unwrap();
        assert!(store.load("desk0").is_err());
        assert!(store.list().is_err());
    }
}
