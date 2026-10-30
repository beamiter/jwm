//! Explicit, local-only migration of the historical bundled voice profile.
//! Reports contain field names, never configuration values or credentials.
//! Writes require editors and other configuration writers to be closed. The
//! advisory lock/recheck detects observed changes but is not a filesystem CAS.
use crate::config::Config;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{File, Metadata, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
};

const MAX_BYTES: u64 = 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Check,
    Write,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Current,
    Ready,
    Migrated,
    ManualReview,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub status: Status,
    pub changed_fields: Vec<String>,
    pub manual_review_fields: Vec<String>,
    pub backup: Option<PathBuf>,
    pub restart_required: bool,
    pub concurrent_writes_supported: bool,
}
impl Report {
    fn manual(fields: Vec<String>) -> Self {
        Self {
            status: Status::ManualReview,
            changed_fields: vec![],
            manual_review_fields: fields,
            backup: None,
            restart_required: false,
            concurrent_writes_supported: false,
        }
    }
}

struct Field {
    section: &'static str,
    key: &'static str,
    old: toml::Value,
    new: toml::Value,
    required: bool,
}
fn fields() -> Vec<Field> {
    let mut fields = Vec::new();
    macro_rules! field {
        ($s:literal,$k:literal,$o:expr,$n:expr,$r:expr) => {
            fields.push(Field {
                section: $s,
                key: $k,
                old: toml::Value::try_from($o).unwrap(),
                new: toml::Value::try_from($n).unwrap(),
                required: $r,
            });
        };
    }
    let c = Config::default();
    field!(
        "interaction",
        "follow_up_ms",
        5000u64,
        c.interaction.follow_up_ms,
        true
    );
    field!(
        "interaction",
        "max_utterance_ms",
        20000u64,
        c.interaction.max_utterance_ms,
        true
    );
    field!(
        "interaction",
        "max_session_ms",
        300000u64,
        c.interaction.max_session_ms,
        true
    );
    field!(
        "interaction",
        "half_duplex",
        true,
        c.interaction.half_duplex,
        true
    );
    field!(
        "interaction",
        "feedback",
        "brief",
        c.interaction.feedback,
        true
    );
    field!("openai", "model", "gpt-realtime-mini", c.openai.model, true);
    field!(
        "openai",
        "plan_deadline_ms",
        12000u64,
        c.openai.plan_deadline_ms,
        false
    );
    field!(
        "openai",
        "plan_max_output_tokens",
        2048u32,
        c.openai.plan_max_output_tokens,
        false
    );
    field!(
        "openai",
        "feedback_max_output_tokens",
        256u32,
        c.openai.feedback_max_output_tokens,
        false
    );
    field!(
        "turn_detection",
        "type",
        "server_vad",
        c.turn_detection.kind,
        false
    );
    field!(
        "execution",
        "max_actions_per_turn",
        8usize,
        c.execution.max_actions_per_turn,
        true
    );
    field!(
        "execution",
        "max_planning_rounds",
        3u8,
        c.execution.max_planning_rounds,
        true
    );
    field!(
        "pricing",
        "version",
        "openai-2026-10-29",
        c.pricing.version,
        true
    );
    field!(
        "pricing",
        "model",
        "gpt-realtime-mini",
        c.pricing.model,
        true
    );
    field!("pricing", "input_text", 0.60, c.pricing.input_text, true);
    field!("pricing", "input_audio", 10.0, c.pricing.input_audio, true);
    field!("pricing", "cached_text", 0.06, c.pricing.cached_text, true);
    field!(
        "pricing",
        "cached_audio",
        0.30,
        c.pricing.cached_audio,
        true
    );
    field!("pricing", "output_text", 2.40, c.pricing.output_text, true);
    field!(
        "pricing",
        "output_audio",
        20.0,
        c.pricing.output_audio,
        true
    );
    field!(
        "pricing",
        "transcription_per_minute",
        0.0045,
        c.pricing.transcription_per_minute,
        true
    );
    fields
}
fn number(value: &toml::Value) -> Option<f64> {
    value
        .as_float()
        .or_else(|| value.as_integer().map(|v| v as f64))
}
fn same(a: &toml::Value, b: &toml::Value) -> bool {
    match (number(a), number(b)) {
        (Some(a), Some(b)) => a.is_finite() && b.is_finite() && (a - b).abs() < 1e-12,
        _ => a == b,
    }
}
fn get<'a>(value: &'a toml::Value, s: &str, k: &str) -> Option<&'a toml::Value> {
    value.get(s)?.get(k)
}
fn name(field: &Field) -> String {
    format!("{}.{}", field.section, field.key)
}

