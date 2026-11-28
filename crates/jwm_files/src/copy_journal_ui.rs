//! Read-only, bounded copy-journal records for a Files window.
use gtk::{gio, glib, prelude::*};
use gtk4 as gtk;
use jwm_file_core::{
    COPY_JOURNAL_SLOTS, CopyJournalSlotState, CopyJournalSnapshot, CopyJournalState,
    inspect_copy_journal, open_files_state_root,
};
use std::{cell::Cell, io, path::PathBuf, rc::Rc};

pub(super) const TITLE: &str = "JWM Files · 复制记录";

fn load() -> io::Result<Option<CopyJournalSnapshot>> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let state_home = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    let root = match open_files_state_root(home.as_deref(), state_home.as_deref(), false) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    inspect_copy_journal(&root).map(Some)
}

fn populate(rows: &gtk::Box, status: &gtk::Label, snapshot: Option<CopyJournalSnapshot>) {
    while let Some(child) = rows.first_child() {
        rows.remove(&child);
    }
    let occupied = snapshot.as_ref().map_or(&[][..], |view| view.occupied());
    let suffix = if occupied.is_empty() {
        " · 尚无复制记录"
    } else {
        ""
    };
    let used = occupied.len();
    status.set_text(&format!("已占用 {used}/{COPY_JOURNAL_SLOTS}{suffix}"));
    for entry in occupied {
        let state = match &entry.state {
            CopyJournalSlotState::Incomplete => "记录未写完".into(),
            CopyJournalSlotState::Unreadable => "记录无法读取".into(),
            CopyJournalSlotState::Readable(record) => {
                let state = match record.state {
                    CopyJournalState::Prepared => "只有开始记录，复制结果未知",
                    CopyJournalState::Published(_) => "有写入记录，未检查当前文件",
                };
                let target = record.destination_parent.join(&record.name);
                format!(
                    "{state} · {}",
                    crate::trash_ui::display_path(target.as_os_str())
                )
            }
        };
        let line = format!("{:02} · {state}", entry.slot + 1);
        let row = gtk::Label::new(Some(&line));
        row.set_xalign(0.0);
        row.set_selectable(true);
        row.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        row.set_tooltip_text(Some(&line));
        rows.append(&row);
    }
}

pub(super) fn install(parent: &gtk::ApplicationWindow) {
    let action = gio::SimpleAction::new("copy-journal", None);
    let weak_parent = parent.downgrade();
    let popup = glib::WeakRef::<gtk::Window>::new();
    let busy = Rc::new(Cell::new(false));
    action.connect_activate(move |_, _| {
        let Some(parent) = weak_parent.upgrade() else {
            return;
        };
        if let Some(window) = popup.upgrade().filter(|window| window.is_visible()) {
            window.present();
            return;
        }
        let window = gtk::Window::builder()
            .title(TITLE)
            .default_width(710)
            .default_height(380)
            .transient_for(&parent)
            .destroy_with_parent(true)
            .build();
        window.set_application(parent.application().as_ref());
        let layout = gtk::Box::new(gtk::Orientation::Vertical, 10);
        layout.set_margin_top(12);
        layout.set_margin_bottom(12);
        layout.set_margin_start(12);
        layout.set_margin_end(12);
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_hexpand(true);
        let refresh = gtk::Button::with_label("刷新复制记录");
        toolbar.append(&status);
        toolbar.append(&refresh);
        layout.append(&toolbar);
        let note = gtk::Label::new(Some("只读 · 按记录编号排列，不检查当前目标文件"));
        note.set_xalign(0.0);
        note.add_css_class("dim-label");
        layout.append(&note);
        let rows = gtk::Box::new(gtk::Orientation::Vertical, 7);
        let scroll = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&rows)
            .build();
        layout.append(&scroll);
        window.set_child(Some(&layout));

        let (weak_window, weak_status, weak_rows, busy) = (
            window.downgrade(),
            status.downgrade(),
            rows.downgrade(),
            busy.clone(),
        );
        refresh.connect_clicked(move |refresh| {
            let Some(status) = weak_status.upgrade() else {
                return;
            };
            if busy.replace(true) {
                status.set_text("上一次读取仍在结束，请稍后刷新。");
                return;
            }
            status.set_text("正在读取复制记录…");
            let (send, receive) = futures_channel::oneshot::channel();
            let spawned = std::thread::Builder::new()
                .name("jwm-files-copy-records".into())
                .spawn(move || {
                    let _ = send.send(load());
                });
            if spawned.is_err() {
                busy.set(false);
                status.set_text("无法启动复制记录读取。");
                return;
            }
            refresh.set_sensitive(false);
            let (window, status, rows, refresh, busy) = (
                weak_window.clone(),
                weak_status.clone(),
                weak_rows.clone(),
                refresh.downgrade(),
                busy.clone(),
            );
            glib::spawn_future_local(async move {
                let result = receive.await;
                busy.set(false);
                if !window.upgrade().is_some_and(|window| window.is_visible()) {
                    return;
                }
                let Some(((status, rows), refresh)) =
                    status.upgrade().zip(rows.upgrade()).zip(refresh.upgrade())
                else {
                    return;
                };
                refresh.set_sensitive(true);
                match result {
                    Ok(Ok(view)) => populate(&rows, &status, view),
                    Ok(Err(_)) | Err(_) => {
                        while let Some(child) = rows.first_child() {
                            rows.remove(&child);
                        }
                        status.set_text("无法安全读取复制记录。");
                    }
                }
            });
        });
        popup.set(Some(&window));
        window.present();
        refresh.emit_clicked();
    });
    parent.add_action(&action);
}
