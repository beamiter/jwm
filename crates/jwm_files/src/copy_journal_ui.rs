//! Bounded copy-journal records and explicit retired cleanup for a Files window.

mod retired_action;
use gtk::{gio, glib, prelude::*};
use gtk4 as gtk;
use jwm_file_core::{
    COPY_JOURNAL_SLOTS, CopyJournalRecord, CopyJournalSlotState, CopyJournalSnapshot,
    CopyJournalState, CopyReview, DirectoryHandle, ManagedJournalSnapshot, ManagedRetiredState,
    inspect_copy_journal, inspect_managed_journal, open_files_state_root,
};
use std::{cell::Cell, io, path::PathBuf, rc::Rc};

pub(super) const TITLE: &str = "JWM Files · 复制记录";

struct JournalViews {
    legacy: io::Result<Option<CopyJournalSnapshot>>,
    managed: io::Result<Option<ManagedJournalSnapshot>>,
}

fn state_root() -> io::Result<DirectoryHandle> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let state_home = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    open_files_state_root(home.as_deref(), state_home.as_deref(), false)
}

fn load() -> io::Result<JournalViews> {
    let root = match state_root() {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(JournalViews {
                legacy: Ok(None),
                managed: Ok(None),
            });
        }
        Err(error) => return Err(error),
    };
    Ok(JournalViews {
        legacy: inspect_copy_journal(&root).map(Some),
        managed: inspect_managed_journal(&root).map(Some),
    })
}

fn review_text(result: CopyReview) -> &'static str {
    match result {
        CopyReview::InvalidRecord => "记录不完整，无法核对。",
        CopyReview::ParentUnavailable => "当前无法打开记录中的目标目录。",
        CopyReview::ParentChanged => "记录中的目标目录已变化。",
        CopyReview::DestinationMissing => "当前目标不存在，不能据此确定是否曾复制。",
        CopyReview::DestinationUnavailable => "当前无法读取目标的属性。",
        CopyReview::DestinationNotRegular => "当前目标不是普通文件。",
        CopyReview::PresentWithoutPublicationRecord => "目标目前存在，但没有记录能确认是本次复制。",
        CopyReview::DestinationChanged => "当前目标与已保存的属性不同。",
        CopyReview::MetadataMatches => "当前目标属性与记录一致，不代表文件内容已重新验证。",
    }
}

fn append_check(
    container: &gtk::Box,
    row: &gtk::Box,
    window: &gtk::Window,
    record: CopyJournalRecord,
    reviewing: &Rc<Cell<bool>>,
) {
    let button = gtk::Button::with_label("检查当前目标");
    let details = gtk::Label::new(None);
    details.set_xalign(0.0);
    details.set_wrap(true);
    details.set_visible(false);
    container.append(&details);
    let (weak_window, details, reviewing) =
        (window.downgrade(), details.downgrade(), reviewing.clone());
    button.connect_clicked(move |button| {
        let Some(label) = details.upgrade() else {
            return;
        };
        label.set_visible(true);
        if reviewing.replace(true) {
            label.set_text("另一条记录正在检查，请稍后再试。");
            return;
        }
        label.set_text("正在检查当前目标…");
        let (send, receive) = futures_channel::oneshot::channel();
        let record = record.clone();
        let launched = std::thread::Builder::new()
            .name("jwm-files-journal-check".into())
            .spawn(move || {
                #[cfg(test)]
                check_test_gate::before(&record);
                let _ = send.send(record.review());
            });
        if launched.is_err() {
            reviewing.set(false);
            label.set_text("无法启动目标检查。");
            return;
        }
        button.set_sensitive(false);
        let (window, details, button, reviewing) = (
            weak_window.clone(),
            details.clone(),
            button.downgrade(),
            reviewing.clone(),
        );
        glib::spawn_future_local(async move {
            let result = receive.await;
            reviewing.set(false);
            let Some(window) = window.upgrade().filter(|window| window.is_visible()) else {
                return;
            };
            let Some((label, button)) = details.upgrade().zip(button.upgrade()) else {
                return;
            };
            if !label.root().is_some_and(|root| {
                root.downcast::<gtk::Window>()
                    .is_ok_and(|root| root == window)
            }) {
                return;
            }
            button.set_sensitive(true);
            label.set_text(result.map_or("未收到目标检查结果。", review_text));
        });
    });
    row.append(&button);
}

fn append_group(
    rows: &gtk::Box,
    window: &gtk::Window,
    title: &str,
    snapshot: io::Result<Option<CopyJournalSnapshot>>,
    reviewing: &Rc<Cell<bool>>,
) {
    let header = gtk::Label::new(None);
    header.set_xalign(0.0);
    header.add_css_class("heading");
    rows.append(&header);
    let Ok(snapshot) = snapshot else {
        header.set_text(&format!("{title} · 暂时无法安全读取"));
        return;
    };
    let occupied = snapshot.as_ref().map_or(&[][..], |view| view.occupied());
    let suffix = if occupied.is_empty() {
        " · 尚无复制记录"
    } else {
        ""
    };
    let used = occupied.len();
    header.set_text(&format!(
        "{title} · 已占用 {used}/{COPY_JOURNAL_SLOTS}{suffix}"
    ));
    for entry in occupied {
        let state = match &entry.state {
            CopyJournalSlotState::Incomplete => "记录未写完".into(),
            CopyJournalSlotState::Unreadable => "记录无法读取".into(),
            CopyJournalSlotState::Readable(record) => {
                let state = match record.state {
                    CopyJournalState::Prepared => "只有开始记录，复制结果未知",
                    CopyJournalState::Published(_) => "有保存的写入记录",
                };
                let target = record.destination_parent.join(&record.name);
                format!(
                    "{state} · {}",
                    crate::trash_ui::display_path(target.as_os_str())
                )
            }
        };
        let line = format!("{:02} · {state}", entry.slot + 1);
        let container = gtk::Box::new(gtk::Orientation::Vertical, 5);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let label = gtk::Label::new(Some(&line));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_selectable(true);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        label.set_tooltip_text(Some(&line));
        row.append(&label);
        container.append(&row);
        if let CopyJournalSlotState::Readable(record) = &entry.state {
            append_check(&container, &row, window, record.clone(), reviewing);
        }
        rows.append(&container);
    }
}

