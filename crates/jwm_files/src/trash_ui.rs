use futures_channel::oneshot;
use gtk::{gio, glib, prelude::*};
use gtk4 as gtk;
use jwm_file_core::{
    Cancellation, CopyError, FileKind, HomeTrashCursor, HomeTrashEntry, HomeTrashRestoreOutcome,
    HomeTrashRestorePlan, HomeTrashSnapshot, RecoverCopyError, recover_home_trash_copy,
};
use std::{
    cell::{Cell, RefCell},
    ffi::OsStr,
    fmt::Write as _,
    io::ErrorKind,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    thread,
};

const TITLE: &str = "JWM Files · Home Trash";
pub(super) const RESTORE_TITLE: &str = "JWM Files · Restore Original";
const RESTORE_LABEL: &str = "恢复原件";

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
    restore: glib::WeakRef<gtk::Button>,
    next: glib::WeakRef<gtk::Button>,
    cursor: RefCell<Option<HomeTrashCursor>>,
    page: Cell<usize>,
    status: glib::WeakRef<gtk::Label>,
    entries: RefCell<Vec<Arc<HomeTrashEntry>>>,
    confirming: Cell<bool>,
    needs_refresh: Cell<bool>,
    running: RefCell<Option<Cancellation>>,
    busy: Rc<Cell<bool>>,
    data_home: PathBuf,
}

