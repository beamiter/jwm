use crate::{
    action, error, local_path, note_title,
    ui::{self, Icon, button},
};
use gtk::{gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
};

const MAX_TEXT_BYTES: u64 = 8 * 1024 * 1024;

/// An etag protects edits from overwriting a file changed by another process.
pub struct TextFile {
    pub file: gio::File,
    etag: Option<glib::GString>,
    bom: bool,
    crlf: bool,
}

impl TextFile {
    pub fn load(file: gio::File) -> Result<(Self, String), String> {
        let path = local_path(&file)?;
        let metadata = path.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() || metadata.len() > MAX_TEXT_BYTES {
            return Err("请选择不超过 8 MiB 的 UTF-8 文本文件。".into());
        }
        let (bytes, etag) = file
            .load_contents(gio::Cancellable::NONE)
            .map_err(|error| error.to_string())?;
        let text =
            std::str::from_utf8(&bytes).map_err(|_| "文件不是 UTF-8 文本，未修改原文件。")?;
        let bom = text.starts_with('\u{feff}');
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let crlf = text.contains("\r\n") && !text.replace("\r\n", "").contains('\n');
        let text = text.replace("\r\n", "\n");
        Ok((
            Self {
                file,
                etag,
                bom,
                crlf,
            },
            text,
        ))
    }

    pub fn save(&mut self, text: &str) -> Result<(), String> {
        let text = encode_text(text, self.bom, self.crlf);
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
        Ok(())
    }
}

fn encode_text(text: &str, bom: bool, crlf: bool) -> String {
    let text = if crlf {
        text.replace('\n', "\r\n")
    } else {
        text.to_owned()
    };
    if bom { format!("\u{feff}{text}") } else { text }
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

struct Notes {
    window: gtk::ApplicationWindow,
    title: gtk::Label,
    status: gtk::Label,
    editor: gtk::TextView,
    buffer: gtk::TextBuffer,
    list: gtk::ListBox,
    search: gtk::SearchEntry,
    directory: PathBuf,
    document: RefCell<Option<TextFile>>,
    rows: RefCell<Vec<PathBuf>>,
    managed: bool,
    loading: Cell<bool>,
    dirty: Cell<bool>,
    closing: Cell<bool>,
    autosave: RefCell<Option<glib::SourceId>>,
}

pub fn open(app: &gtk::Application, file: Option<gio::File>) {
    open_window(app, file, false);
}

fn open_window(app: &gtk::Application, file: Option<gio::File>, create_new: bool) {
    // Repeated desktop Open requests focus an existing editor for the same file.
    if let Some(file) = &file {
        for window in app.windows() {
            if window.widget_name() == file.uri().as_str() {
                window.present();
                return;
            }
        }
    }
    let managed = file.is_none();
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("JWM 备忘录")
        .default_width(960)
        .default_height(680)
        .build();
    let title = gtk::Label::new(Some("备忘录"));
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let header = ui::header(&window, Icon::Notes, "JWM 备忘录", &title);
    let save = button(Icon::Save, "保存 (Ctrl+S)", "win.save");
    let export = button(Icon::Export, "另存为 / 导出 (Ctrl+Shift+S)", "win.save-as");
    let document_tools = ui::group(&[&save, &export]);
    if managed {
        document_tools.append(&button(Icon::Trash, "将备忘录移至废纸篓", "win.trash"));
    }
    header.pack_end(&document_tools);
    header.pack_end(&ui::group(&[
        &button(Icon::New, "新建备忘录 (Ctrl+N)", "win.new"),
        &button(Icon::Folder, "打开文本文件 (Ctrl+O)", "win.open"),
    ]));
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let body = gtk::Paned::new(gtk::Orientation::Horizontal);
    body.set_vexpand(true);
    body.set_position(260);
    body.set_resize_start_child(false);
    body.set_shrink_start_child(false);
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 8);
    sidebar.set_size_request(210, -1);
    sidebar.add_css_class("jwm-sidebar");
    let search = gtk::SearchEntry::builder()
        .placeholder_text("搜索备忘录")
        .css_classes(["jwm-search"])
        .build();
    search.set_margin_top(12);
    search.set_margin_start(12);
    search.set_margin_end(12);
    sidebar.append(&search);
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();
    sidebar.append(&scroll);
    if managed {
        body.set_start_child(Some(&sidebar));
    }
    let buffer = gtk::TextBuffer::new(None);
    buffer.set_enable_undo(true);
    let editor = gtk::TextView::builder()
        .buffer(&buffer)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(32)
        .bottom_margin(32)
        .left_margin(32)
        .right_margin(32)
        .accepts_tab(true)
        .editable(false)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&editor)
        .build();
    body.set_end_child(Some(&scroll));
    root.append(&body);
    let status = ui::status();
    root.append(&status);
    window.set_child(Some(&root));
    let notes = Rc::new(Notes {
        window,
        title,
        status,
        editor,
        buffer,
        list,
        search,
        directory: glib::user_data_dir().join("jwm/notes"),
        document: RefCell::new(None),
        rows: RefCell::new(vec![]),
        managed,
        loading: Cell::new(false),
        dirty: Cell::new(false),
        closing: Cell::new(false),
        autosave: RefCell::new(None),
    });
    notes.connect();
    if let Some(file) = file {
        if let Err(message) = notes.load(file) {
            error(&notes.window, &message);
        }
    } else {
        notes.refresh();
        let first = notes.rows.borrow().first().cloned();
        if let Some(path) = first.filter(|_| !create_new) {
            if let Err(message) = notes.load(gio::File::for_path(path)) {
                error(&notes.window, &message);
            }
        } else {
            notes.new_note();
        }
    }
    notes.window.present();
    notes.editor.grab_focus();
}

