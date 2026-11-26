use futures_channel::oneshot;
use gtk::{gio, glib, prelude::*};
use gtk4 as gtk;
use jwm_file_core::{Cancellation, FileKind, HomeTrashSnapshot, list_home_trash};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    thread,
};

const TITLE: &str = "JWM Files · Home Trash";

fn present_rows(snapshot: HomeTrashSnapshot) -> (Vec<String>, String) {
    let missing = snapshot
        .entries
        .iter()
        .filter(|row| row.info.is_none())
        .count();
    let mut status = format!("主目录回收站：{} 项（只读）", snapshot.entries.len());
    if missing > 0 {
        status.push_str(&format!(" · {missing} 项原路径未知"));
    }
    if snapshot.unreadable_entries > 0 {
        status.push_str(&format!(" · {} 项读取失败", snapshot.unreadable_entries));
    }
    if snapshot.incomplete {
        status.push_str(" · 已达到读取上限，列表不完整");
    }
    let rows = snapshot
        .entries
        .into_iter()
        .map(|row| {
            let storage = row.name.to_string_lossy().escape_debug().to_string();
            let kind = match row.kind {
                FileKind::Directory => "文件夹",
                FileKind::Regular => "文件",
                FileKind::Symlink => "符号链接",
                FileKind::Other => "其他",
            };
            if let Some(info) = row.info {
                let original = info.original.to_string_lossy().escape_debug().to_string();
                format!("{} · {original} · {kind} · {storage}", info.deletion_date)
            } else {
                format!("原路径未知 · {kind} · {storage}")
            }
        })
        .collect();
    (rows, status)
}

struct View {
    rows: glib::WeakRef<gtk::StringList>,
    status: glib::WeakRef<gtk::Label>,
    running: RefCell<Option<Cancellation>>,
    busy: Rc<Cell<bool>>,
    data_home: PathBuf,
}

impl View {
    fn refresh(self: &Rc<Self>) {
        let Some(status) = self.status.upgrade() else {
            return;
        };
        if self.busy.get() {
            if self.running.borrow().is_none() {
                status.set_text("上一项读取正在结束，请稍后刷新。");
            }
            return;
        }
        let data_home = self.data_home.clone();
        let cancel = Cancellation::default();
        let worker_cancel = cancel.clone();
        let (send, receive) = oneshot::channel();
        if let Err(error) = thread::Builder::new()
            .name("jwm-files-home-trash".into())
            .spawn(move || {
                let _ = send.send(list_home_trash(&data_home, &worker_cancel).map(present_rows));
            })
        {
            status.set_text(&format!("无法启动回收站读取：{error}"));
            return;
        }
        self.busy.set(true);
        self.running.replace(Some(cancel));
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
                Ok(Ok((entries, summary))) => {
                    let refs: Vec<_> = entries.iter().map(String::as_str).collect();
                    rows.splice(0, rows.n_items(), &refs);
                    status.set_text(&summary);
                }
                Ok(Err(error)) => status.set_text(&format!("无法读取主目录回收站：{error}")),
                Err(_) => status.set_text("回收站读取未返回结果。"),
            }
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
        let refresh = gtk::Button::with_label("刷新");
        toolbar.append(&refresh);
        layout.append(&toolbar);

        let rows = gtk::StringList::new(&[]);
        let model = gtk::NoSelection::new(Some(rows.clone()));
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
        let list = gtk::ListView::new(Some(model), Some(factory));
        let scroll = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&list)
            .build();
        layout.append(&scroll);
        window.set_child(Some(&layout));
        let view = Rc::new(View {
            rows: rows.downgrade(),
            status: status.downgrade(),
            running: RefCell::new(None),
            busy: busy.clone(),
            data_home: glib::user_data_dir(),
        });
        refresh.connect_clicked({
            let view = view.clone();
            move |_| view.refresh()
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
        view.refresh();
    });
    parent.add_action(&action);
}
