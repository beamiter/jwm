use futures_channel::oneshot;
use gtk::{gio, glib, prelude::*};
use gtk4 as gtk;
use jwm_file_core::{
    Cancellation, CopyError, FileKind, HomeTrashCursor, HomeTrashEntry, HomeTrashSnapshot,
    RecoverCopyError, recover_home_trash_copy,
};
use std::{
    cell::{Cell, RefCell},
    ffi::OsStr,
    fmt::Write as _,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    thread,
};

const TITLE: &str = "JWM Files · Home Trash";

/// Keep ordinary Unicode readable, but distinguish raw Unix bytes from a
/// literal replacement character or a filename containing escape notation.
pub(super) fn display_path(path: &OsStr) -> String {
    let mut display = String::new();
    for chunk in path.as_bytes().utf8_chunks() {
        for ch in chunk.valid().chars() {
            display.extend(ch.escape_debug());
        }
        for byte in chunk.invalid() {
            let _ = write!(display, "\\x{byte:02X}");
        }
    }
    display
}

struct Rows {
    labels: Vec<String>,
    entries: Vec<Arc<HomeTrashEntry>>,
    status: String,
}

fn present_rows(snapshot: HomeTrashSnapshot, page: usize) -> Rows {
    let missing = snapshot
        .entries
        .iter()
        .filter(|row| row.info.is_none())
        .count();
    let mut status = format!(
        "主目录回收站 · 第 {page} 批 · {} 项",
        snapshot.entries.len()
    );
    if page > 1 || snapshot.incomplete {
        status.push_str(" · 仅按本批删除时间排序");
    }
    if missing > 0 {
        status.push_str(&format!(" · {missing} 项原路径未知"));
    }
    if snapshot.unreadable_entries > 0 {
        status.push_str(&format!(" · {} 项读取失败", snapshot.unreadable_entries));
    }
    if snapshot.incomplete {
        status.push_str(" · 还有未读取的项目");
    }
    let labels = snapshot
        .entries
        .iter()
        .map(|row| {
            let storage = display_path(&row.name);
            let kind = match row.kind {
                FileKind::Directory => "文件夹",
                FileKind::Regular => "文件",
                FileKind::Symlink => "符号链接",
                FileKind::Other => "其他",
            };
            if let Some(info) = &row.info {
                let original = display_path(info.original.as_os_str());
                format!("{} · {original} · {kind} · {storage}", info.deletion_date)
            } else {
                format!("原路径未知 · {kind} · {storage}")
            }
        })
        .collect();
    Rows {
        labels,
        entries: snapshot.entries.into_iter().map(Arc::new).collect(),
        status,
    }
}

struct View {
    rows: glib::WeakRef<gtk::StringList>,
    selection: glib::WeakRef<gtk::SingleSelection>,
    recover: glib::WeakRef<gtk::Button>,
    next: glib::WeakRef<gtk::Button>,
    cursor: RefCell<Option<HomeTrashCursor>>,
    page: Cell<usize>,
    status: glib::WeakRef<gtk::Label>,
    entries: RefCell<Vec<Arc<HomeTrashEntry>>>,
    confirming: Cell<bool>,
    running: RefCell<Option<Cancellation>>,
    busy: Rc<Cell<bool>>,
    data_home: PathBuf,
}

impl View {
    fn selected(&self) -> Option<Arc<HomeTrashEntry>> {
        let current = self.selection.upgrade()?.selected() as usize;
        self.entries
            .borrow()
            .get(current)
            .filter(|entry| entry.kind == FileKind::Regular && entry.info.is_some())
            .cloned()
    }

    fn update_controls(&self) {
        let available = !self.busy.get() && !self.confirming.get();
        if let Some(button) = self.recover.upgrade() {
            button.set_sensitive(available && self.selected().is_some());
        }
        if let Some(button) = self.next.upgrade() {
            button.set_sensitive(available && self.cursor.borrow().is_some());
        }
    }

