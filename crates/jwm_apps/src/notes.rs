use crate::{editor::Editor, note_title};
use gio::{self, glib, prelude::*};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
};
use std::time::Instant;

const MAX_TEXT_BYTES: u64 = 8 * 1024 * 1024;
const SIZE_ERROR: &str =
    "编码后的文件超过 8 MiB（含 BOM 和换行）。未写入文件，编辑内容仍保留，请缩短内容后重试。";

fn read_limited(reader: impl Read) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_TEXT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_TEXT_BYTES {
        return Err("文件实际内容超过 8 MiB，未打开或修改原文件。".into());
    }
    Ok(bytes)
}

// GIO's local etag can collide when two files have the same timestamp. Keep
// inode/change metadata too, so atomic replacements are still detected.
#[derive(PartialEq, Eq)]
struct FileStamp(u64, u64, u64, i64, i64, i64, i64);
impl FileStamp {
    fn from(metadata: &std::fs::Metadata) -> Self {
        Self(
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    }
    fn read(file: &gio::File) -> Result<Self, String> {
        file.path()
            .ok_or("请选择本地文件。")?
            .metadata()
            .map(|m| Self::from(&m))
            .map_err(|e| e.to_string())
    }
}
#[derive(Debug)]
pub(crate) enum SaveError {
    Conflict,
    Other(String),
}

impl SaveError {
    fn message(self) -> String {
        match self {
            Self::Conflict => "文件已被其他程序修改，请另存为以保留当前内容。".into(),
            Self::Other(message) => message,
        }
    }
}

/// Etags and local file identity protect against conflicting external edits.
pub struct TextFile {
    pub file: gio::File,
    etag: Option<glib::GString>,
    bom: bool,
    crlf: bool,
    stamp: FileStamp,
}

impl TextFile {
    pub fn load(file: gio::File) -> Result<(Self, String), String> {
        Self::load_with_reader(file, |stream| read_limited(stream.into_read()))
    }

    fn load_with_reader(
        file: gio::File,
        read: impl FnOnce(gio::FileInputStream) -> Result<Vec<u8>, String>,
    ) -> Result<(Self, String), String> {
        let path = file.path().ok_or("请选择本地文件。")?;
        let metadata = path.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() || metadata.len() > MAX_TEXT_BYTES {
            return Err("请选择不超过 8 MiB 的 UTF-8 文本文件。".into());
        }
        let stamp = FileStamp::from(&metadata);
        let stream = file
            .read(gio::Cancellable::NONE)
            .map_err(|error| error.to_string())?;
        let etag = stream
            .query_info("etag::value", gio::Cancellable::NONE)
            .map_err(|error| error.to_string())?
            .etag();
        // A stale metadata size must never permit an unbounded allocation.
        let bytes = read(stream)?;
        if FileStamp::read(&file)? != stamp {
            return Err("读取期间文件已被其他程序修改，请重新打开。".into());
        }
        let text =
            std::str::from_utf8(&bytes).map_err(|_| "文件不是 UTF-8 文本，未修改原文件。")?;
        let bom = text.starts_with('\u{feff}');
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let crlf = uses_only_crlf(text);
        // Only normalize uniform CRLF documents. Mixed files keep their exact
        // separators in the editor; a global replacement would silently change
        // unrelated lines on the next save (including Save As).
        let text = if crlf {
            text.replace("\r\n", "\n")
        } else {
            text.to_owned()
        };
        Ok((
            Self {
                file,
                etag,
                bom,
                crlf,
                stamp,
            },
            text,
        ))
    }

    fn save(&mut self, text: &str) -> Result<(), SaveError> {
        if FileStamp::read(&self.file).map_err(SaveError::Other)? != self.stamp {
            return Err(SaveError::Conflict);
        }
        let text = encode_text(text, self.bom, self.crlf).map_err(SaveError::Other)?;
        self.etag = self
            .file
            .replace_contents(
                text.as_bytes(),
                self.etag.as_deref(),
                true,
                gio::FileCreateFlags::PRIVATE,
                gio::Cancellable::NONE,
            )
            .map_err(|error| {
                if error.matches(gio::IOErrorEnum::WrongEtag) {
                    SaveError::Conflict
                } else {
                    SaveError::Other(format!("保存失败：{error}"))
                }
            })?;
        self.stamp = FileStamp::read(&self.file).map_err(SaveError::Other)?;
        Ok(())
    }
}

fn uses_only_crlf(mut text: &str) -> bool {
    let mut seen = false;
    while let Some(index) = text.find(['\r', '\n']) {
        let Some(rest) = text[index..].strip_prefix("\r\n") else {
            return false;
        };
        seen = true;
        text = rest;
    }
    seen
}

fn encode_text(text: &str, bom: bool, crlf: bool) -> Result<String, String> {
    // Check final bytes before allocating another copy or touching the target.
    let extra_newlines = if crlf {
        text.bytes().filter(|byte| *byte == b'\n').count()
    } else {
        0
    };
    let size = text
        .len()
        .checked_add(extra_newlines)
        .and_then(|size| size.checked_add(if bom { 3 } else { 0 }));
    if size.is_none_or(|size| size as u64 > MAX_TEXT_BYTES) {
        return Err(SIZE_ERROR.into());
    }
    let text = if crlf {
        text.replace('\n', "\r\n")
    } else {
        text.to_owned()
    };
    Ok(if bom { format!("\u{feff}{text}") } else { text })
}

fn recovery_name(path: &Path, index: usize) -> Result<std::ffi::OsString, String> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let suffix = if index == 1 {
        " (恢复副本)".into()
    } else {
        format!(" (恢复副本 {index})")
    };
    let extension = path.extension().map(OsStrExt::as_bytes);
    let extra = extension.map_or(0, |ext| 1 + ext.len());
    let limit = 255usize
        .checked_sub(suffix.len() + extra)
        .filter(|limit| *limit >= 4)
        .ok_or("原文件扩展名过长，请改用另存为。")?;
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("Notes");
    let mut end = stem.len().min(limit);
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    let mut name = stem.as_bytes()[..end].to_vec();
    name.extend_from_slice(suffix.as_bytes());
    if let Some(extension) = extension {
        name.push(b'.');
        name.extend_from_slice(extension);
    }
    Ok(std::ffi::OsString::from_vec(name))
}

