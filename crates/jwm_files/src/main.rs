mod navigation;

use gtk::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use jwm_file_core::{DirectoryHandle, MakeDirectoryError};
use jwm_file_core::{
    FileEntry, FileKind, ListOptions, LocalProvider, Location, ScanId, ScanWorker,
};
use navigation::{Destination, Navigation};
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
    pending: Option<(ScanId, Destination)>,
    navigation: Navigation,
    show_hidden: bool,
    entries: Vec<FileEntry>,
    visible_count: usize,
    parent: Option<Arc<DirectoryHandle>>,
    mkdir: Option<FolderOperation>,
    activation: Option<Activation>,
}

impl Browser {
    fn requested_destination(&self) -> Destination {
        self.pending.as_ref().map_or_else(
            || Destination::Visit(self.path.clone()),
            |(_, destination)| destination.clone(),
        )
    }

    fn requested_path(&self) -> &Path {
        self.pending
            .as_ref()
            .map_or(self.path.as_path(), |(_, target)| target.path())
    }
}

type Shared = Rc<RefCell<Browser>>;

struct Activation {
    path: PathBuf,
    cancel: gio::Cancellable,
}

fn current_activation(state: &Shared, cancel: &gio::Cancellable) -> bool {
    state
        .borrow()
        .activation
        .as_ref()
        .is_some_and(|active| &active.cancel == cancel && !cancel.is_cancelled())
}

fn finish_activation(state: &Shared, cancel: &gio::Cancellable) -> bool {
    if !current_activation(state, cancel) {
        return false;
    }
    state.borrow_mut().activation = None;
    true
}

fn safe_document(path: &Path, content_type: Option<&str>, can_execute: bool) -> bool {
    let launcher_name = path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("desktop"));
    !launcher_name
        && content_type.is_some_and(|kind| {
            !(kind == "application/x-desktop"
                || gio::content_type_is_unknown(kind)
                || gio::content_type_is_a(kind, "application/x-executable")
                || can_execute && gio::content_type_can_be_executable(kind))
        })
}

