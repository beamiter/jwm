use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use jwm_file_core::{DirectoryHandle, MakeDirectoryError};
use jwm_file_core::{
    FileEntry, FileKind, ListOptions, LocalProvider, Location, ScanId, ScanWorker,
};
use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

const APP_ID: &str = "org.jwm.Files";
const ROW_BATCH: usize = 768;

struct Browser {
    worker: ScanWorker,
    path: PathBuf,
    pending: Option<(ScanId, PathBuf)>,
    show_hidden: bool,
    entries: Vec<FileEntry>,
    visible_count: usize,
    parent: Option<Arc<DirectoryHandle>>,
    mkdir: Option<FolderOperation>,
}

impl Browser {
    fn requested_path(&self) -> &Path {
        self.pending
            .as_ref()
            .map_or(self.path.as_path(), |(_, path)| path.as_path())
    }
}

type Shared = Rc<RefCell<Browser>>;

struct FolderOperation {
    parent: Arc<DirectoryHandle>,
    path: PathBuf,
    reply: mpsc::Receiver<Result<(), MakeDirectoryError>>,
}

fn show_new_folder(window: &gtk::ApplicationWindow, state: &Shared, status: &gtk::Label) {
    let (path, parent) = {
        let browser = state.borrow();
        if browser.mkdir.is_some() {
            status.set_text("正在创建文件夹，请稍候。");
            return;
        }
        let Some(parent) = browser.parent.clone() else {
            status.set_text("请先打开一个可读目录。");
            return;
        };
        (browser.path.clone(), parent)
    };
    let dialog = gtk::Dialog::builder()
        .title("JWM Files · New Folder")
        .transient_for(window)
        .modal(true)
        .build();
    dialog.add_button("取消", gtk::ResponseType::Cancel);
    dialog.add_button("创建", gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Accept);
    let content = dialog.content_area();
    content.set_margin_start(16);
    content.set_margin_end(16);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_spacing(8);
    let where_label = gtk::Label::new(Some(&format!("位置：{}", path.display())));
    where_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    where_label.set_xalign(0.0);
    content.append(&where_label);
    let name = gtk::Entry::builder()
        .placeholder_text("文件夹名称")
        .activates_default(true)
        .build();
    content.append(&name);
    let (state, status) = (state.clone(), status.clone());
    let name_for_response = name.clone();
    dialog.connect_response(move |dialog, decision| {
        let value = OsString::from(name_for_response.text().as_str());
        dialog.destroy();
        if decision != gtk::ResponseType::Accept {
            return;
        }
        let mut browser = state.borrow_mut();
        if browser.mkdir.is_some()
            || browser.path != path
            || browser
                .parent
                .as_ref()
                .is_none_or(|current| !Arc::ptr_eq(current, &parent))
        {
            status.set_text("目录已变化，没有创建文件夹。");
            return;
        }
        let (send, reply) = mpsc::sync_channel(1);
        let target = parent.clone();
        let original_path = path.clone();
        match thread::Builder::new()
            .name("jwm-file-mkdir".into())
            .spawn(move || {
                let result = target.create_directory_at(&original_path, &value);
                let _ = send.send(result);
            }) {
            Ok(_) => {
                browser.mkdir = Some(FolderOperation {
                    parent: parent.clone(),
                    path: path.clone(),
                    reply,
                });
                status.set_text("正在创建文件夹…");
            }
            Err(error) => status.set_text(&format!("无法启动创建任务：{error}")),
        }
    });
    dialog.present();
    name.grab_focus();
}

fn start_scan(
    state: &Shared,
    path: PathBuf,
    model: &gtk::StringList,
    address: &gtk::Entry,
    status: &gtk::Label,
) {
    {
        let mut browser = state.borrow_mut();
        let id = browser.worker.submit(
            Location::Local(path.clone()),
            ListOptions {
                show_hidden: browser.show_hidden,
            },
        );
        browser.pending = Some((id, path.clone()));
        browser.entries.clear();
        browser.visible_count = 0;
        browser.parent = None;
    }
    model.splice(0, model.n_items(), &[]);
    address.set_text(&path.to_string_lossy());
    status.set_text("正在读取目录…");
}