fn publish_recovery(
    source: &Path,
    encoded: &[u8],
    reserved: impl Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    use std::{
        fs::{self, DirBuilder, OpenOptions},
        io::{ErrorKind, Write},
        os::unix::fs::{DirBuilderExt, OpenOptionsExt},
        time::{SystemTime, UNIX_EPOCH},
    };

    let parent = source.parent().ok_or("无法找到原文件的目录。")?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    struct Staging(PathBuf);
    impl Drop for Staging {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0.join("draft"));
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    let mut staging = None;
    for i in 0..16 {
        let path = parent.join(format!(".jwm-notes-{}-{stamp}-{i}", std::process::id()));
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {
                staging = Some(Staging(path));
                break;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    let staging = staging.ok_or("无法创建临时恢复文件，请改用另存为。")?;
    let draft = staging.0.join("draft");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&draft)
        .map_err(|error| error.to_string())?;
    file.write_all(encoded)
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())?;
    drop(file);

    for index in 1..=64 {
        let path = parent.join(recovery_name(source, index)?);
        if reserved(&path) {
            continue;
        }
        // Publish only complete bytes. Never replace a file or follow a
        // symlink occupying a candidate name, even if it appeared mid-save.
        match fs::hard_link(&draft, &path) {
            Ok(()) => {
                // Remove the staging name before the caller records ctime.
                drop(staging);
                return Ok(path);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("恢复副本名称已用完，请改用另存为。".into())
}

pub fn create_note(directory: &Path) -> Result<PathBuf, String> {
    use std::{
        fs::{DirBuilder, OpenOptions},
        os::unix::fs::{DirBuilderExt, OpenOptionsExt},
        time::{SystemTime, UNIX_EPOCH},
    };
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .map_err(|error| error.to_string())?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    for sequence in 0..100 {
        let path = directory.join(format!("{stamp}-{}-{sequence}.txt", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("无法创建新备忘录。".into())
}

type NotesRows = Vec<(PathBuf, String)>;
pub(crate) type SearchWake = Arc<dyn Fn() + Send + Sync>;

struct ActiveSearch {
    result: Receiver<NotesRows>,
    cancelled: Arc<AtomicBool>,
}

fn scan_rows(directory: &Path, query: &str, cancelled: impl Fn() -> bool) -> NotesRows {
    let mut entries: Vec<_> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .take_while(|_| !cancelled())
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|s| s == "txt"))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() || meta.len() > MAX_TEXT_BYTES {
                return None;
            }
            let bytes = read_limited(std::fs::File::open(e.path()).ok()?).ok()?;
            let text = std::str::from_utf8(&bytes).ok()?;
            (query.is_empty() || text.to_lowercase().contains(query))
                .then(|| (e.path(), note_title(text), meta.modified().ok()))
        })
        .collect();
    entries.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    entries.into_iter().map(|(p, t, _)| (p, t)).collect()
}

pub(crate) struct Notes {
    pub document: Option<TextFile>,
    pub editor: Editor,
    pub preview: crate::preview::Preview,
    pub search: Editor,
    pub find: crate::find::Find,
    pub directory: PathBuf,
    pub rows: NotesRows,
    query_text: String,
    query_generation: u64,
    // A list refresh can preserve a settled filter without blanking its visible list.
    painted_refresh: bool,
    search_wake: Option<SearchWake>,
    search_active: Option<ActiveSearch>,
    pending_search: Option<String>,
    pub managed: bool,
    pub dirty: bool,
    pub due: Option<Instant>,
    pub status: String,
    pub list_scroll: f64,
    #[cfg(test)]
    refreshes: usize,
}

impl Notes {
    pub fn new(path: Option<PathBuf>) -> Result<Self, String> {
        let mut this = Self {
            document: None,
            editor: Editor::default(),
            preview: crate::preview::Preview::default(),
            search: Editor::default(),
            find: crate::find::Find::default(),
            directory: glib::user_data_dir().join("jwm/notes"),
            rows: vec![],
            query_text: String::new(),
            query_generation: 0,
            painted_refresh: false,
            search_wake: None,
            search_active: None,
            pending_search: None,
            managed: path.is_none(),
            dirty: false,
            due: None,
            status: "自动保存已开启 · Ctrl+N 新建 · Ctrl+F 搜索".into(),
            list_scroll: 0.,
            #[cfg(test)]
            refreshes: 0,
        };
        if let Some(path) = path {
            this.load(path)?;
        } else {
            this.refresh();
            let path = this
                .rows
                .first()
                .map(|row| row.0.clone())
                .map(Ok)
                .unwrap_or_else(|| create_note(&this.directory))?;
            this.load(path)?;
        }
        Ok(this)
    }
    pub fn load(&mut self, path: PathBuf) -> Result<(), String> {
        let (document, text) = TextFile::load(gio::File::for_path(path))?;
        self.document = Some(document);
        self.editor = Editor::new(text);
        self.preview = crate::preview::Preview::default();
        self.find = crate::find::Find::default();
        self.dirty = false;
        self.due = None;
        self.status = if self.managed {
            "自动保存已开启 · Ctrl+N 新建"
        } else {
            "UTF-8 · Ctrl+S 保存 · Ctrl+Shift+S 另存为"
        }
        .into();
        Ok(())
    }
    /// Explicitly re-read a clean external document. Do not change any
    /// document state until the bounded, consistent read succeeds.
    pub fn reload_external(&mut self) -> Result<bool, String> {
        if self.managed {
            return Ok(false);
        }
        if self.dirty {
            return Err(
                "当前有未保存的修改，未重新载入。请先保存，或另存副本后重新打开原文件。".into(),
            );
        }
        if !self.editor.preedit.is_empty() || !self.find.query.preedit.is_empty() {
            return Ok(false);
        }
        let file = self
            .document
            .as_ref()
            .ok_or("没有可重新载入的文件。")?
            .file
            .clone();
        let (document, text) = TextFile::load(file)?;
        let changed = self.editor.text != text;
        if changed {
            let (line, column) = self.editor.position();
            let monospace = self.editor.monospace;
            self.editor = Editor::new(text);
            self.editor.monospace = monospace;
            self.editor.move_to_position(line, column);
            let preview_enabled = self.preview.enabled;
            self.preview = crate::preview::Preview::default();
            self.preview.enabled = preview_enabled;
            self.find = crate::find::Find::default();
        }
        // Even a byte-for-byte replacement may have a new inode/etag.
        // Also adopt any new BOM/newline format when decoded text matches.
        self.document = Some(document);
        self.status = "已从磁盘重新载入".into();
        Ok(changed)
    }
    pub fn title(&self) -> String {
        let title = if self.managed {
            note_title(&self.editor.text)
        } else {
            self.document
                .as_ref()
                .and_then(|d| d.file.basename())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "文本文件".into())
        };
        format!("{}{title}", if self.dirty { "• " } else { "" })
    }
    pub fn editor_status(&mut self) -> String {
        if self.managed {
            return self.status.clone();
        }
        let (line, column) = self.editor.position();
        let raw_endings = self.editor.line_endings();
        let (bom, crlf) = self
            .document
            .as_ref()
            .map(|d| (d.bom, d.crlf))
            .unwrap_or_default();
        let endings = match (crlf, raw_endings) {
            (true, "LF") => "CRLF",
            // The existing CRLF save mode prefixes every LF with CR.
            (true, "CRLF") => "混合换行",
            _ => raw_endings,
        };
        format!(
            "行 {line} · 字素列 {column} · UTF-8{} · {endings} · {}",
            if bom { " BOM" } else { "" },
            self.status
        )
    }
    pub fn edited(&mut self) {
        if self.find.enabled {
            self.find.document_edited(&mut self.editor);
        }
        self.dirty = true;
        self.status = if self.managed {
            "等待自动保存…"
        } else {
            "有未保存的修改 · Ctrl+S 保存"
        }
        .into();
        self.due = self
            .managed
            .then(|| Instant::now() + std::time::Duration::from_millis(700));
    }
    fn persist(&mut self) -> Result<bool, SaveError> {
        self.due = None;
        if !self.dirty {
            return Ok(false);
        }
        self.document
            .as_mut()
            .ok_or_else(|| SaveError::Other("没有可保存的文件。".into()))?
            .save(&self.editor.text)?;
        self.dirty = false;
        self.status = "已保存".into();
        Ok(true)
    }
    pub fn save(&mut self) -> Result<(), String> {
        self.save_explicitly().map_err(SaveError::message)
    }
    pub(crate) fn save_explicitly(&mut self) -> Result<(), SaveError> {
        if self.persist()? && self.managed {
            self.refresh();
        }
        Ok(())
    }
    /// Complete the write before acknowledging it, but rebuild the notebook
    /// results off the input loop. The existing worker cancels older scans.
    pub fn autosave(&mut self) -> Result<(), String> {
        if self.persist().map_err(SaveError::message)? && self.managed {
            self.refresh_notebook();
        }
        Ok(())
    }
    /// Re-scan results for the existing filter without touching the open note.
    pub fn refresh_notebook(&mut self) {
        if !self.managed {
            return;
        }
        let keep_painted =
            !self.rows.is_empty() && (self.current_rows().is_some() || self.painted_refresh());
        self.update_query();
        self.painted_refresh = keep_painted && self.search_pending();
    }
    pub fn open_result(&mut self, path: PathBuf) -> Result<(), String> {
        // Save before loading another note, but scan the result list only
        // once. If the save succeeded and the load failed, still refresh to
        // reflect the saved file's new title, contents and modification time.
        let saved = self.persist().map_err(SaveError::message)?;
        let opened = self.load(path);
        if opened.is_ok() || (saved && self.managed) {
            self.refresh();
        }
        opened
    }
    pub(crate) fn keep_conflict_copy(
        &mut self,
        reserved: impl Fn(&Path) -> bool,
    ) -> Result<PathBuf, String> {
        if !self.dirty {
            return Err("没有需要保留的未保存修改。".into());
        }
        let source = self.document.as_ref().ok_or("没有可保存的文件。")?;
        let original = source.file.path().ok_or("请选择本地文件。")?;
        let (bom, crlf) = (source.bom, source.crlf);
        let bytes = encode_text(&self.editor.text, bom, crlf)?;
        let path = publish_recovery(&original, bytes.as_bytes(), reserved)?;
        // Verify the bytes and stamp the new inode after removing its staging
        // hard link. Keep the editor's encoding rules and in-memory content.
        let (mut document, _) = TextFile::load_with_reader(gio::File::for_path(&path), |stream| {
            let read = read_limited(stream.into_read())?;
            if read != bytes.as_bytes() {
                return Err("副本已发生变化，当前编辑内容仍保留。".into());
            }
            Ok(read)
        })
        .map_err(|error| format!("副本已写入 {}，但无法切换：{error}", path.display()))?;
        document.bom = bom;
        document.crlf = crlf;
        self.document = Some(document);
        self.dirty = false;
        self.due = None;
        if self.managed {
            self.refresh_notebook();
        }
        self.status = format!("已保留副本：{}", path.display());
        Ok(path)
    }
    pub fn save_as(&mut self, path: &Path) -> Result<(), String> {
        let file = gio::File::for_path(path);
        if self.document.as_ref().is_some_and(|d| d.file == file) {
            return self.save();
        }
        let (bom, crlf) = self
            .document
            .as_ref()
            .map(|d| (d.bom, d.crlf))
            .unwrap_or_default();
        let encoded = encode_text(&self.editor.text, bom, crlf)?;
        let etag = file
            .replace_contents(
                encoded.as_bytes(),
                None,
                false,
                gio::FileCreateFlags::PRIVATE,
                gio::Cancellable::NONE,
            )
            .map_err(|e| e.to_string())?;
        if !self.managed {
            self.document = Some(TextFile {
                stamp: FileStamp::read(&file)?,
                file,
                etag,
                bom,
                crlf,
            });
            self.dirty = false;
        }
        self.status = "已导出 / 保存文本文件".into();
        Ok(())
    }
    pub fn refresh(&mut self) {
        #[cfg(test)]
        {
            self.refreshes += 1;
        }
        self.advance_query();
        self.pending_search = None;
        self.cancel_scan();
        let rows = scan_rows(&self.directory, &self.search.text.to_lowercase(), || false);
        self.publish_rows(rows);
    }

    pub fn painted_refresh(&self) -> bool {
        self.painted_refresh && self.query_text == self.search.text && self.search_pending()
    }

    fn advance_query(&mut self) {
        if self.query_text != self.search.text {
            self.query_text.clone_from(&self.search.text);
            self.query_generation = self
                .query_generation
                .checked_add(1)
                .expect("search generations exhausted");
        }
    }

    fn cancel_scan(&self) {
        if let Some(active) = &self.search_active {
            active.cancelled.store(true, Ordering::Relaxed);
        }
    }

    pub fn set_search_wake(&mut self, wake: SearchWake) {
        self.search_wake = self.managed.then_some(wake);
    }

    pub fn current_rows(&self) -> Option<u64> {
        (!self.search_pending() && self.query_text == self.search.text)
            .then_some(self.query_generation)
    }

    pub fn search_pending(&self) -> bool {
        self.pending_search.is_some()
            || self
                .search_active
                .as_ref()
                .is_some_and(|active| !active.cancelled.load(Ordering::Relaxed))
    }

    pub fn update_query(&mut self) {
        self.painted_refresh = false;
        if self.search_wake.is_none() {
            self.refresh();
            return;
        }
        self.advance_query();
        self.pending_search = Some(self.search.text.to_lowercase());
        self.cancel_scan();
        self.start_scan();
    }

    fn start_scan(&mut self) {
        if self.search_active.is_some() {
            return;
        }
        let Some(query) = self.pending_search.take() else {
            return;
        };
        let wake = self
            .search_wake
            .as_ref()
            .expect("search event missing")
            .clone();
        let flag = Arc::new(AtomicBool::new(false));
        let worker_flag = flag.clone();
        let directory = self.directory.clone();
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new()
            .name("jwm-notes-search".into())
            .spawn(move || {
                let rows = scan_rows(&directory, &query, || worker_flag.load(Ordering::Relaxed));
                // Publish before waking the event loop. A closed window owns no receiver.
                let _ = tx.send(rows);
                wake();
            }) {
            Ok(_) => {
                self.search_active = Some(ActiveSearch {
                    result: rx,
                    cancelled: flag,
                })
            }
            Err(_) => self.refresh(),
        }
    }

    pub fn poll_search(&mut self) -> bool {
        let Some(active) = &self.search_active else {
            return false;
        };
        let result = match active.result.try_recv() {
            Ok(rows) => Some(rows),
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => None,
        };
        let active = self.search_active.take().expect("active scan");
        if active.cancelled.load(Ordering::Relaxed) {
            self.start_scan();
            // Start may synchronously recover from a thread creation failure.
            return !self.search_pending();
        }
        match result {
            Some(rows) => self.publish_rows(rows),
            None => self.refresh(),
        }
        true
    }

    fn publish_rows(&mut self, rows: NotesRows) {
        self.painted_refresh = false;
        self.rows = rows;
        self.list_scroll = self
            .list_scroll
            .min((self.rows.len() as f64 * 48. - 48.).max(0.));
    }

    pub fn new_note(&mut self) -> Result<(), String> {
        self.save()?;
        let path = create_note(&self.directory)?;
        self.load(path)?;
        self.search = Editor::default();
        self.refresh();
        self.list_scroll = 0.;
        Ok(())
    }
    pub fn trash(&mut self) -> Result<(), String> {
        self.save()?;
        if let Some(doc) = &self.document {
            doc.file
                .trash(gio::Cancellable::NONE)
                .map_err(|e| e.to_string())?;
        }
        self.document = None;
        self.refresh();
        if let Some((path, _)) = self.rows.first() {
            self.load(path.clone())?;
        } else {
            self.new_note()?;
        }
        Ok(())
    }
}

impl Drop for Notes {
    fn drop(&mut self) {
        self.cancel_scan();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reloading_changed_external_text_keeps_the_logical_caret() {
        let dir = std::env::temp_dir().join(format!("jwm-reload-caret-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("external.txt");
        std::fs::write(&path, "old\r\n中e\u{301} suffix\r\nthird").unwrap();
        let mut notes = Notes::new(Some(path.clone())).unwrap();
        let old_caret = "old\n中e\u{301}".len();
        notes.editor.select(0..old_caret);
        assert_eq!(notes.editor.position(), (2, 3));

        let updated = "new header\r\n👩‍💻文 changed\r\nlast";
        std::fs::write(&path, updated).unwrap();
        assert!(notes.reload_external().unwrap());
        assert_eq!(notes.editor.position(), (2, 3));
        let caret = "new header\n👩‍💻文".len();
        assert_eq!(notes.editor.selection(), caret..caret);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
        assert!(!notes.dirty);

        std::fs::write(&path, "短").unwrap();
        assert!(notes.reload_external().unwrap());
        assert_eq!(notes.editor.position(), (1, 2));
        assert_eq!(notes.editor.selection(), "短".len().."短".len());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_refresh_supersedes_an_already_completed_background_query() {
        let dir = std::env::temp_dir().join(format!("jwm-bg-save-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let current = dir.join("current.txt");
        let external = dir.join("external.txt");
        std::fs::write(&current, "source text").unwrap();
        let mut notes = Notes::new(Some(current.clone())).unwrap();
        notes.managed = true;
        notes.directory = dir.clone();
        notes.refresh();

        let (wake, ready) = mpsc::channel();
        notes.set_search_wake(Arc::new(move || {
            let _ = wake.send(());
        }));
        notes.search.text = "needle".into();
        notes.update_query();
        ready
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        assert!(notes.current_rows().is_none());

        std::fs::write(&external, "Fresh needle note").unwrap();
        notes.editor.insert("needle ");
        notes.edited();
        notes.save().unwrap();
        assert_eq!(notes.rows.len(), 2);
        assert!(notes.current_rows().is_some());
        let latest = notes.rows.clone();
        assert!(notes.poll_search());
        assert_eq!(notes.rows, latest);
        assert_eq!(
            std::fs::read_to_string(&current).unwrap(),
            "needle source text"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn autosave_persists_before_return_and_publishes_only_the_latest_search() {
        let dir = std::env::temp_dir().join(format!("jwm-autosave-worker-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let current = dir.join("current.txt");
        let other = dir.join("other.txt");
        std::fs::write(&current, "old title").unwrap();
        std::fs::write(&other, "another old title").unwrap();
        let mut notes = Notes::new(Some(current.clone())).unwrap();
        notes.managed = true;
        notes.directory = dir.clone();
        notes.refresh();
        let sync_refreshes = notes.refreshes;
        let (send, ready) = mpsc::channel();
        notes.set_search_wake(Arc::new(move || {
            let _ = send.send(());
        }));
        let wait = || {
            ready
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap()
        };

        notes.search.text = "old".into();
        notes.update_query();
        wait(); // Result is queued but not polled yet.

        notes.editor = Editor::new("intermediate title".into());
        notes.edited();
        notes.autosave().unwrap();
        assert!(!notes.dirty && notes.due.is_none());
        assert_eq!(
            std::fs::read_to_string(&current).unwrap(),
            "intermediate title"
        );
        assert_eq!(notes.status, "已保存");
        assert!(notes.current_rows().is_none());

        notes.search.text = "latest".into();
        notes.update_query();
        notes.editor = Editor::new("latest title".into());
        notes.edited();
        notes.autosave().unwrap();
        assert_eq!(std::fs::read_to_string(&current).unwrap(), "latest title");
        assert!(notes.current_rows().is_none());
        assert!(!notes.poll_search()); // Discard the old result and start the newest.
        assert!(notes.current_rows().is_none());
        wait();
        assert!(notes.poll_search());
        assert!(notes.current_rows().is_some());
        assert_eq!(notes.rows, vec![(current, "latest title".into())]);
        assert_eq!(notes.refreshes, sync_refreshes);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_autosave_keeps_settled_rows_painted_but_never_actions_an_outdated_filter() {
        let dir = std::env::temp_dir().join(format!("jwm-autosave-paint-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let current = dir.join("current.txt");
        std::fs::write(&current, "old title").unwrap();
        let mut notes = Notes::new(Some(current.clone())).unwrap();
        notes.managed = true;
        notes.directory = dir.clone();
        notes.search.text = "old".into();
        notes.refresh();
        let (send, ready) = mpsc::channel();
        notes.set_search_wake(Arc::new(move || {
            let _ = send.send(());
        }));
        let wait = || {
            ready
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap()
        };

        notes.editor = Editor::new("new title".into());
        notes.edited();
        notes.autosave().unwrap();
        assert!(notes.painted_refresh());
        assert_eq!(notes.rows, vec![(current.clone(), "old title".into())]);
        assert!(notes.current_rows().is_none());
        wait();

        notes.search.text = "new".into();
        notes.update_query();
        assert!(!notes.painted_refresh());
        assert!(!notes.poll_search());
        wait();
        assert!(notes.poll_search());
        assert!(!notes.painted_refresh() && notes.current_rows().is_some());
        assert_eq!(notes.rows, vec![(current, "new title".into())]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn open_result_refreshes_once_and_keeps_save_and_load_failures_distinct() {
        let dir = std::env::temp_dir().join(format!("jwm-note-switch-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let first = dir.join("first.txt");
        let second = dir.join("second.txt");
        let decoy = dir.join("decoy.txt");
        std::fs::write(&first, "First").unwrap();
        std::fs::write(&second, "Second needle").unwrap();
        std::fs::write(&decoy, "Different").unwrap();
        let mut notes = Notes::new(Some(first.clone())).unwrap();
        notes.managed = true;
        notes.directory = dir.clone();
        notes.search.text = "needle".into();
        notes.refresh();
        let scans = notes.refreshes;

        notes.editor.insert("needle ");
        notes.edited();
        std::fs::write(&decoy, "Externally changed needle").unwrap();
        notes.open_result(second.clone()).unwrap();
        assert_eq!(notes.refreshes, scans + 1);
        assert_eq!(notes.editor.text, "Second needle");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "needle First");
        assert!(notes.rows.iter().any(|(path, _)| path == &decoy));

        // A clean open also refreshes externally edited notes.
        std::fs::write(&decoy, "Removed from results").unwrap();
        notes.open_result(first.clone()).unwrap();
        assert_eq!(notes.refreshes, scans + 2);
        assert!(!notes.rows.iter().any(|(path, _)| path == &decoy));

        notes.editor.insert("new ");
        notes.edited();
        let missing = dir.join("missing.txt");
        assert!(notes.open_result(missing).is_err());
        assert_eq!(notes.refreshes, scans + 3);
        assert_eq!(
            notes.document.as_ref().unwrap().file.path(),
            Some(first.clone())
        );
        assert!(!notes.dirty);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "new needle First");
        assert!(
            notes
                .rows
                .iter()
                .any(|(path, title)| path == &first && title == "new needle First")
        );

        notes.editor.insert("pending ");
        notes.edited();
        std::fs::write(&first, "external file replacement").unwrap();
        assert!(notes.open_result(second).is_err());
        assert_eq!(notes.refreshes, scans + 3);
        assert_eq!(
            notes.document.as_ref().unwrap().file.path(),
            Some(first.clone())
        );
        assert_eq!(notes.editor.text, "new pending needle First");
        assert!(notes.dirty);
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            "external file replacement"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn file_size_boundary_counts_bom_and_encoded_newlines() {
        let dir = std::env::temp_dir().join(format!("jwm-size-limit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let limit = MAX_TEXT_BYTES as usize;
        for bom in [false, true] {
            let path = dir.join(format!("boundary-{bom}.txt"));
            let body = "a".repeat(limit - if bom { 3 } else { 0 });
            let original = if bom {
                format!("\u{feff}{body}")
            } else {
                body.clone()
            };
            std::fs::write(&path, &original).unwrap();
            let (mut file, loaded) = TextFile::load(gio::File::for_path(&path)).unwrap();
            assert!(
                file.etag.is_some(),
                "local GIO stream must retain etag protection"
            );
            assert_eq!(loaded, body);
            file.save(&loaded).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
            let error = file.save(&format!("{loaded}x")).unwrap_err();
            assert!(matches!(error, SaveError::Other(message) if message.contains("8 MiB")));
            assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
            std::fs::write(&path, format!("{original}x")).unwrap();
            assert!(TextFile::load(gio::File::for_path(&path)).is_err());
        }
        // The editor holds LF but saving a uniform-CRLF document expands it.
        let path = dir.join("crlf.txt");
        std::fs::write(&path, "\u{feff}a\r\n").unwrap();
        let mut notes = Notes::new(Some(path.clone())).unwrap();
        notes.editor.select_all();
        let boundary = format!("{}\n", "a".repeat(limit - 5));
        notes.editor.insert(&boundary);
        notes.edited();
        notes.save().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), MAX_TEXT_BYTES);
        assert_eq!(
            TextFile::load(gio::File::for_path(&path)).unwrap().1,
            boundary
        );
        let before = std::fs::read(&path).unwrap();
        notes.editor.insert("x");
        notes.edited();
        let text = notes.editor.text.clone();
        let revision = notes.editor.revision;
        assert!(notes.save().unwrap_err().contains("8 MiB"));
        let existing = dir.join("existing.txt");
        std::fs::write(&existing, "do not overwrite").unwrap();
        assert!(notes.save_as(&existing).is_err());
        assert_eq!(
            std::fs::read_to_string(&existing).unwrap(),
            "do not overwrite"
        );
        let absent = dir.join("absent.txt");
        assert!(notes.save_as(&absent).is_err());
        assert!(!absent.exists());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(notes.editor.text, text);
        assert_eq!(notes.editor.revision, revision);
        assert!(notes.dirty);
        // Recovery is possible without closing or discarding the document.
        notes.editor.select_all();
        notes.editor.insert("short\n");
        notes.edited();
        notes.save().unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "\u{feff}short\r\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_metadata_cannot_bypass_read_limit_or_read_consistency() {
        let path = std::env::temp_dir().join(format!("jwm-read-race-{}.txt", std::process::id()));
        std::fs::write(&path, "old").unwrap();
        let result = TextFile::load_with_reader(gio::File::for_path(&path), |stream| {
            // Deterministically grow the same file after metadata/open/etag.
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(MAX_TEXT_BYTES + 1)
                .unwrap();
            read_limited(stream.into_read())
        });
        assert!(result.err().unwrap().contains("实际内容超过"));
        std::fs::write(&path, "old").unwrap();
        let result = TextFile::load_with_reader(gio::File::for_path(&path), |stream| {
            let replacement = path.with_extension("replacement");
            std::fs::write(&replacement, "new").unwrap();
            std::fs::rename(replacement, &path).unwrap();
            read_limited(stream.into_read())
        });
        assert!(result.err().unwrap().contains("读取期间"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        // Even a reader with no trustworthy metadata consumes only limit + 1.
        let mut reader = std::io::Cursor::new(vec![b'a'; MAX_TEXT_BYTES as usize + 2]);
        assert!(read_limited(&mut reader).is_err());
        assert_eq!(reader.position(), MAX_TEXT_BYTES + 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn load_save_preserves_mixed_line_endings_and_bom() {
        let dir = std::env::temp_dir().join(format!("jwm-mixed-eol-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (i, original) in [
            "a\r\nb\nc",
            "a\r\nb\nc\n",
            "a\r\nb\nc\r\n",
            "\u{feff}a\r\nb\nc",
            "\u{feff}a\r\nb\nc\n",
            "\u{feff}a\r\nb\nc\r\n",
            "a\r\nb\r\n",
            "a\nb\n",
        ]
        .iter()
        .enumerate()
        {
            let path = dir.join(format!("{i}.txt"));
            std::fs::write(&path, original).unwrap();
            let (mut file, text) = TextFile::load(gio::File::for_path(&path)).unwrap();
            file.save(&text).unwrap();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                *original,
                "case {i}"
            );
            file.save(&format!("edit {text}")).unwrap();
            let (bom, body) = if let Some(body) = original.strip_prefix('\u{feff}') {
                ("\u{feff}", body)
            } else {
                ("", *original)
            };
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                format!("{bom}edit {body}")
            );
            let mut notes = Notes::new(Some(path.clone())).unwrap();
            let copy = dir.join(format!("{i}-copy.txt"));
            notes.save_as(&copy).unwrap();
            assert_eq!(std::fs::read(copy).unwrap(), std::fs::read(path).unwrap());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn editing_mixed_crlf_and_bare_cr_does_not_double_inserted_breaks() {
        let dir = std::env::temp_dir().join(format!("jwm-crlf-bare-cr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.txt");
        let original = "\u{feff}甲\r\nsecond\rthird\r\n";
        std::fs::write(&source, original).unwrap();

        let (mut file, text) = TextFile::load(gio::File::for_path(&source)).unwrap();
        assert!(!file.crlf);
        assert_eq!(text, "甲\r\nsecond\rthird\r\n");
        let mut notes = Notes::new(Some(source.clone())).unwrap();
        notes.editor.select(0..0);
        notes.editor.insert("new\r\n");
        let expected = "\u{feff}new\r\n甲\r\nsecond\rthird\r\n";
        let copy = dir.join("copy.txt");
        notes.save_as(&copy).unwrap();
        file.save(&notes.editor.text).unwrap();
        assert_eq!(std::fs::read_to_string(copy).unwrap(), expected);
        assert_eq!(std::fs::read_to_string(source).unwrap(), expected);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_literal_crlf_pairs_select_windows_conversion_mode() {
        assert!(uses_only_crlf("甲\r\nb\r\n"));
        for other in ["none", "\r", "\n", "\r\r\n", "\r\n\r", "\r\n\n"] {
            assert!(!uses_only_crlf(other), "{other:?}");
        }
    }

    #[test]
    fn preserves_bom_and_windows_line_endings() {
        assert_eq!(
            encode_text("备忘\n第二行\n", true, true).unwrap(),
            "\u{feff}备忘\r\n第二行\r\n"
        );
    }

    #[test]
    fn conflict_copy_keeps_bytes_and_skips_long_name_and_symlink_collisions() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory =
            std::env::temp_dir().join(format!("jwm-notes-keep-conflict-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let source = directory.join(format!("{}.md", "备".repeat(83)));
        std::fs::write(&source, "\u{feff}first\r\nsecond\r\n").unwrap();
        let mut notes = Notes::new(Some(source.clone())).unwrap();
        notes.editor.text = "我的草稿\n第二行\n".into();
        notes.edited();

        let replacement = directory.join("replacement");
        std::fs::write(&replacement, "outside edit").unwrap();
        std::fs::rename(replacement, &source).unwrap();
        assert!(matches!(notes.save_explicitly(), Err(SaveError::Conflict)));

        let occupied = directory.join(recovery_name(&source, 1).unwrap());
        std::fs::write(&occupied, "keep existing").unwrap();
        let symlink_path = directory.join(recovery_name(&source, 2).unwrap());
        let missing = directory.join("not-created");
        symlink(&missing, &symlink_path).unwrap();

        let open_but_missing = directory.join(recovery_name(&source, 3).unwrap());
        let recovered = notes
            .keep_conflict_copy(|path| path == open_but_missing.as_path())
            .unwrap();
        assert!(!open_but_missing.exists());
        assert_eq!(
            recovered,
            directory.join(recovery_name(&source, 4).unwrap())
        );
        assert_eq!(recovered.extension().unwrap(), "md");
        assert!(recovered.file_name().unwrap().to_str().unwrap().len() <= 255);
        assert_eq!(
            std::fs::read_to_string(&recovered).unwrap(),
            "\u{feff}我的草稿\r\n第二行\r\n"
        );
        assert_eq!(
            recovered.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!notes.dirty);
        assert_eq!(
            notes.document.as_ref().unwrap().file.path().as_deref(),
            Some(recovered.as_path())
        );
        assert!(notes.status.contains(&*recovered.to_string_lossy()));
        notes.editor.text.push_str("再次保存\n");
        notes.edited();
        notes.save_explicitly().unwrap();
        assert_eq!(
            std::fs::read_to_string(recovered).unwrap(),
            "\u{feff}我的草稿\r\n第二行\r\n再次保存\r\n"
        );
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "outside edit");
        assert_eq!(std::fs::read_to_string(occupied).unwrap(), "keep existing");
        assert_eq!(std::fs::read_link(symlink_path).unwrap(), missing);
        assert!(!missing.exists());
        assert!(std::fs::read_dir(&directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".jwm-notes-")
        }));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn exhausted_recovery_names_leave_the_original_editor_dirty() {
        let directory =
            std::env::temp_dir().join(format!("jwm-notes-recovery-full-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let source = directory.join("active.txt");
        std::fs::write(&source, "old contents").unwrap();
        let mut notes = Notes::new(Some(source.clone())).unwrap();
        notes.editor.text = "unsaved edit".into();
        notes.edited();
        std::fs::write(&source, "other contents").unwrap();
        for index in 1..=64 {
            let path = directory.join(recovery_name(&source, index).unwrap());
            std::fs::write(path, "already occupied").unwrap();
        }

        assert!(notes.keep_conflict_copy(|_| false).is_err());
        assert!(notes.dirty);
        assert_eq!(notes.editor.text, "unsaved edit");
        assert_eq!(
            notes.document.as_ref().unwrap().file.path().as_deref(),
            Some(source.as_path())
        );
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "other contents");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 65);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn notes_are_private_unique_and_conflicting_edits_are_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!("jwm-notes-test-{}", std::process::id()));
        let path = create_note(&directory).unwrap();
        let second = create_note(&directory).unwrap();
        assert_ne!(path, second);
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        let (mut document, _) = TextFile::load(gio::File::for_path(&path)).unwrap();
        document.save("购物清单\n牛奶").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "购物清单\n牛奶");
        // Atomic replacement guarantees a different inode/etag on all filesystems.
        let other = directory.join("other");
        std::fs::write(&other, "changed elsewhere").unwrap();
        std::fs::rename(other, &path).unwrap();
        assert!(document.save("would overwrite").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "changed elsewhere");
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[cfg(test)]
mod refresh_semantics_tests {
    use super::*;
    #[test]
    fn empty_query_preserves_validation_unicode_order_and_external_changes() {
        use std::{
            fs::{File, FileTimes},
            time::{Duration, SystemTime},
        };
        let dir =
            std::env::temp_dir().join(format!("jwm-refresh-semantics-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        struct Clean(PathBuf);
        impl Drop for Clean {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Clean(dir.clone());
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        let hidden = dir.join(".hidden.txt");
        std::fs::write(&a, "École Αθήνα 中文 ASCII\nbody").unwrap();
        std::fs::write(&b, "Other").unwrap();
        std::fs::write(&hidden, "Hidden title").unwrap();
        std::fs::write(dir.join("invalid.txt"), b"Valid title\ninvalid later: \xff").unwrap();
        std::fs::write(dir.join("ignored.md"), "Not a managed note").unwrap();
        std::fs::create_dir(dir.join("directory.txt")).unwrap();
        File::create(dir.join("oversize.txt"))
            .unwrap()
            .set_len(MAX_TEXT_BYTES + 1)
            .unwrap();
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        for path in [&a, &b] {
            File::open(path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(old))
                .unwrap();
        }
        File::open(&hidden)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old + Duration::from_secs(1)))
            .unwrap();
        let mut notes = Notes::new(Some(a.clone())).unwrap();
        notes.managed = true;
        notes.directory = dir.clone();
        notes.refresh();
        assert_eq!(
            notes.rows.iter().map(|v| v.0.clone()).collect::<Vec<_>>(),
            vec![hidden.clone(), a.clone(), b.clone()]
        );
        for query in ["ascii", "éCOLE", "ΑΘΉΝΑ", "中文"] {
            notes.search.text = query.into();
            notes.refresh();
            assert_eq!(notes.rows.len(), 1, "{query}");
            assert_eq!(notes.rows[0].0, a);
        }
        std::fs::write(&a, "External replacement").unwrap();
        notes.search.text = "école".into();
        notes.refresh();
        assert!(notes.rows.is_empty());
        let renamed = dir.join("renamed.txt");
        std::fs::rename(&hidden, &renamed).unwrap();
        std::fs::remove_file(&b).unwrap();
        notes.search.text.clear();
        notes.refresh();
        assert_eq!(notes.rows.len(), 2);
        assert!(
            notes
                .rows
                .iter()
                .any(|v| v.0 == renamed && v.1 == "Hidden title")
        );
        assert!(
            notes
                .rows
                .iter()
                .any(|v| v.0 == a && v.1 == "External replacement")
        );
        assert!(!notes.rows.iter().any(|v| v.0 == hidden || v.0 == b));
    }
}