fn populate(
    rows: &gtk::Box,
    status: &gtk::Label,
    window: &gtk::Window,
    clean: &gtk::Button,
    view: JournalViews,
    reviewing: &Rc<Cell<bool>>,
) {
    while let Some(child) = rows.first_child() {
        rows.remove(&child);
    }
    let retired = view.managed.as_ref().map(|snapshot| {
        snapshot
            .as_ref()
            .map_or(ManagedRetiredState::Absent, |s| s.retired)
    });
    clean.set_visible(matches!(retired, Ok(ManagedRetiredState::PendingCleanup)));
    let summary = match retired {
        Ok(ManagedRetiredState::Absent) => "没有待清理的托管记录。",
        Ok(ManagedRetiredState::PendingCleanup) => "有已移出的托管记录待清理，不占用复制记录编号。",
        Ok(ManagedRetiredState::Unsafe) => "待清理的托管记录无法安全读取。",
        Err(_) => "暂时无法确认是否有托管记录待清理。",
    };
    status.set_text(summary);
    let managed = view
        .managed
        .map(|snapshot| snapshot.map(|view| view.numbered));
    append_group(rows, window, "托管记录", managed, reviewing);
    append_group(rows, window, "旧版记录（只读）", view.legacy, reviewing);
}

pub(super) fn install(parent: &gtk::ApplicationWindow) {
    let action = gio::SimpleAction::new("copy-journal", None);
    let weak_parent = parent.downgrade();
    let popup = glib::WeakRef::<gtk::Window>::new();
    let busy = Rc::new(Cell::new(false));
    let reviewing = Rc::new(Cell::new(false));
    let cleaning = Rc::new(Cell::new(false));
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
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let clean = gtk::Button::with_label(retired_action::BUTTON);
        clean.set_visible(false);
        let feedback = gtk::Label::new(None);
        feedback.set_xalign(0.0);
        feedback.set_wrap(true);
        feedback.set_hexpand(true);
        actions.append(&clean);
        actions.append(&feedback);
        layout.append(&actions);
        retired_action::install(&window, &clean, &feedback, &refresh, &cleaning);
        let note = gtk::Label::new(Some("按记录编号排列，不自动检查当前目标文件或清理日志"));
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
        let reviewing = reviewing.clone();
        let weak_clean = clean.downgrade();
        refresh.connect_clicked(move |refresh| {
            let Some(status) = weak_status.upgrade() else {
                return;
            };
            if busy.replace(true) {
                status.set_text("上一次读取仍在结束，请稍后刷新。");
                return;
            }
            status.set_text("正在读取复制记录…");
            if let Some(clean) = weak_clean.upgrade() {
                clean.set_visible(false);
            }
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
            let (reviewing, clean) = (reviewing.clone(), weak_clean.clone());
            glib::spawn_future_local(async move {
                let result = receive.await;
                busy.set(false);
                let Some(window) = window.upgrade().filter(|window| window.is_visible()) else {
                    return;
                };
                let Some(((status, rows), refresh)) =
                    status.upgrade().zip(rows.upgrade()).zip(refresh.upgrade())
                else {
                    return;
                };
                refresh.set_sensitive(true);
                let Some(clean) = clean.upgrade() else {
                    return;
                };
                match result {
                    Ok(Ok(view)) => populate(&rows, &status, &window, &clean, view, &reviewing),
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

#[cfg(test)]
pub(super) mod check_test_gate {
    use super::CopyJournalRecord;
    use std::{
        ffi::OsString,
        path::PathBuf,
        sync::{Mutex, mpsc},
        time::Duration,
    };

    struct Gate {
        parent: PathBuf,
        name: OsString,
        reached: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    static GATE: Mutex<Option<Gate>> = Mutex::new(None);

    pub fn arm(parent: PathBuf, name: OsString) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (reached, observed) = mpsc::channel();
        let (resume, release) = mpsc::channel();
        let old = GATE.lock().unwrap().replace(Gate {
            parent,
            name,
            reached,
            release,
        });
        assert!(old.is_none(), "only one held destination check");
        (observed, resume)
    }

    pub(super) fn before(record: &CopyJournalRecord) {
        let gate = {
            let mut gate = GATE.lock().unwrap();
            if gate.as_ref().is_some_and(|armed| {
                armed.parent == record.destination_parent && armed.name == record.name
            }) {
                gate.take()
            } else {
                None
            }
        };
        if let Some(gate) = gate {
            let _ = gate.reached.send(());
            let _ = gate.release.recv_timeout(Duration::from_secs(8));
        }
    }
}