    fn confirm_recover(self: &Rc<Self>, panel: &gtk::Window) {
        if self.busy.get() || self.confirming.get() {
            return;
        }
        let Some(entry) = self.selected() else {
            return;
        };
        let Some(info) = &entry.info else {
            return;
        };
        let original = display_path(info.original.as_os_str());
        let slot = display_path(&entry.name);
        self.confirming.set(true);
        self.update_controls();
        let dialog = gtk::Dialog::builder()
            .title("JWM Files · Recover Copy")
            .transient_for(panel)
            .modal(true)
            .destroy_with_parent(true)
            .build();
        dialog.add_button("取消", gtk::ResponseType::Cancel);
        dialog.add_button("恢复副本", gtk::ResponseType::Accept);
        dialog.set_default_response(gtk::ResponseType::Cancel);
        let label = gtk::Label::new(Some(&format!(
            "项目：{slot}\n恢复位置：{original}\n保留回收站原件，且不会覆盖现有文件。"
        )));
        label.set_margin_start(12);
        label.set_margin_end(12);
        label.set_margin_top(12);
        label.set_margin_bottom(12);
        label.set_wrap(true);
        label.set_selectable(true);
        dialog.content_area().append(&label);
        dialog.connect_destroy({
            let view = self.clone();
            move |_| {
                view.confirming.set(false);
                view.update_controls();
            }
        });
        dialog.connect_response({
            let (view, panel) = (self.clone(), panel.downgrade());
            move |dialog, response| {
                dialog.destroy();
                view.confirming.set(false);
                if response == gtk::ResponseType::Accept
                    && panel.upgrade().is_some_and(|panel| panel.is_visible())
                    && view
                        .selected()
                        .is_some_and(|current| Arc::ptr_eq(&entry, &current))
                {
                    view.recover_copy(entry.clone());
                }
                view.update_controls();
            }
        });
        dialog.present();
    }

    fn recover_copy(self: &Rc<Self>, entry: Arc<HomeTrashEntry>) {
        if self.busy.get() {
            return;
        }
        let Some(status) = self.status.upgrade() else {
            return;
        };
        let data_home = self.data_home.clone();
        let cancel = Cancellation::default();
        let worker_cancel = cancel.clone();
        let (send, receive) = oneshot::channel();
        if let Err(error) = thread::Builder::new()
            .name("jwm-files-trash-recover".into())
            .spawn(move || {
                let _ = send.send(recover_home_trash_copy(&data_home, &entry, &worker_cancel));
            })
        {
            status.set_text(&format!("无法启动恢复副本：{error}"));
            return;
        }
        self.busy.set(true);
        self.running.replace(Some(cancel));
        self.update_controls();
        status.set_text("正在恢复副本…");
        let view = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let result = receive.await;
            view.busy.set(false);
            if view
                .running
                .take()
                .is_none_or(|cancel| cancel.check().is_err())
            {
                return;
            }
            view.update_controls();
            let Some(status) = view.status.upgrade() else {
                return;
            };
            match result {
                Ok(Ok(_)) => status.set_text("已恢复副本，回收站原件保留。"),
                Ok(Err(RecoverCopyError::Copy(CopyError::DestinationExists))) => {
                    status.set_text("原位置已有同名项目，未覆盖。回收站原件保留。")
                }
                Ok(Err(error)) => status.set_text(&format!("未能恢复副本：{error}")),
                Err(_) => status.set_text("恢复副本未返回结果；请检查目标位置。"),
            }
        });
    }

    fn read_page(self: &Rc<Self>, next: bool) {
        let Some(status) = self.status.upgrade() else {
            return;
        };
        if self.confirming.get() {
            return;
        }
        if self.busy.get() {
            if self.running.borrow().is_none() {
                status.set_text("上一项读取正在结束，请稍后刷新。");
            }
            return;
        }
        let data_home = self.data_home.clone();
        let previous = self.cursor.take();
        if next && previous.is_none() {
            return;
        }
        let page = if next {
            self.page.get().saturating_add(1)
        } else {
            1
        };
        let cancel = Cancellation::default();
        let worker_cancel = cancel.clone();
        let (send, receive) = oneshot::channel();
        if let Err(error) = thread::Builder::new()
            .name("jwm-files-home-trash".into())
            .spawn(move || {
                let result = (|| {
                    let mut cursor = if next {
                        previous
                    } else {
                        HomeTrashCursor::open(&data_home, &worker_cancel)?
                    };
                    let snapshot = cursor.as_mut().map_or_else(
                        || Ok(HomeTrashSnapshot::default()),
                        |scan| scan.next_page(&worker_cancel),
                    )?;
                    if !snapshot.incomplete {
                        cursor = None;
                    }
                    Ok::<_, std::io::Error>((present_rows(snapshot, page), cursor))
                })();
                let _ = send.send(result);
            })
        {
            status.set_text(&format!("无法启动回收站读取：{error}"));
            self.update_controls();
            return;
        }
        self.busy.set(true);
        self.running.replace(Some(cancel));
        self.update_controls();
        status.set_text("正在读取主目录回收站…");
        let view = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let result = receive.await;
            view.busy.set(false);
            let active = view
                .running
                .take()
                .is_some_and(|cancel| cancel.check().is_ok());
            if !active {
                return;
            }
            let (Some(rows), Some(status)) = (view.rows.upgrade(), view.status.upgrade()) else {
                return;
            };
            match result {
                Ok(Ok((result, cursor))) => {
                    view.cursor.replace(cursor);
                    view.page.set(page);
                    if let Some(selection) = view.selection.upgrade() {
                        selection.set_selected(gtk::INVALID_LIST_POSITION);
                    }
                    view.entries.replace(result.entries);
                    let refs: Vec<_> = result.labels.iter().map(String::as_str).collect();
                    rows.splice(0, rows.n_items(), &refs);
                    status.set_text(&result.status);
                }
                Ok(Err(error)) => status.set_text(&format!("无法继续读取，请刷新：{error}")),
                Err(_) => status.set_text("回收站读取未返回结果。"),
            }
            view.update_controls();
        });
    }
}

