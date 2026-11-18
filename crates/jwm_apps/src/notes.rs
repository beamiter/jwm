use crate::{editor::Editor, note_title};
use gio::{self, glib, prelude::*};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
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
        let crlf = text.contains("\r\n") && !text.replace("\r\n", "").contains('\n');
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

    pub fn save(&mut self, text: &str) -> Result<(), String> {
        if FileStamp::read(&self.file)? != self.stamp {
            return Err("文件已被其他程序修改，请另存为以保留当前内容。".into());
        }
        let text = encode_text(text, self.bom, self.crlf)?;
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
                format!("保存失败：{error}\n如果文件已被其他程序修改，请另存为以保留当前内容。")
            })?;
        self.stamp = FileStamp::read(&self.file)?;
        Ok(())
    }
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

pub(crate) struct Notes {
    pub document: Option<TextFile>,
    pub editor: Editor,
    pub preview: crate::preview::Preview,
    pub search: Editor,
    pub find: crate::find::Find,
    pub directory: PathBuf,
    pub rows: Vec<(PathBuf, String)>,
    pub managed: bool,
    pub dirty: bool,
    pub due: Option<Instant>,
    pub status: String,
    pub list_scroll: f64,
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
            managed: path.is_none(),
            dirty: false,
            due: None,
            status: "自动保存已开启 · Ctrl+N 新建 · Ctrl+F 搜索".into(),
            list_scroll: 0.,
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
    pub fn save(&mut self) -> Result<(), String> {
        self.due = None;
        if !self.dirty {
            return Ok(());
        }
        self.document
            .as_mut()
            .ok_or("没有可保存的文件。")?
            .save(&self.editor.text)?;
        self.dirty = false;
        self.status = "已保存".into();
        if self.managed {
            self.refresh();
        }
        Ok(())
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
        let query = self.search.text.to_lowercase();
        let mut entries: Vec<_> = std::fs::read_dir(&self.directory)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|s| s == "txt"))
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                if !meta.is_file() || meta.len() > MAX_TEXT_BYTES {
                    return None;
                }
                let bytes = read_limited(std::fs::File::open(e.path()).ok()?).ok()?;
                let text = std::str::from_utf8(&bytes).ok()?;
                text.to_lowercase()
                    .contains(&query)
                    .then(|| (e.path(), note_title(text), meta.modified().ok()))
            })
            .collect();
        entries.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        self.rows = entries.into_iter().map(|(p, t, _)| (p, t)).collect();
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

#[cfg(test)]
mod tests {
    use super::*;

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
            assert!(
                file.save(&format!("{loaded}x"))
                    .unwrap_err()
                    .contains("8 MiB")
            );
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
    fn preserves_bom_and_windows_line_endings() {
        assert_eq!(
            encode_text("备忘\n第二行\n", true, true).unwrap(),
            "\u{feff}备忘\r\n第二行\r\n"
        );
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