impl View {
    fn selected(&self) -> Option<Arc<HomeTrashEntry>> {
        if self.needs_refresh.get() {
            return None;
        }
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
        if let Some(button) = self.restore.upgrade() {
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
        let cancel = dialog.add_button("取消", gtk::ResponseType::Cancel);
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
        let released = Rc::new(Cell::new(false));
        dialog.connect_destroy({
            let (view, released) = (self.clone(), released.clone());
            move |_| {
                if !released.replace(true) {
                    view.confirming.set(false);
                    view.update_controls();
                }
            }
        });
        dialog.connect_response({
            let (view, panel) = (self.clone(), panel.downgrade());
            move |dialog, response| {
                if released.replace(true) {
                    return;
                }
                view.confirming.set(false);
                dialog.destroy();
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
        cancel.grab_focus();
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

    fn prepare_restore(self: &Rc<Self>, panel: &gtk::Window) {
        if self.busy.get() || self.confirming.get() {
            return;
        }
        let (Some(entry), Some(status)) = (self.selected(), self.status.upgrade()) else {
            return;
        };
        let cancel = Cancellation::default();
        let worker_cancel = cancel.clone();
        let data_home = self.data_home.clone();
        let worker_entry = entry.clone();
        let (send, receive) = oneshot::channel();
        if let Err(error) = thread::Builder::new()
            .name("jwm-trash-prepare-restore".into())
            .spawn(move || {
                let _ = send.send(HomeTrashRestorePlan::prepare(
                    &data_home,
                    &worker_entry,
                    &worker_cancel,
                ));
            })
        {
            status.set_text(&format!("无法核对恢复位置：{error}"));
            return;
        }
        self.busy.set(true);
        self.running.replace(Some(cancel));
        self.update_controls();
        status.set_text("正在核对回收站原件和原位置…");
        let (view, panel) = (self.clone(), panel.downgrade());
        glib::MainContext::default().spawn_local(async move {
            let result = receive.await;
            view.busy.set(false);
            let active = view
                .running
                .take()
                .is_some_and(|cancel| cancel.check().is_ok());
            view.update_controls();
            let Some(panel) = panel.upgrade().filter(|panel| panel.is_visible()) else {
                return;
            };
            let Some(status) = view.status.upgrade() else {
                return;
            };
            if !active {
                return;
            }
            if !view.selected().is_some_and(|row| Arc::ptr_eq(&row, &entry)) {
                status.set_text("选择已变化，未恢复。");
                return;
            }
            match result {
                Ok(Ok(plan)) => view.confirm_restore(&panel, entry, plan),
                Ok(Err(error)) => status.set_text(&restore_refusal(&error)),
                Err(_) => status.set_text("未能核对原件，未发起恢复。"),
            }
        });
    }

    fn confirm_restore(
        self: &Rc<Self>,
        panel: &gtk::Window,
        entry: Arc<HomeTrashEntry>,
        plan: HomeTrashRestorePlan,
    ) {
        let slot = display_path(plan.storage_key());
        let original = display_path(plan.original_path().as_os_str());
        let dialog = gtk::Dialog::builder()
            .title(RESTORE_TITLE)
            .transient_for(panel)
            .modal(true)
            .destroy_with_parent(true)
            .build();
        let cancel = dialog.add_button("取消", gtk::ResponseType::Cancel);
        dialog.add_button(RESTORE_LABEL, gtk::ResponseType::Accept);
        dialog.set_default_response(gtk::ResponseType::Cancel);
        let label = gtk::Label::new(Some(&format!(
            "项目：{slot}\n原位置：{original}\n原件将移出回收站，不会覆盖现有文件。"
        )));
        label.set_margin_start(12);
        label.set_margin_end(12);
        label.set_margin_top(12);
        label.set_margin_bottom(12);
        label.set_wrap(true);
        label.set_selectable(true);
        dialog.content_area().append(&label);
        let plan = Rc::new(RefCell::new(Some(plan)));
        self.confirming.set(true);
        self.update_controls();
        dialog.connect_destroy({
            let (view, plan) = (self.clone(), plan.clone());
            move |_| {
                if plan.take().is_some() {
                    view.confirming.set(false);
                    view.update_controls();
                }
            }
        });
        dialog.connect_response({
            let (view, panel) = (self.clone(), panel.downgrade());
            move |dialog, response| {
                let Some(plan) = plan.take() else {
                    return;
                };
                view.confirming.set(false);
                dialog.destroy();
                if response == gtk::ResponseType::Accept
                    && panel.upgrade().is_some_and(|panel| panel.is_visible())
                    && view.selected().is_some_and(|row| Arc::ptr_eq(&row, &entry))
                {
                    view.restore_original(plan);
                } else if let Some(status) = view.status.upgrade() {
                    status.set_text("未恢复，回收站原件未变。");
                }
                view.update_controls();
            }
        });
        dialog.present();
        cancel.grab_focus();
    }

    fn restore_original(self: &Rc<Self>, plan: HomeTrashRestorePlan) {
        let Some(status) = self.status.upgrade() else {
            return;
        };
        if self.busy.get() {
            status.set_text("另一个操作正在进行，未恢复。");
            return;
        }
        let cancel = Cancellation::default();
        let worker_cancel = cancel.clone();
        let (send, receive) = oneshot::channel();
        if let Err(error) = thread::Builder::new()
            .name("jwm-trash-restore-original".into())
            .spawn(move || {
                let _ = send.send(plan.restore(&worker_cancel));
            })
        {
            status.set_text(&format!("未能启动恢复原件：{error}"));
            return;
        }
        self.busy.set(true);
        self.running.replace(Some(cancel));
        self.update_controls();
        status.set_text("正在恢复原件…");
        let view = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let result = receive.await;
            let _ = view.running.take();
            view.busy.set(false);
            // The old listing cannot authorize a second action after a
            // completed or uncertain native rename. Preserve the receipt.
            if !matches!(&result, Ok(Err(_))) {
                view.needs_refresh.set(true);
                let _ = view.cursor.take();
            }
            view.update_controls();
            let Some(status) = view.status.upgrade() else {
                return;
            };
            let receipt = match result {
                Ok(Ok(HomeTrashRestoreOutcome::Restored)) => {
                    "已恢复原件到原位置。刷新可查看剩余项目。".into()
                }
                Ok(Ok(HomeTrashRestoreOutcome::RestoredWithMetadataRemnant)) => {
                    "原件已回到原位置，但回收站记录可能保留。请检查，不要重复恢复。".into()
                }
                Ok(Ok(HomeTrashRestoreOutcome::RenamedUnverified)) => {
                    "文件已移出，但无法确认原位置中的文件。请检查，不要重复恢复。".into()
                }
                Ok(Err(error)) => restore_refusal(&error),
                Err(_) => "恢复结果不明，请检查原位置和回收站，不要重复恢复。".into(),
            };
            status.set_text(&receipt);
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
                    view.needs_refresh.set(false);
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

fn restore_refusal(error: &std::io::Error) -> String {
    match error.kind() {
        ErrorKind::AlreadyExists => "原位置已有同名项目，未覆盖。".into(),
        ErrorKind::NotFound => "原目录或回收站记录已不存在，未恢复。请检查原位置并刷新。".into(),
        ErrorKind::InvalidData => "回收站记录不能唯一确定原位置，未恢复原件。".into(),
        ErrorKind::CrossesDevices => "原位置与回收站不在同一挂载点，未恢复原件。".into(),
        ErrorKind::Interrupted => "已取消，未恢复。".into(),
        _ => format!("未能恢复原件：{error}"),
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
        let restore = gtk::Button::with_label(RESTORE_LABEL);
        restore.set_tooltip_text(Some("把回收站原件移回原位置，不覆盖现有文件"));
        restore.set_sensitive(false);
        toolbar.append(&restore);
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
            restore: restore.downgrade(),
            next: next.downgrade(),
            cursor: RefCell::new(None),
            page: Cell::new(0),
            status: status.downgrade(),
            entries: RefCell::new(Vec::new()),
            confirming: Cell::new(false),
            needs_refresh: Cell::new(false),
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
        restore.connect_clicked({
            let (view, panel) = (view.clone(), window.downgrade());
            move |_| {
                if let Some(panel) = panel.upgrade() {
                    view.prepare_restore(&panel);
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