pub(super) fn install(parent: &gtk::ApplicationWindow) {
    let action = gio::SimpleAction::new("home-trash", None);
    let weak = parent.downgrade();
    let popup = glib::WeakRef::<gtk::Window>::new();
    let busy = Rc::new(Cell::new(false));
    action.connect_activate(move |_, _| {
        let Some(parent) = weak.upgrade() else {
            return;
        };
        if let Some(existing) = popup.upgrade().filter(|w| w.is_visible()) {
            existing.present();
            return;
        }
        let window = gtk::Window::builder()
            .title(TITLE)
            .default_width(760)
            .default_height(440)
            .transient_for(&parent)
            .destroy_with_parent(true)
            .build();
        window.set_application(parent.application().as_ref());
        let layout = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        toolbar.set_margin_start(12);
        toolbar.set_margin_end(12);
        toolbar.set_margin_top(8);
        let status = gtk::Label::new(None);
        status.set_wrap(true);
        status.set_xalign(0.0);
        status.set_hexpand(true);
        toolbar.append(&status);
        let recover = gtk::Button::with_label("恢复副本");
        recover.set_sensitive(false);
        toolbar.append(&recover);
        let next = gtk::Button::with_label("下一批");
        next.set_sensitive(false);
        toolbar.append(&next);
        let refresh = gtk::Button::with_label("刷新");
        refresh.set_tooltip_text(Some("从第一批重新读取"));
        toolbar.append(&refresh);
        layout.append(&toolbar);

        let rows = gtk::StringList::new(&[]);
        let model = gtk::SingleSelection::new(Some(rows.clone()));
        model.set_autoselect(false);
        model.set_can_unselect(true);
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, object| {
            let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_margin_start(12);
            label.set_margin_end(12);
            label.set_margin_top(5);
            label.set_margin_bottom(5);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            item.set_child(Some(&label));
        });
        factory.connect_bind(|_, object| {
            let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            if let (Some(row), Some(label)) = (
                item.item().and_downcast::<gtk::StringObject>(),
                item.child().and_downcast::<gtk::Label>(),
            ) {
                let line = row.string();
                label.set_text(&line);
                label.set_tooltip_text(Some(&line));
            }
        });
        let list = gtk::ListView::new(Some(model.clone()), Some(factory));
        let scroll = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&list)
            .build();
        layout.append(&scroll);
        window.set_child(Some(&layout));
        let view = Rc::new(View {
            rows: rows.downgrade(),
            selection: model.downgrade(),
            recover: recover.downgrade(),
            next: next.downgrade(),
            cursor: RefCell::new(None),
            page: Cell::new(0),
            status: status.downgrade(),
            entries: RefCell::new(Vec::new()),
            confirming: Cell::new(false),
            running: RefCell::new(None),
            busy: busy.clone(),
            data_home: glib::user_data_dir(),
        });
        model.connect_selected_notify({
            let view = view.clone();
            move |_| view.update_controls()
        });
        recover.connect_clicked({
            let (view, panel) = (view.clone(), window.downgrade());
            move |_| {
                if let Some(panel) = panel.upgrade() {
                    view.confirm_recover(&panel);
                }
            }
        });
        refresh.connect_clicked({
            let view = view.clone();
            move |_| view.read_page(false)
        });
        next.connect_clicked({
            let view = view.clone();
            move |_| view.read_page(true)
        });
        window.connect_close_request({
            let view = view.clone();
            move |_| {
                if let Some(cancel) = view.running.borrow().as_ref() {
                    cancel.cancel();
                }
                glib::Propagation::Proceed
            }
        });
        window.connect_destroy({
            let view = view.clone();
            move |_| {
                if let Some(cancel) = view.running.borrow().as_ref() {
                    cancel.cancel();
                }
            }
        });
        popup.set(Some(&window));
        window.present();
        view.read_page(false);
    });
    parent.add_action(&action);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trash_display_distinguishes_raw_bytes_replacement_and_literal_escape() {
        let show = |bytes| display_path(OsStr::from_bytes(bytes));
        assert_eq!(show(b"test\x80"), r"test\x80");
        assert_eq!(show(b"test\x81"), r"test\x81");
        assert_eq!(show("test�".as_bytes()), "test�");
        assert_eq!(show(br"test\x80"), r"test\\x80");
        assert_eq!(show("测试\n\u{202E}txt".as_bytes()), r"测试\n\u{202e}txt");
        assert_eq!(show(b"\xf0\x9fASCII\xff"), r"\xF0\x9FASCII\xFF");
    }
}