fn proposal(text: &str) -> Result<(Report, Option<String>), ()> {
    let raw: toml::Value = toml::from_str(text).map_err(|_| ())?;
    let config: Config = match toml::from_str(text) {
        Ok(value) => value,
        Err(_) => {
            return Ok((
                Report::manual(vec!["configuration.unknown_or_invalid_fields".into()]),
                None,
            ));
        }
    };
    if config.validate().is_err() {
        return Ok((
            Report::manual(vec!["configuration.unsupported_values".into()]),
            None,
        ));
    }
    let table = fields();
    let current = toml::Value::try_from(&config).map_err(|_| ())?;
    if table.iter().all(|field| {
        get(&current, field.section, field.key).is_some_and(|value| same(value, &field.new))
    }) {
        return Ok((
            Report {
                status: Status::Current,
                changed_fields: vec![],
                manual_review_fields: vec![],
                backup: None,
                restart_required: false,
                concurrent_writes_supported: false,
            },
            None,
        ));
    }
    let old_model = get(&raw, "openai", "model").and_then(toml::Value::as_str);
    let recognized_model = matches!(
        old_model,
        Some("gpt-realtime-2.1-mini" | "gpt-realtime-mini")
    );
    let mut review = Vec::new();
    for field in &table {
        let value = get(&raw, field.section, field.key);
        let recognized =
            if field.key == "model" && (field.section == "openai" || field.section == "pricing") {
                recognized_model && value.and_then(toml::Value::as_str) == old_model
            } else {
                value.is_some_and(|value| same(value, &field.old))
                    || (!field.required && value.is_none())
            };
        if !recognized {
            review.push(name(field));
        }
    }
    // These unchanging interaction/VAD settings must still match the historical
    // bundle. Devices, application argv, credentials, privacy and budgets are
    // intentionally outside this list and are preserved byte-for-byte.
    let defaults = toml::Value::try_from(Config::default()).map_err(|_| ())?;
    for (section, keys) in [
        (
            "interaction",
            &[
                "trigger",
                "mode",
                "first_speech_timeout_ms",
                "escape_cancel",
                "local_start_detection",
                "start_candidate_grace_ms",
            ][..],
        ),
        (
            "turn_detection",
            &[
                "threshold",
                "prefix_padding_ms",
                "silence_duration_ms",
                "eagerness",
                "create_response",
                "interrupt_response",
            ][..],
        ),
    ] {
        for key in keys {
            if let Some(value) = get(&raw, section, key)
                && !same(value, get(&defaults, section, key).ok_or(())?)
            {
                review.push(format!("{section}.{key}"));
            }
        }
    }
    if !review.is_empty() {
        review.sort();
        review.dedup();
        return Ok((Report::manual(review), None));
    }
    let replacements = table
        .iter()
        .filter(|field| {
            get(&raw, field.section, field.key).is_some_and(|value| !same(value, &field.new))
        })
        .map(|field| {
            (
                (field.section.to_owned(), field.key.to_owned()),
                field.new.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let candidate = match replace_scalars(text, &replacements) {
        Some(text) => text,
        None => {
            return Ok((
                Report::manual(vec!["configuration.formatting".into()]),
                None,
            ));
        }
    };
    let mut expected = raw.clone();
    for ((section, key), value) in &replacements {
        expected[section][key] = value.clone();
    }
    let check: toml::Value = toml::from_str(&candidate).map_err(|_| ())?;
    let validated: Config = toml::from_str(&candidate).map_err(|_| ())?;
    if check != expected || validated.validate().is_err() {
        return Err(());
    }
    Ok((
        Report {
            status: Status::Ready,
            changed_fields: replacements
                .keys()
                .map(|(s, k)| format!("{s}.{k}"))
                .collect(),
            manual_review_fields: vec![],
            backup: None,
            restart_required: false,
            concurrent_writes_supported: false,
        },
        Some(candidate),
    ))
}

/// Edit only ordinary scalar assignments inside explicit tables. Unusual TOML
/// styles remain valid configurations but require manual review, not rewriting.
fn replace_scalars(text: &str, fields: &BTreeMap<(String, String), toml::Value>) -> Option<String> {
    let mut section = String::new();
    let mut offset = 0;
    let mut edits = Vec::new();
    let mut seen = BTreeMap::new();
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let trimmed = body.trim_start();
        if trimmed.starts_with('[') {
            let header = trimmed.split('#').next()?.trim();
            section = header
                .strip_prefix('[')?
                .strip_suffix(']')?
                .trim()
                .to_owned();
        } else if let Some((key, _)) = trimmed.split_once('=') {
            let key = key.trim();
            let field = (section.clone(), key.to_owned());
            if let Some(value) = fields.get(&field) {
                let equals = body.find('=')?;
                let value_start =
                    equals + 1 + body[equals + 1..].len() - body[equals + 1..].trim_start().len();
                let rest = &body[value_start..];
                let bytes = rest.as_bytes();
                let length = if matches!(bytes.first(), Some(b'\'' | b'"')) {
                    let quote = bytes[0];
                    if bytes.get(1) == Some(&quote) && bytes.get(2) == Some(&quote) {
                        return None;
                    }
                    let mut end = None;
                    let mut escaped = false;
                    for (index, byte) in bytes.iter().enumerate().skip(1) {
                        if escaped {
                            escaped = false;
                            continue;
                        }
                        if *byte == b'\\' && quote == b'"' {
                            escaped = true;
                            continue;
                        }
                        if *byte == quote {
                            end = Some(index + 1);
                            break;
                        }
                    }
                    end?
                } else {
                    bytes
                        .iter()
                        .position(|byte| byte.is_ascii_whitespace() || *byte == b'#')
                        .unwrap_or(bytes.len())
                };
                let tail = rest[length..].trim_start();
                if !tail.is_empty() && !tail.starts_with('#') {
                    return None;
                }
                if seen.insert(field, ()).is_some() {
                    return None;
                }
                edits.push((
                    offset + value_start,
                    offset + value_start + length,
                    value.to_string(),
                ));
            }
        }
        offset += line.len();
    }
    if seen.len() != fields.len() {
        return None;
    }
    let mut result = text.to_owned();
    for (start, end, value) in edits.into_iter().rev() {
        result.replace_range(start..end, &value);
    }
    Some(result)
}

struct Source {
    file: File,
    parent: File,
    path: PathBuf,
    name: CString,
    meta: Metadata,
    bytes: Vec<u8>,
}
fn owned_regular(meta: &Metadata) -> bool {
    meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o7022 == 0
}
fn stable(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.uid() == b.uid()
        && a.mode() == b.mode()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
impl Source {
    fn open(path: &Path, mode: Mode) -> io::Result<Self> {
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        let parent_path = path
            .parent()
            .ok_or_else(|| io::Error::other("migration_parent_missing"))?;
        let parent = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(parent_path)?;
        let directory = parent.metadata()?;
        if directory.uid() != unsafe { libc::geteuid() } || directory.mode() & 0o022 != 0 {
            return Err(io::Error::other(
                "migration_requires_private_user_directory",
            ));
        }
        let name = CString::new(
            path.file_name()
                .ok_or_else(|| io::Error::other("migration_filename_missing"))?
                .as_bytes(),
        )
        .map_err(|_| io::Error::other("invalid_migration_path"))?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata()?;
        if !owned_regular(&meta) {
            return Err(io::Error::other(
                "migration_requires_user_owned_regular_config",
            ));
        }
        if meta.len() > MAX_BYTES {
            return Err(io::Error::other("migration_configuration_too_large"));
        }
        if unsafe {
            libc::flock(
                file.as_raw_fd(),
                if mode == Mode::Write {
                    libc::LOCK_EX | libc::LOCK_NB
                } else {
                    libc::LOCK_SH | libc::LOCK_NB
                },
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut bytes = Vec::new();
        (&mut file).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES || !stable(&meta, &file.metadata()?) {
            return Err(io::Error::other("migration_source_changed"));
        }
        Ok(Self {
            file,
            parent,
            path,
            name,
            meta,
            bytes,
        })
    }
    fn recheck(&mut self) -> io::Result<()> {
        let current = std::fs::symlink_metadata(&self.path)?;
        if !owned_regular(&current)
            || !stable(&self.meta, &current)
            || !stable(&self.meta, &self.file.metadata()?)
        {
            return Err(io::Error::other("migration_source_changed"));
        }
        self.file.seek(SeekFrom::Start(0))?;
        let mut now = Vec::new();
        (&mut self.file).take(MAX_BYTES + 1).read_to_end(&mut now)?;
        if now != self.bytes {
            return Err(io::Error::other("migration_source_changed"));
        }
        Ok(())
    }
    fn create(&self, suffix: &str) -> io::Result<(CString, File)> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut name = self.name.as_bytes().to_vec();
        name.extend_from_slice(
            format!(
                ".{suffix}-{}-{}-{seq}",
                crate::store::wall_ms(),
                std::process::id()
            )
            .as_bytes(),
        );
        let name = CString::new(name).map_err(|_| io::Error::other("invalid_migration_path"))?;
        let fd = unsafe {
            libc::openat(
                self.parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((name, unsafe { File::from_raw_fd(fd) }))
    }
}
pub fn migrate(path: &Path, mode: Mode) -> io::Result<Report> {
    migrate_inner(path, mode, || Ok(()))
}
fn migrate_inner(
    path: &Path,
    mode: Mode,
    before_commit: impl FnOnce() -> io::Result<()>,
) -> io::Result<Report> {
    let mut source = Source::open(path, mode)?;
    let text = match std::str::from_utf8(&source.bytes) {
        Ok(text) => text,
        Err(_) => return Ok(Report::manual(vec!["configuration.encoding".into()])),
    };
    let (mut report, candidate) = proposal(text)
        .unwrap_or_else(|_| (Report::manual(vec!["configuration.format".into()]), None));
    if mode == Mode::Check || report.status != Status::Ready {
        return Ok(report);
    }
    let candidate = candidate.ok_or_else(|| io::Error::other("migration_candidate_missing"))?;
    source.recheck()?;
    let (backup_name, mut backup) = source.create("backup")?;
    backup.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    backup.write_all(&source.bytes)?;
    backup.sync_all()?;
    source.parent.sync_all()?;
    report.backup = Some(
        source
            .path
            .parent()
            .unwrap()
            .join(std::ffi::OsStr::from_bytes(backup_name.as_bytes())),
    );
    let (temp_name, mut temp) = source.create("migration")?;
    let transaction = (|| {
        temp.write_all(candidate.as_bytes())?;
        temp.set_permissions(std::fs::Permissions::from_mode(source.meta.mode() & 0o777))?;
        temp.sync_all()?;
        before_commit()?;
        source.recheck()?;
        if unsafe {
            libc::renameat(
                source.parent.as_raw_fd(),
                temp_name.as_ptr(),
                source.parent.as_raw_fd(),
                source.name.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        source.parent.sync_all()?;
        Ok(())
    })();
    if transaction.is_err() {
        unsafe { libc::unlinkat(source.parent.as_raw_fd(), temp_name.as_ptr(), 0) };
    }
    transaction?;
    report.status = Status::Migrated;
    report.restart_required = true;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    struct Fixture {
        directory: PathBuf,
        path: PathBuf,
    }
    impl Fixture {
        fn new(text: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let directory = std::env::temp_dir().join(format!(
                "eva-migration-{}-{}-{}",
                std::process::id(),
                crate::store::wall_ms(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
            let path = directory.join("eva.toml");
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
            Self { directory, path }
        }
        fn entries(&self) -> usize {
            std::fs::read_dir(&self.directory).unwrap().count()
        }
        fn backups(&self) -> Vec<PathBuf> {
            std::fs::read_dir(&self.directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .contains(".backup-")
                })
                .collect()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
    fn legacy() -> String {
        let updates = fields()
            .into_iter()
            .map(|field| ((field.section.into(), field.key.into()), field.old))
            .collect();
        let old = replace_scalars(include_str!("../dist/eva.toml"), &updates).unwrap();
        old.replace(
            "model = \"gpt-realtime-mini\"",
            "model = \"gpt-realtime-2.1-mini\"",
        )
        // The wake model remains the modern mini alias; only active-model
        // prices identify the historical profile.
        .replace(
            "[wake_word]\nenabled = false\nphrase = \"hi eva\"\nmodel = \"gpt-realtime-2.1-mini\"",
            "[wake_word]\nenabled = false\nphrase = \"hi eva\"\nmodel = \"gpt-realtime-mini\"",
        )
        .replace(
            "[wake_pricing]\nversion = \"openai-2026-10-29\"\nmodel = \"gpt-realtime-2.1-mini\"",
            "[wake_pricing]\nversion = \"openai-2026-10-29\"\nmodel = \"gpt-realtime-mini\"",
        )
    }
    #[test]
    fn check_is_read_only_and_old_model_aliases_are_recognized() {
        for text in [
            legacy(),
            legacy().replace("gpt-realtime-2.1-mini", "gpt-realtime-mini"),
        ] {
            let fixture = Fixture::new(&text);
            let before = std::fs::metadata(&fixture.path).unwrap();
            let report = migrate(&fixture.path, Mode::Check).unwrap();
            assert_eq!(report.status, Status::Ready);
            assert!(report.backup.is_none());
            assert!(!report.restart_required);
            assert_eq!(std::fs::read_to_string(&fixture.path).unwrap(), text);
            assert!(stable(&before, &std::fs::metadata(&fixture.path).unwrap()));
            assert_eq!(fixture.entries(), 1);
        }
    }
    #[test]
    fn write_backs_up_exact_bytes_preserves_comments_permissions_and_user_settings() {
        let text = legacy()
            .replace(
                "api_key_source = \"secret_service\"",
                "api_key_source = \"file\"\napi_key_file = \"/not-opened/credential\"",
            )
            .replace("daily_limit_usd = 1.00", "daily_limit_usd = 7.25")
            .replace(
                "include_clipboard_in_context = false",
                "include_clipboard_in_context = true",
            )
            .replace(
                "# browser = [\"firefox\"]",
                "browser = [\"firefox\", \"--private-window\"]",
            )
            .replace(
                "model = \"gpt-realtime-2.1-mini\"",
                "model = 'gpt-realtime-2.1-mini' # keep this comment",
            );
        let fixture = Fixture::new(&text);
        let report = migrate(&fixture.path, Mode::Write).unwrap();
        assert_eq!(report.status, Status::Migrated);
        assert!(report.restart_required);
        let backup = report.backup.unwrap();
        assert_eq!(std::fs::read(&backup).unwrap(), text.as_bytes());
        assert_eq!(std::fs::metadata(backup).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(&fixture.path).unwrap().mode() & 0o777,
            0o640
        );
        let updated = std::fs::read_to_string(&fixture.path).unwrap();
        assert!(updated.contains("# keep this comment"));
        let config: Config = toml::from_str(&updated).unwrap();
        config.validate().unwrap();
        assert_eq!(config.openai.model, "gpt-realtime");
        assert_eq!(config.pricing.input_audio, 32.0);
        assert_eq!(config.openai.api_key_source, "file");
        assert_eq!(
            config.openai.api_key_file,
            Some(PathBuf::from("/not-opened/credential"))
        );
        assert_eq!(
            config.applications["browser"],
            vec!["firefox", "--private-window"]
        );
        assert_eq!(config.budget.daily_limit_usd, 7.25);
        assert!(config.privacy.include_clipboard_in_context);
        assert!(!config.wake_word.enabled);
        assert!(!config.interaction.half_duplex);
        assert_eq!(config.interaction.feedback, "voice");
        assert_eq!(config.execution.max_actions_per_turn, 32);
        assert_eq!(config.turn_detection.kind, "semantic_vad");
        assert!(
            !serde_json::to_string(&migrate(&fixture.path, Mode::Check).unwrap())
                .unwrap()
                .contains("credential")
        );
    }
    #[test]
    fn current_profile_is_noop_even_with_write_and_different_user_budget() {
        let text = include_str!("../dist/eva.toml")
            .replace("daily_limit_usd = 1.00", "daily_limit_usd = 4.5");
        let fixture = Fixture::new(&text);
        let meta = std::fs::metadata(&fixture.path).unwrap();
        assert_eq!(
            migrate(&fixture.path, Mode::Write).unwrap().status,
            Status::Current
        );
        assert_eq!(fixture.entries(), 1);
        assert!(stable(&meta, &std::fs::metadata(&fixture.path).unwrap()));
        assert_eq!(std::fs::read_to_string(&fixture.path).unwrap(), text);
    }
    #[test]
    fn reports_explicitly_exclude_uncooperative_concurrent_writers() {
        for text in [
            legacy(),
            include_str!("../dist/eva.toml").to_owned(),
            legacy().replace("follow_up_ms = 5000", "follow_up_ms = 6000"),
        ] {
            let fixture = Fixture::new(&text);
            let report = migrate(&fixture.path, Mode::Check).unwrap();
            assert!(!report.concurrent_writes_supported);
            assert_eq!(
                serde_json::to_value(report).unwrap()["concurrent_writes_supported"],
                false
            );
        }
        let fixture = Fixture::new(&legacy());
        assert!(
            !migrate(&fixture.path, Mode::Write)
                .unwrap()
                .concurrent_writes_supported
        );
    }
    #[test]
    fn original_future_wake_section_is_preserved_without_enabling_listening() {
        let text = legacy();
        let start = text.find("[wake_word]").unwrap();
        let end = text.find("[applications]").unwrap();
        let original = format!(
            "{}[future_wake_word]\nenabled = false\nphrase = \"hi eva\"\n\n{}",
            &text[..start],
            &text[end..]
        );
        let fixture = Fixture::new(&original);
        assert_eq!(
            migrate(&fixture.path, Mode::Write).unwrap().status,
            Status::Migrated
        );
        let updated = std::fs::read_to_string(&fixture.path).unwrap();
        assert!(updated.contains("[future_wake_word]\nenabled = false"));
        let config: Config = toml::from_str(&updated).unwrap();
        assert!(!config.wake_word.enabled);
        assert_eq!(config.wake_word.model, "gpt-realtime-mini");
        config.validate().unwrap();
    }
    #[test]
    fn custom_models_prices_and_interaction_are_never_overwritten() {
        for (text, field) in [
            (
                legacy().replace("gpt-realtime-2.1-mini", "custom-private-model"),
                "openai.model",
            ),
            (
                legacy().replace("input_text = 0.6", "input_text = 0.7"),
                "pricing.input_text",
            ),
            (
                legacy().replace("follow_up_ms = 5000", "follow_up_ms = 6000"),
                "interaction.follow_up_ms",
            ),
            (
                legacy().replace(
                    "first_speech_timeout_ms = 8000",
                    "first_speech_timeout_ms = 9000",
                ),
                "interaction.first_speech_timeout_ms",
            ),
        ] {
            let fixture = Fixture::new(&text);
            let report = migrate(&fixture.path, Mode::Write).unwrap();
            assert_eq!(report.status, Status::ManualReview, "{field}");
            assert!(
                report.manual_review_fields.contains(&field.to_owned()),
                "{:?}",
                report.manual_review_fields
            );
            assert_eq!(fixture.entries(), 1);
            assert_eq!(std::fs::read_to_string(&fixture.path).unwrap(), text);
        }
    }
    #[test]
    fn malformed_unknown_and_embedded_secret_values_never_appear_in_reports() {
        for text in [
            "secret = 'do-not-print-this'\n[bad",
            &legacy().replace(
                "voice = \"marin\"",
                "voice = \"marin\"\napi_key = \"do-not-print-this\"",
            ),
        ] {
            let fixture = Fixture::new(text);
            let report = migrate(&fixture.path, Mode::Write).unwrap();
            assert_eq!(report.status, Status::ManualReview);
            assert!(
                !serde_json::to_string(&report)
                    .unwrap()
                    .contains("do-not-print-this")
            );
            assert_eq!(std::fs::read_to_string(&fixture.path).unwrap(), text);
            assert_eq!(fixture.entries(), 1);
        }
    }
    #[test]
    fn symlinks_shared_files_and_special_files_fail_closed() {
        let fixture = Fixture::new(&legacy());
        let link = fixture.directory.join("link.toml");
        symlink(&fixture.path, &link).unwrap();
        assert!(migrate(&link, Mode::Write).is_err());
        let link_directory = fixture.directory.join("linked");
        symlink(&fixture.directory, &link_directory).unwrap();
        assert!(migrate(&link_directory.join("eva.toml"), Mode::Check).is_err());
        std::fs::set_permissions(&fixture.path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(migrate(&fixture.path, Mode::Check).is_err());
        let fifo = fixture.directory.join("fifo");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(migrate(&fifo, Mode::Check).is_err());
    }
    #[test]
    fn inode_replacement_after_backup_preserves_new_file_and_retains_backup() {
        let text = legacy();
        let fixture = Fixture::new(&text);
        let replacement = fixture.directory.join("external");
        std::fs::write(&replacement, "external config edit").unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = migrate_inner(&fixture.path, Mode::Write, || {
            std::fs::rename(&replacement, &fixture.path)
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "migration_source_changed");
        assert_eq!(
            std::fs::read_to_string(&fixture.path).unwrap(),
            "external config edit"
        );
        let backups = fixture.backups();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(&backups[0]).unwrap(), text.as_bytes());
        assert_eq!(fixture.entries(), 2);
    }
    #[test]
    fn in_place_changes_and_precommit_failures_never_replace_original_contents() {
        for edit in [true, false] {
            let text = legacy();
            let fixture = Fixture::new(&text);
            assert!(
                migrate_inner(&fixture.path, Mode::Write, || if edit {
                    std::fs::write(&fixture.path, "external in-place edit")
                } else {
                    Err(io::Error::other("injected_before_commit"))
                })
                .is_err()
            );
            assert_eq!(
                std::fs::read_to_string(&fixture.path).unwrap(),
                if edit {
                    "external in-place edit"
                } else {
                    &text
                }
            );
            assert_eq!(fixture.backups().len(), 1);
            assert_eq!(fixture.entries(), 2);
            assert_eq!(
                std::fs::read(&fixture.backups()[0]).unwrap(),
                text.as_bytes()
            );
        }
    }
    #[test]
    fn crlf_and_missing_final_newline_are_preserved() {
        let text = legacy()
            .replace('\n', "\r\n")
            .trim_end_matches(['\r', '\n'])
            .to_owned();
        let fixture = Fixture::new(&text);
        assert_eq!(
            migrate(&fixture.path, Mode::Write).unwrap().status,
            Status::Migrated
        );
        let updated = std::fs::read_to_string(&fixture.path).unwrap();
        assert!(!updated.ends_with('\n'));
        assert_eq!(
            updated.matches("\r\n").count(),
            text.matches("\r\n").count()
        );
        assert_eq!(
            updated
                .lines()
                .filter(|line| line.trim_start().starts_with('#'))
                .collect::<Vec<_>>(),
            text.lines()
                .filter(|line| line.trim_start().starts_with('#'))
                .collect::<Vec<_>>()
        );
    }
    #[test]
    fn unsupported_inline_style_requires_manual_review_instead_of_rewriting() {
        let text = legacy();
        let start = text.find("[openai]").unwrap();
        let end = text[start..].find("[turn_detection]").unwrap() + start;
        let raw: toml::Value = toml::from_str(&text).unwrap();
        let inline = raw["openai"]
            .as_table()
            .unwrap()
            .iter()
            .map(|(key, value)| format!("{key} = {value}"))
            .collect::<Vec<_>>()
            .join(", ");
        let unusual = format!(
            "openai = {{ {inline} }}\n{}{}",
            &text[..start],
            &text[end..]
        );
        let fixture = Fixture::new(&unusual);
        let report = migrate(&fixture.path, Mode::Write).unwrap();
        assert_eq!(report.status, Status::ManualReview);
        assert_eq!(fixture.entries(), 1);
        assert_eq!(std::fs::read_to_string(&fixture.path).unwrap(), unusual);
    }
}