impl Notes {
    fn text(&self) -> String {
        self.buffer
            .text(&self.buffer.start_iter(), &self.buffer.end_iter(), true)
            .to_string()
    }

    fn load(&self, file: gio::File) -> Result<(), String> {
        let (document, text) = TextFile::load(file)?;
        self.window.set_widget_name(document.file.uri().as_str());
        *self.document.borrow_mut() = Some(document);
        self.loading.set(true);
        // Non-undoable replacement keeps undo history local to this document.
        self.buffer.begin_irreversible_action();
        self.buffer.set_text(&text);
        self.buffer.end_irreversible_action();
        self.loading.set(false);
        self.dirty.set(false);
        self.editor.set_editable(true);
        self.update_title();
        self.status.set_text(if self.managed {
            "自动保存已开启 · Ctrl+N 新建 · Ctrl+Shift+S 导出"
        } else {
            "UTF-8 · Ctrl+S 保存 · Ctrl+Shift+S 另存为"
        });
        Ok(())
    }

    fn update_title(&self) {
        let name = if self.managed {
            note_title(&self.text())
        } else {
            self.document
                .borrow()
                .as_ref()
                .and_then(|doc| doc.file.basename())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "文本文件".into())
        };
        let name = format!("{}{name}", if self.dirty.get() { "• " } else { "" });
        self.title.set_text(&name);
        self.window.set_title(Some(&format!("{name} — JWM 备忘录")));
    }

    fn save(&self) -> bool {
        if !self.dirty.get() {
            return true;
        }
        let result = self
            .document
            .borrow_mut()
            .as_mut()
            .ok_or_else(|| "没有可保存的文件。".to_owned())
            .and_then(|document| document.save(&self.text()));
        match result {
            Ok(()) => {
                self.dirty.set(false);
                self.update_title();
                self.status.set_text(if self.managed {
                    "已自动保存"
                } else {
                    "已保存"
                });
                if self.managed {
                    self.refresh();
                }
                true
            }
            Err(message) => {
                self.status
                    .set_text("保存失败，内容仍在编辑器中；请重试或另存为");
                error(&self.window, &message);
                false
            }
        }
    }

    fn refresh(&self) {
        let query = self.search.text().to_lowercase();
        let mut entries: Vec<_> = std::fs::read_dir(&self.directory)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "txt"))
            .filter_map(|entry| {
                let metadata = entry.metadata().ok()?;
                if !metadata.is_file() || metadata.len() > MAX_TEXT_BYTES {
                    return None;
                }
                let text = std::fs::read_to_string(entry.path()).ok()?;
                if !text.to_lowercase().contains(&query) {
                    return None;
                }
                Some((entry.path(), note_title(&text), metadata.modified().ok()))
            })
            .collect();
        entries.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        self.loading.set(true);
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let current = self
            .document
            .borrow()
            .as_ref()
            .and_then(|doc| doc.file.path());
        self.rows.borrow_mut().clear();
        for (index, (path, title, _)) in entries.into_iter().enumerate() {
            let label = gtk::Label::builder()
                .label(&title)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .margin_top(12)
                .margin_bottom(12)
                .margin_start(8)
                .margin_end(8)
                .build();
            self.list.append(&label);
            if current.as_ref() == Some(&path) {
                self.list
                    .select_row(self.list.row_at_index(index as i32).as_ref());
            }
            self.rows.borrow_mut().push(path);
        }
        self.loading.set(false);
    }

    fn new_note(&self) {
        if !self.save() {
            return;
        }
        match create_note(&self.directory).and_then(|path| self.load(gio::File::for_path(path))) {
            Ok(()) => {
                self.search.set_text("");
                self.refresh();
                self.editor.grab_focus();
            }
            Err(message) => error(&self.window, &message),
        }
    }

    fn save_as(self: &Rc<Self>, close: bool) {
        let dialog = gtk::FileDialog::builder()
            .title("另存为文本文件")
            .initial_name(format!(
                "{}.txt",
                note_title(&self.text()).replace(['/', '\\'], "_")
            ))
            .build();
        let weak = Rc::downgrade(self);
        dialog.save(Some(&self.window), gio::Cancellable::NONE, move |result| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if let Ok(file) = result {
                if let Err(message) = local_path(&file) {
                    error(&this.window, &message);
                    return;
                }
                let same = this
                    .document
                    .borrow()
                    .as_ref()
                    .is_some_and(|doc| doc.file == file);
                let result = if same {
                    if this.save() {
                        Ok(())
                    } else {
                        return;
                    }
                } else {
                    let (bom, crlf) = this
                        .document
                        .borrow()
                        .as_ref()
                        .map(|doc| (doc.bom, doc.crlf))
                        .unwrap_or_default();
                    file.replace_contents(
                        encode_text(&this.text(), bom, crlf).as_bytes(),
                        None,
                        false,
                        gio::FileCreateFlags::PRIVATE,
                        gio::Cancellable::NONE,
                    )
                    .map(|etag| {
                        if !this.managed {
                            this.window.set_widget_name(file.uri().as_str());
                            *this.document.borrow_mut() = Some(TextFile {
                                file,
                                etag,
                                bom,
                                crlf,
                            });
                            this.dirty.set(false);
                            this.update_title();
                        }
                    })
                    .map_err(|error| error.to_string())
                };
                match result {
                    Ok(()) => {
                        this.status.set_text(if this.managed {
                            "已导出文本文件"
                        } else {
                            "已保存"
                        });
                        if close {
                            this.window.destroy();
                        }
                    }
                    Err(message) => error(&this.window, &message),
                }
            }
        });
    }

    fn connect(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.buffer.connect_changed(move |_| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if this.loading.get() {
                return;
            }
            this.dirty.set(true);
            this.update_title();
            this.status.set_text(if this.managed {
                "等待自动保存…"
            } else {
                "有未保存的修改 · Ctrl+S 保存"
            });
            if let Some(source) = this.autosave.borrow_mut().take() {
                source.remove();
            }
            if this.managed {
                let weak = Rc::downgrade(&this);
                *this.autosave.borrow_mut() = Some(glib::timeout_add_local_once(
                    std::time::Duration::from_millis(700),
                    move || {
                        if let Some(this) = weak.upgrade() {
                            this.autosave.borrow_mut().take();
                            this.save();
                        }
                    },
                ));
            }
        });
        let weak = Rc::downgrade(self);
        self.search.connect_search_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.refresh();
            }
        });
        let weak = Rc::downgrade(self);
        self.list.connect_row_selected(move |_, row| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if this.loading.get() {
                return;
            }
            let path = row.and_then(|row| this.rows.borrow().get(row.index() as usize).cloned());
            if let Some(path) = path {
                if this
                    .document
                    .borrow()
                    .as_ref()
                    .and_then(|doc| doc.file.path())
                    .as_ref()
                    == Some(&path)
                {
                    return;
                }
                if !this.save() {
                    this.refresh();
                    return;
                }
                if let Err(message) = this.load(gio::File::for_path(path)) {
                    error(&this.window, &message);
                }
                this.refresh();
            }
        });
        let this = self.clone();
        action(&self.window, "new", &["<Control>n"], move || {
            if this.managed {
                this.new_note();
            } else if let Some(app) = this.window.application() {
                open_window(&app, None, true);
            }
        });
        let this = self.clone();
        action(&self.window, "save", &["<Control>s"], move || {
            this.save();
        });
        let this = self.clone();
        action(&self.window, "save-as", &["<Control><Shift>s"], move || {
            this.save_as(false)
        });
        let this = self.clone();
        action(&self.window, "search", &["<Control>f"], move || {
            if this.managed {
                this.search.grab_focus();
            }
        });
        let this = self.clone();
        action(&self.window, "open", &["<Control>o"], move || {
            let dialog = gtk::FileDialog::builder()
                .title("打开 UTF-8 文本文件")
                .build();
            let weak = Rc::downgrade(&this);
            dialog.open(Some(&this.window), gio::Cancellable::NONE, move |result| {
                if let (Some(this), Ok(file)) = (weak.upgrade(), result)
                    && let Some(app) = this.window.application()
                {
                    open(&app, Some(file));
                }
            });
        });
        let this = self.clone();
        action(&self.window, "trash", &[], move || {
            if !this.managed || this.document.borrow().is_none() {
                return;
            }
            let dialog = gtk::AlertDialog::builder()
                .message("将当前备忘录移至废纸篓？")
                .detail("可以从系统废纸篓恢复此备忘录。")
                .buttons(["取消", "移至废纸篓"])
                .cancel_button(0)
                .default_button(0)
                .build();
            let weak = Rc::downgrade(&this);
            dialog.choose(Some(&this.window), gio::Cancellable::NONE, move |choice| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if choice != Ok(1) || !this.save() {
                    return;
                }
                let result = this
                    .document
                    .borrow()
                    .as_ref()
                    .map(|doc| doc.file.trash(gio::Cancellable::NONE));
                match result {
                    Some(Ok(())) => {
                        this.document.borrow_mut().take();
                        this.refresh();
                        let next = this.rows.borrow().first().cloned();
                        if let Some(path) = next {
                            if let Err(message) = this.load(gio::File::for_path(path)) {
                                error(&this.window, &message);
                            }
                        } else {
                            this.new_note();
                        }
                    }
                    Some(Err(message)) => error(&this.window, &message.to_string()),
                    None => (),
                }
            });
        });
        let this = self.clone();
        action(&self.window, "close", &["<Control>w"], move || {
            this.window.close()
        });
        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |_| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if let Some(source) = this.autosave.borrow_mut().take() {
                source.remove();
            }
            if !this.dirty.get() || (this.managed && this.save()) {
                return glib::Propagation::Proceed;
            }
            if this.closing.replace(true) {
                return glib::Propagation::Stop;
            }
            let dialog = gtk::AlertDialog::builder()
                .message("保存当前修改？")
                .detail("关闭前可以保存修改，或另存为一份文本文件。")
                .buttons(["取消", "放弃修改", "保存", "另存为"])
                .cancel_button(0)
                .default_button(2)
                .build();
            let weak = Rc::downgrade(&this);
            dialog.choose(Some(&this.window), gio::Cancellable::NONE, move |choice| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                this.closing.set(false);
                match choice {
                    Ok(1) => this.window.destroy(),
                    Ok(2) if this.save() => this.window.destroy(),
                    Ok(3) => this.save_as(true),
                    _ => (),
                }
            });
            glib::Propagation::Stop
        });
        // Release action closures when the window closes; they hold the state
        // alive while it is open, without leaking buffers across activations.
        let weak = Rc::downgrade(self);
        self.window.connect_destroy(move |window| {
            if let Some(this) = weak.upgrade()
                && let Some(source) = this.autosave.borrow_mut().take()
            {
                source.remove();
            }
            for name in window.list_actions() {
                window.remove_action(&name);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_bom_and_windows_line_endings() {
        assert_eq!(
            encode_text("备忘\n第二行\n", true, true),
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