fn activate_entry(
    state: &Shared,
    path: PathBuf,
    listed_kind: FileKind,
    address: &gtk::Entry,
    status: &gtk::Label,
) {
    let cancel = {
        let mut browser = state.borrow_mut();
        if browser.pending.is_some()
            || browser
                .activation
                .as_ref()
                .is_some_and(|active| active.path == path)
        {
            return;
        }
        if let Some(previous) = browser.activation.take() {
            previous.cancel.cancel();
        }
        let cancel = gio::Cancellable::new();
        browser.activation = Some(Activation {
            path: path.clone(),
            cancel: cancel.clone(),
        });
        cancel
    };

    status.set_text("正在检查所选项目…");
    let file = gio::File::for_path(&path);
    let (state, address, status) = (state.clone(), address.clone(), status.clone());
    let callback_cancel = cancel.clone();
    let uri = file.uri();
    file.query_info_async(
        "standard::type,standard::content-type,access::can-execute",
        gio::FileQueryInfoFlags::NONE,
        glib::Priority::DEFAULT,
        Some(&cancel),
        move |result| {
            let cancel = callback_cancel;
            if !current_activation(&state, &cancel) {
                return;
            }
            let info = match result {
                Ok(info) => info,
                Err(error) => {
                    finish_activation(&state, &cancel);
                    status.set_text(&format!("无法打开所选项目：{error}"));
                    return;
                }
            };
            match info.file_type() {
                gio::FileType::Directory
                    if matches!(listed_kind, FileKind::Directory | FileKind::Symlink) =>
                {
                    finish_activation(&state, &cancel);
                    start_scan(&state, path, &address, &status);
                }
                gio::FileType::Regular
                    if matches!(listed_kind, FileKind::Regular | FileKind::Symlink)
                        && safe_document(
                            &path,
                            info.content_type().as_deref(),
                            info.boolean("access::can-execute"),
                        ) =>
                {
                    let done_token = cancel.clone();
                    gio::AppInfo::launch_default_for_uri_async(
                        &uri,
                        None::<&gio::AppLaunchContext>,
                        Some(&cancel),
                        move |result| {
                            if finish_activation(&state, &done_token) {
                                match result {
                                    Ok(()) => status.set_text("已交给系统应用打开"),
                                    Err(error) => {
                                        status.set_text(&format!("无法打开文件：{error}"))
                                    }
                                }
                            }
                        },
                    );
                }
                _ => {
                    finish_activation(&state, &cancel);
                    status.set_text("该项目不是可打开的文档或目录。");
                }
            }
        },
    );
}

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
        let Some(parent) = browser.parent.clone().filter(|_| browser.pending.is_none()) else {
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
            || browser.pending.is_some()
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

fn start_scan(state: &Shared, path: PathBuf, address: &gtk::Entry, status: &gtk::Label) {
    request_scan(state, Destination::Visit(path), address, status);
}

fn request_scan(state: &Shared, target: Destination, address: &gtk::Entry, status: &gtk::Label) {
    let path = target.path().to_path_buf();
    {
        let mut browser = state.borrow_mut();
        let id = browser.worker.submit(
            Location::Local(path.clone()),
            ListOptions {
                show_hidden: browser.show_hidden,
            },
        );
        if let Some(activation) = browser.activation.take() {
            activation.cancel.cancel();
        }
        browser.pending = Some((id, target));
    }
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
    let back = gtk::Button::with_label("后退");
    let forward = gtk::Button::with_label("前进");
    let up = gtk::Button::with_label("上一级");
    let refresh = gtk::Button::with_label("刷新");
    let new_folder = gtk::Button::with_label("新建文件夹");
    let address = gtk::Entry::builder()
        .hexpand(true)
        .placeholder_text("输入目录路径")
        .build();
    let hidden = gtk::ToggleButton::with_label("隐藏项");
    for widget in [
        back.upcast_ref::<gtk::Widget>(),
        forward.upcast_ref(),
        up.upcast_ref(),
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
        navigation: Navigation::default(),
        show_hidden: false,
        entries: Vec::new(),
        visible_count: 0,
        parent: None,
        mkdir: None,
        activation: None,
    }));
    {
        let (state, address, status) = (state.clone(), address.clone(), status.clone());
        places.connect_row_activated(move |_, row| {
            if let Some((_, path)) = locations.get(row.index() as usize) {
                start_scan(&state, path.clone(), &address, &status);
            }
        });
    }
    {
        let (state, address, status) = (state.clone(), address.clone(), status.clone());
        up.connect_clicked(move |_| {
            let path = state
                .borrow()
                .requested_path()
                .parent()
                .map(Path::to_path_buf);
            if let Some(path) = path {
                start_scan(&state, path, &address, &status);
            }
        });
    }
    {
        let (state, address, status) = (state.clone(), address.clone(), status.clone());
        refresh.connect_clicked(move |_| {
            let target = state.borrow().requested_destination();
            request_scan(&state, target, &address, &status);
        });
    }
    {
        let (state, address, status) = (state.clone(), address.clone(), status.clone());
        hidden.connect_toggled(move |toggle| {
            let path = {
                let mut browser = state.borrow_mut();
                browser.show_hidden = toggle.is_active();
                browser.requested_destination()
            };
            request_scan(&state, path, &address, &status);
        });
    }
    {
        let (state, status, home) = (state.clone(), status.clone(), home.to_path_buf());
        address.connect_activate(move |entry| {
            let input = entry.text();
            if input.is_empty() {
                return;
            }
            let target = state.borrow().requested_destination();
            if input.as_str() == target.path().to_string_lossy() {
                request_scan(&state, target, entry, &status);
                return;
            }
            let path = if input.as_str() == "~" {
                home.clone()
            } else if let Some(tail) = input.strip_prefix("~/") {
                home.join(tail)
            } else {
                target.path().join(Path::new(input.as_str()))
            };
            start_scan(&state, path, entry, &status);
        });
    }
    {
        let (state, address, status) = (state.clone(), address.clone(), status.clone());
        list.connect_activate(move |_, index| {
            let row = {
                let browser = state.borrow();
                if browser.pending.is_some() {
                    return;
                }
                browser
                    .entries
                    .get(index as usize)
                    .map(|e| (e.kind, e.location.clone()))
            };
            if let Some((kind, Location::Local(path))) = row {
                activate_entry(&state, path, kind, &address, &status);
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

    for (is_back, button, name, accel) in [
        (true, &back, "back", "<Alt>Left"),
        (false, &forward, "forward", "<Alt>Right"),
    ] {
        let action = gtk::gio::SimpleAction::new(name, None);
        let (state, address, status) = (state.clone(), address.clone(), status.clone());
        action.connect_activate(move |_, _| {
            let target = {
                let browser = state.borrow();
                if is_back {
                    browser.navigation.back()
                } else {
                    browser.navigation.forward()
                }
            };
            if let Some(target) = target {
                request_scan(&state, target, &address, &status);
            }
        });
        window.add_action(&action);
        let detailed = format!("win.{name}");
        button.set_action_name(Some(&detailed));
        app.set_accels_for_action(&detailed, &[accel]);
    }

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

    {
        let state = state.clone();
        window.connect_close_request(move |_| {
            if let Some(active) = state.borrow_mut().activation.take() {
                active.cancel.cancel();
            }
            glib::Propagation::Proceed
        });
    }
    start_scan(&state, initial.into(), &address, &status);
    let weak_window = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(16), move || {
        let Some(window) = weak_window.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let mut browser = state.borrow_mut();
        let mut replaced = false;
        if let Some(reply) = browser.worker.take_reply()
            && let Some((id, requested)) = browser.pending.take()
            && id == reply.id
        {
            match reply.result {
                Ok(snapshot) => {
                    let path = requested.path().to_path_buf();
                    if !browser.navigation.accept(requested) {
                        return glib::ControlFlow::Continue;
                    }
                    browser.path = path;
                    browser.entries = snapshot.entries;
                    browser.visible_count = 0;
                    browser.parent = snapshot.local_parent;
                    replaced = true;
                    let base = format!("{} 项", browser.entries.len());
                    let label = if snapshot.skipped_entries == 0 {
                        base
                    } else {
                        format!("{base} · {} 项读取失败", snapshot.skipped_entries)
                    };
                    status.set_text(&label);
                    window.set_title(Some(&format!("JWM Files · {}", browser.path.display())));
                }
                Err(error) => {
                    address.set_text(&browser.path.to_string_lossy());
                    status.set_text(&format!("无法读取目录：{error}"));
                }
            }
        }
        if replaced {
            drop(browser);
            model.splice(0, model.n_items(), &[]);
            return glib::ControlFlow::Continue;
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
            && browser.pending.is_none()
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
                    start_scan(&state, path, &address, &status);
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

#[cfg(test)]
mod activation_tests {
    use super::*;

    #[test]
    fn permits_ordinary_text_but_not_launchers_or_executables() {
        assert!(safe_document(
            Path::new("report.txt"),
            Some("text/plain"),
            false
        ));
        assert!(!safe_document(
            Path::new("report.txt"),
            Some("text/plain"),
            true
        ));
        assert!(!safe_document(
            Path::new("test.DESKTOP"),
            Some("text/plain"),
            false
        ));
        assert!(!safe_document(
            Path::new("alias"),
            Some("application/x-desktop"),
            false
        ));
        assert!(!safe_document(
            Path::new("program"),
            Some("application/x-executable"),
            false
        ));
        assert!(!safe_document(Path::new("unknown"), None, false));
    }

    #[test]
    fn gio_uri_round_trips_exact_native_path_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let original = PathBuf::from(OsString::from_vec(b"/tmp/test space%\xff.txt".to_vec()));
        let uri = gio::File::for_path(&original).uri();
        assert!(uri.contains("%20") && uri.contains("%25") && uri.contains("%FF"));
        assert_eq!(
            gio::File::for_uri(&uri).path().as_deref(),
            Some(original.as_path())
        );
    }
}