fn open_window(app: &gtk::Application, initial: &Path, home: &Path) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("JWM Files")
        .default_width(900)
        .default_height(620)
        .build();
    let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    for (key, value) in [("top", 10), ("bottom", 10), ("start", 12), ("end", 12)] {
        match key {
            "top" => header.set_margin_top(value),
            "bottom" => header.set_margin_bottom(value),
            "start" => header.set_margin_start(value),
            _ => header.set_margin_end(value),
        }
    }
    let up = gtk::Button::with_label("上一级");
    let refresh = gtk::Button::with_label("刷新");
    let new_folder = gtk::Button::with_label("新建文件夹");
    let address = gtk::Entry::builder()
        .hexpand(true)
        .placeholder_text("输入目录路径")
        .build();
    let hidden = gtk::ToggleButton::with_label("隐藏项");
    for widget in [
        up.upcast_ref::<gtk::Widget>(),
        refresh.upcast_ref(),
        new_folder.upcast_ref(),
        address.upcast_ref(),
        hidden.upcast_ref(),
    ] {
        header.append(widget);
    }
    layout.append(&header);

    let places = gtk::ListBox::new();
    places.add_css_class("navigation-sidebar");
    places.set_selection_mode(gtk::SelectionMode::None);
    let mut locations = vec![("主目录", home.to_path_buf())];
    locations.extend([
        ("文档", home.join("Documents")),
        ("下载", home.join("Downloads")),
        ("根目录", PathBuf::from("/")),
    ]);
    for (name, _) in &locations {
        let label = gtk::Label::new(Some(name));
        label.set_xalign(0.0);
        label.set_margin_start(12);
        label.set_margin_end(12);
        label.set_margin_top(8);
        label.set_margin_bottom(8);
        places.append(&label);
    }
    let model = gtk::StringList::new(&[]);
    let selection = gtk::SingleSelection::new(Some(model.clone()));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, object| {
        if let Ok(item) = object.clone().downcast::<gtk::ListItem>() {
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_margin_start(8);
            label.set_margin_end(8);
            label.set_margin_top(5);
            label.set_margin_bottom(5);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            item.set_child(Some(&label));
        }
    });
    factory.connect_bind(|_, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let name = item.item().and_downcast::<gtk::StringObject>();
        let label = item.child().and_downcast::<gtk::Label>();
        if let (Some(name), Some(label)) = (name, label) {
            label.set_text(&name.string());
        }
    });
    let list = gtk::ListView::new(Some(selection), Some(factory));
    list.set_single_click_activate(false);
    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&list)
        .build();
    let panes = gtk::Paned::new(gtk::Orientation::Horizontal);
    panes.set_start_child(Some(&places));
    panes.set_end_child(Some(&scroll));
    panes.set_position(180);
    panes.set_resize_start_child(false);
    layout.append(&panes);

    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_margin_start(12);
    status.set_margin_top(6);
    status.set_margin_bottom(6);
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    layout.append(&status);
    window.set_child(Some(&layout));
    let worker = match ScanWorker::start(LocalProvider) {
        Ok(worker) => worker,
        Err(e) => {
            status.set_text(&format!("无法启动目录读取：{e}"));
            window.present();
            return;
        }
    };
    let state = Rc::new(RefCell::new(Browser {
        worker,
        path: initial.into(),
        pending: None,
        show_hidden: false,
        entries: Vec::new(),
        visible_count: 0,
        parent: None,
        mkdir: None,
    }));
    {
        let (state, model, address, status) = (
            state.clone(),
            model.clone(),
            address.clone(),
            status.clone(),
        );
        places.connect_row_activated(move |_, row| {
            if let Some((_, path)) = locations.get(row.index() as usize) {
                start_scan(&state, path.clone(), &model, &address, &status);
            }
        });
    }
    {
        let (state, model, address, status) = (
            state.clone(),
            model.clone(),
            address.clone(),
            status.clone(),
        );
        up.connect_clicked(move |_| {
            let path = state
                .borrow()
                .requested_path()
                .parent()
                .map(Path::to_path_buf);
            if let Some(path) = path {
                start_scan(&state, path, &model, &address, &status);
            }
        });
    }
    {
        let (state, model, address, status) = (
            state.clone(),
            model.clone(),
            address.clone(),
            status.clone(),
        );
        refresh.connect_clicked(move |_| {
            let path = state.borrow().requested_path().to_path_buf();
            start_scan(&state, path, &model, &address, &status);
        });
    }
    {
        let (state, model, address, status) = (
            state.clone(),
            model.clone(),
            address.clone(),
            status.clone(),
        );
        hidden.connect_toggled(move |toggle| {
            let path = {
                let mut browser = state.borrow_mut();
                browser.show_hidden = toggle.is_active();
                browser.requested_path().to_path_buf()
            };
            start_scan(&state, path, &model, &address, &status);
        });
    }
    {
        let (state, model, status, home) = (
            state.clone(),
            model.clone(),
            status.clone(),
            home.to_path_buf(),
        );
        address.connect_activate(move |entry| {
            let input = entry.text();
            if input.is_empty() {
                return;
            }
            let current = state.borrow().requested_path().to_path_buf();
            let path = if input.as_str() == current.to_string_lossy() {
                current
            } else if input.as_str() == "~" {
                home.clone()
            } else if let Some(tail) = input.strip_prefix("~/") {
                home.join(tail)
            } else {
                current.join(Path::new(input.as_str()))
            };
            start_scan(&state, path, &model, entry, &status);
        });
    }
    {
        let (state, model, address, status) = (
            state.clone(),
            model.clone(),
            address.clone(),
            status.clone(),
        );
        list.connect_activate(move |_, index| {
            let row = state
                .borrow()
                .entries
                .get(index as usize)
                .map(|e| (e.kind, e.location.clone()));
            if let Some((kind, Location::Local(path))) = row {
                if kind == FileKind::Directory {
                    start_scan(&state, path, &model, &address, &status);
                } else {
                    status.set_text(&path.to_string_lossy());
                }
            }
        });
    }
    let action = gtk::gio::SimpleAction::new("location", None);
    let weak = address.downgrade();
    action.connect_activate(move |_, _| {
        if let Some(address) = weak.upgrade() {
            address.grab_focus();
            address.select_region(0, -1);
        }
    });
    window.add_action(&action);
    app.set_accels_for_action("win.location", &["<Control>l"]);

    let mkdir_action = gtk::gio::SimpleAction::new("new-folder", None);
    let weak_window = window.downgrade();
    let (mkdir_state, mkdir_status) = (state.clone(), status.clone());
    mkdir_action.connect_activate(move |_, _| {
        if let Some(window) = weak_window.upgrade() {
            show_new_folder(&window, &mkdir_state, &mkdir_status);
        }
    });
    window.add_action(&mkdir_action);
    new_folder.set_action_name(Some("win.new-folder"));
    app.set_accels_for_action("win.new-folder", &["<Control><Shift>n"]);

    start_scan(&state, initial.into(), &model, &address, &status);
    let weak_window = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(16), move || {
        let Some(window) = weak_window.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let mut browser = state.borrow_mut();
        if let Some(reply) = browser.worker.take_reply()
            && let Some((id, requested)) = browser.pending.take()
            && id == reply.id
        {
            match reply.result {
                Ok(snapshot) => {
                    browser.path = requested;
                    browser.entries = snapshot.entries;
                    browser.parent = snapshot.local_parent;
                    let base = format!("{} 项", browser.entries.len());
                    let label = if snapshot.skipped_entries == 0 {
                        base
                    } else {
                        format!("{base} · {} 项读取失败", snapshot.skipped_entries)
                    };
                    status.set_text(&label);
                    window.set_title(Some(&format!("JWM Files · {}", browser.path.display())));
                }
                Err(error) => status.set_text(&format!("无法读取目录：{error}")),
            }
        }
        let completed = browser
            .mkdir
            .as_ref()
            .and_then(|work| match work.reply.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(MakeDirectoryError::Io(
                    std::io::Error::other("folder worker ended without a result"),
                ))),
                Err(mpsc::TryRecvError::Empty) => None,
            });
        if let Some(result) = completed
            && let Some(work) = browser.mkdir.take()
            && browser.path == work.path
            && browser
                .parent
                .as_ref()
                .is_some_and(|current| current.identity() == work.parent.identity())
        {
            match result {
                Ok(()) => {
                    let path = browser.path.clone();
                    drop(browser);
                    start_scan(&state, path, &model, &address, &status);
                    return glib::ControlFlow::Continue;
                }
                Err(error) => status.set_text(&format!("没有创建文件夹：{error}")),
            }
        }
        if browser.visible_count < browser.entries.len() {
            let start = browser.visible_count;
            let limit = (start + ROW_BATCH).min(browser.entries.len());
            let labels: Vec<_> = browser.entries[start..limit]
                .iter()
                .map(|entry| {
                    let prefix = match entry.kind {
                        FileKind::Directory => "▸ ",
                        FileKind::Symlink => "↪ ",
                        _ => "",
                    };
                    format!("{prefix}{}", entry.name.to_string_lossy().escape_debug())
                })
                .collect();
            browser.visible_count = limit;
            drop(browser);
            let strings: Vec<_> = labels.iter().map(String::as_str).collect();
            model.splice(start as u32, 0, &strings);
        }
        glib::ControlFlow::Continue
    });
    window.present();
}

fn main() -> glib::ExitCode {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.clone());
    let initial = std::env::args_os()
        .nth(1)
        .map(|path| cwd.join(path))
        .unwrap_or(cwd);
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| open_window(app, &initial, &home));
    app.run_with_args(&["jwm-files"])
}
