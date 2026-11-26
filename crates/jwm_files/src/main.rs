#[cfg(all(test, target_os = "linux"))]
mod bench_ui;
mod breadcrumb;
mod copy_ui;
mod grid;
mod navigation;
mod preview_ui;
mod row_model;
mod selection;
mod tab_strip;
mod tabs;
mod trash_ui;
#[cfg(target_os = "linux")]
mod tree;
mod wake;

use gtk::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use jwm_file_core::{DirectoryChange, DirectoryHandle, FileIdentity, RenameError, RenameReceipt};
use jwm_file_core::{
    FileEntry, FileKind, ListOptions, LocalProvider, Location, ScanId, ScanWorker,
};
use navigation::Destination;
use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;
use tabs::{TabId, Tabs};
use wake::{SignalOnDrop, Wake};

const APP_ID: &str = "org.jwm.Files";

struct Browser {
    worker: ScanWorker,
    wake: Wake,
    tabs: Tabs,
    pending: Option<(ScanId, Destination)>,
    show_hidden: bool,
    entries: Rc<Vec<FileEntry>>,
    parent: Option<Arc<DirectoryHandle>>,
    mutation: Option<MutationWork>,
    rename_undo: Option<Arc<RenameReceipt>>,
    copy_item: Option<copy_ui::StashedCopy>,
    activation: Option<Activation>,
    active_scan: Option<ScanId>,
    automatic_scan: Option<ScanId>,
    reload_due: Option<Rc<Cell<bool>>>,
    dialog_open: bool,
}

impl Browser {
    fn path(&self) -> &PathBuf {
        &self.tabs.active().path
    }

    fn requested_destination(&self) -> Destination {
        self.pending.as_ref().map_or_else(
            || Destination::Visit(self.path().clone()),
            |(_, destination)| destination.clone(),
        )
    }

    fn requested_path(&self) -> &Path {
        self.pending
            .as_ref()
            .map_or(self.path().as_path(), |(_, target)| target.path())
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
    let mut browser = state.borrow_mut();
    browser.activation = None;
    browser.wake.signal();
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
            || browser.parent.is_none()
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

struct MutationWork {
    parent: Arc<DirectoryHandle>,
    path: PathBuf,
    operation: &'static str,
    reply: mpsc::Receiver<Result<MutationOutcome, String>>,
    copy: Option<(jwm_file_core::Cancellation, String)>,
}

enum MutationOutcome {
    FileOperation,
    Rename(Option<RenameReceipt>),
    RenameUndone,
}

#[derive(Clone)]
enum Mutation {
    NewFolder,
    Rename {
        source: OsString,
        identity: FileIdentity,
    },
}

impl Mutation {
    fn operation(&self) -> &'static str {
        match self {
            Self::NewFolder => "创建文件夹",
            Self::Rename { .. } => "重命名",
        }
    }

    fn title(&self) -> &'static str {
        match self {
            Self::NewFolder => "JWM Files · New Folder",
            Self::Rename { .. } => "JWM Files · Rename",
        }
    }

    fn apply(
        &self,
        parent: &DirectoryHandle,
        path: &Path,
        name: &std::ffi::OsStr,
    ) -> Result<MutationOutcome, String> {
        match self {
            Self::NewFolder => parent
                .create_directory_at(path, name)
                .map(|_| MutationOutcome::FileOperation)
                .map_err(|e| e.to_string()),
            Self::Rename { source, identity } => parent
                .rename_child_recorded(path, source, *identity, name)
                .map(MutationOutcome::Rename)
                .map_err(|e| e.to_string()),
        }
    }
}

fn start_undo_rename(state: &Shared, status: &gtk::Label) {
    let mut browser = state.borrow_mut();
    if browser.mutation.is_some() || browser.dialog_open {
        status.set_text("有文件操作正在进行，请稍候。");
        return;
    }
    let Some(receipt) = browser.rename_undo.clone() else {
        status.set_text("没有可撤销的重命名。");
        return;
    };
    let Some(parent) = browser.parent.clone().filter(|_| browser.pending.is_none()) else {
        status.set_text("请等目录加载完成再撤销。");
        return;
    };
    if parent.identity() != receipt.directory_identity() {
        status.set_text("请回到执行重命名的目录再撤销。");
        return;
    }

    let path = browser.path().clone();
    let (target, target_path) = (parent.clone(), path.clone());
    let wake = browser.wake.clone();
    let (send, reply) = mpsc::sync_channel(1);
    let operation = "撤销重命名";
    match thread::Builder::new()
        .name("jwm-file-undo-rename".into())
        .spawn(move || {
            let _notify = SignalOnDrop(wake);
            let send = send;
            let result = receipt
                .undo(&target, &target_path)
                .map(|_| MutationOutcome::RenameUndone)
                .map_err(|err| match err {
                    RenameError::Io(ref inner)
                        if inner.kind() == std::io::ErrorKind::AlreadyExists =>
                    {
                        "原名称已被占用，未覆盖任何文件。".to_string()
                    }
                    RenameError::ParentChanged | RenameError::SourceChanged => {
                        "原目录或项目已变化，未执行。".to_string()
                    }
                    other => other.to_string(),
                });
            let _ = send.send(result);
        }) {
        Ok(_) => {
            browser.mutation = Some(MutationWork {
                parent,
                path,
                operation,
                reply,
                copy: None,
            });
            status.set_text("正在撤销重命名…");
        }
        Err(error) => status.set_text(&format!("无法启动{operation}：{error}")),
    }
}

fn show_name_dialog(
    window: &gtk::ApplicationWindow,
    state: &Shared,
    status: &gtk::Label,
    mutation: Mutation,
) {
    let (path, parent) = {
        let mut browser = state.borrow_mut();
        if browser.mutation.is_some() || browser.dialog_open {
            status.set_text("正在处理文件，请稍候。");
            return;
        }
        let Some(parent) = browser.parent.clone().filter(|_| browser.pending.is_none()) else {
            status.set_text("请先打开一个可读目录。");
            return;
        };
        if let Some(activation) = browser.activation.take() {
            activation.cancel.cancel();
        }
        browser.dialog_open = true;
        (browser.path().clone(), parent)
    };
    let dialog = gtk::Dialog::builder()
        .title(mutation.title())
        .transient_for(window)
        .modal(true)
        .build();
    dialog.add_button("取消", gtk::ResponseType::Cancel);
    dialog.add_button(mutation.operation(), gtk::ResponseType::Accept);
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
        .placeholder_text("输入名称")
        .activates_default(true)
        .build();
    if let Mutation::Rename { source, .. } = &mutation {
        if let Some(initial) = source.to_str() {
            name.set_text(initial);
        } else {
            let original = gtk::Label::new(Some(&format!(
                "原名：{}",
                source.to_string_lossy().escape_debug()
            )));
            original.set_ellipsize(gtk::pango::EllipsizeMode::End);
            original.set_xalign(0.0);
            content.append(&original);
        }
    }
    content.append(&name);
    let (state, status) = (state.clone(), status.clone());
    let name_for_response = name.clone();
    dialog.connect_response(move |dialog, decision| {
        let value = OsString::from(name_for_response.text().as_str());
        dialog.destroy();
        {
            let mut browser = state.borrow_mut();
            browser.dialog_open = false;
            browser.wake.signal();
        }
        if decision != gtk::ResponseType::Accept {
            return;
        }
        let mut browser = state.borrow_mut();
        if browser.mutation.is_some()
            || browser.pending.is_some()
            || browser.path() != &path
            || browser
                .parent
                .as_ref()
                .is_none_or(|current| !Arc::ptr_eq(current, &parent))
        {
            status.set_text("目录已变化，未执行文件操作。");
            return;
        }
        let (send, reply) = mpsc::sync_channel(1);
        let target = parent.clone();
        let original_path = path.clone();
        let action = mutation.clone();
        let operation = mutation.operation();
        let wake = browser.wake.clone();
        match thread::Builder::new()
            .name("jwm-file-mutation".into())
            .spawn(move || {
                let _notify = SignalOnDrop(wake);
                let send = send;
                let _ = send.send(action.apply(&target, &original_path, &value));
            }) {
            Ok(_) => {
                browser.mutation = Some(MutationWork {
                    parent: parent.clone(),
                    path: path.clone(),
                    operation,
                    reply,
                    copy: None,
                });
                status.set_text(&format!("正在{operation}…"));
            }
            Err(error) => status.set_text(&format!("无法启动{operation}：{error}")),
        }
    });
    dialog.present();
    name.grab_focus();
    name.select_region(0, -1);
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
        browser.automatic_scan = None;
    }
    address.set_text(&path.to_string_lossy());
    status.set_text("正在读取目录…");
}

fn place_locations(
    home: &Path,
    special: &[(glib::UserDirectory, Option<PathBuf>)],
) -> Vec<(&'static str, PathBuf)> {
    let mut locations = vec![("主目录", home.to_path_buf())];
    for (kind, path) in special {
        let label = match kind {
            glib::UserDirectory::Documents => "文档",
            glib::UserDirectory::Downloads => "下载",
            _ => continue,
        };
        if let Some(path) = path
            .as_ref()
            .filter(|path| path.is_absolute() && path != &home)
            && path != Path::new("/")
            && !locations.iter().any(|(_, existing)| existing == path)
        {
            locations.push((label, path.clone()));
        }
    }
    if home != Path::new("/") {
        locations.push(("根目录", PathBuf::from("/")));
    }
    locations
}

fn focus_current_view(views: &glib::WeakRef<gtk::Stack>) {
    if let Some(content) = views
        .upgrade()
        .and_then(|views| views.visible_child())
        .and_downcast::<gtk::ScrolledWindow>()
        .and_then(|scroll| scroll.child())
    {
        content.grab_focus();
    }
}

fn open_window(
    app: &gtk::Application,
    initial: &Path,
    home: &Path,
    special: &[(glib::UserDirectory, Option<PathBuf>)],
) {
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
    let location_bar = gtk::Stack::builder().hexpand(true).build();
    location_bar.set_hhomogeneous(false);
    location_bar.set_vhomogeneous(true);
    location_bar.add_named(&address, Some("entry"));
    let hidden = gtk::ToggleButton::with_label("隐藏项");
    let file_actions = gtk::MenuButton::builder().label("操作").build();
    let view_toggle = gtk::ToggleButton::with_label("图标");
    view_toggle.set_tooltip_text(Some("图标（Ctrl+2）／列表（Ctrl+1）"));
    for widget in [
        back.upcast_ref::<gtk::Widget>(),
        forward.upcast_ref(),
        up.upcast_ref(),
        refresh.upcast_ref(),
        new_folder.upcast_ref(),
        location_bar.upcast_ref(),
        hidden.upcast_ref(),
        view_toggle.upcast_ref(),
        file_actions.upcast_ref(),
    ] {
        header.append(widget);
    }
    layout.append(&header);
    let tabs = Rc::new(tab_strip::TabStrip::new());
    layout.append(&tabs.container);

    let places = gtk::ListBox::new();
    places.add_css_class("navigation-sidebar");
    places.set_selection_mode(gtk::SelectionMode::None);
    let locations = place_locations(home, special);
    for (name, _) in &locations {
        let label = gtk::Label::new(Some(name));
        label.set_xalign(0.0);
        label.set_margin_start(12);
        label.set_margin_end(12);
        label.set_margin_top(8);
        label.set_margin_bottom(8);
        places.append(&label);
    }
    let model = row_model::FileRowModel::new();
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
        let row = item.item().and_downcast::<glib::BoxedAnyObject>();
        let label = item.child().and_downcast::<gtk::Label>();
        if let (Some(row), Some(label)) = (row, label) {
            label.set_text(&row.borrow::<row_model::RowPresentation>().list_label());
        }
    });
    let list = gtk::ListView::new(Some(selection.clone()), Some(factory));
    let grid = grid::new(&selection);
    for view in [list.upcast_ref::<gtk::Widget>(), grid.upcast_ref()] {
        let enter = gtk::EventControllerKey::new();
        enter.set_propagation_phase(gtk::PropagationPhase::Capture);
        let selection = selection.clone();
        let weak_view = view.downgrade();
        enter.connect_key_pressed(move |_, key, _, modifiers| {
            if key == gtk::gdk::Key::space
                && !modifiers.intersects(
                    gtk::gdk::ModifierType::CONTROL_MASK
                        | gtk::gdk::ModifierType::ALT_MASK
                        | gtk::gdk::ModifierType::SUPER_MASK
                        | gtk::gdk::ModifierType::SHIFT_MASK,
                )
                && let Some(view) = weak_view.upgrade()
                && gtk::prelude::WidgetExt::activate_action(&view, "win.quick-preview", None)
                    .is_ok()
            {
                return glib::Propagation::Stop;
            }
            if !matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter)
                || modifiers.intersects(
                    gtk::gdk::ModifierType::CONTROL_MASK
                        | gtk::gdk::ModifierType::ALT_MASK
                        | gtk::gdk::ModifierType::SUPER_MASK,
                )
            {
                return glib::Propagation::Proceed;
            }
            if let Some(view) = weak_view.upgrade()
                && selection.selected() != gtk::INVALID_LIST_POSITION
            {
                view.emit_by_name::<()>("activate", &[&selection.selected()]);
            }
            glib::Propagation::Stop
        });
        view.add_controller(enter);
    }

    list.set_single_click_activate(false);
    let views = gtk::Stack::builder().hexpand(true).vexpand(true).build();
    for (name, content) in [
        ("list", list.upcast_ref::<gtk::Widget>()),
        ("grid", grid.upcast_ref()),
    ] {
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .child(content)
            .build();
        views.add_named(&scroll, Some(name));
    }
    views.set_visible_child_name("list");
    {
        let (views, list, grid) = (views.downgrade(), list.downgrade(), grid.downgrade());
        view_toggle.connect_toggled(move |toggle| {
            if let (Some(views), Some(list), Some(grid)) =
                (views.upgrade(), list.upgrade(), grid.upgrade())
            {
                views.set_visible_child_name(if toggle.is_active() { "grid" } else { "list" });
                if toggle.is_active() {
                    grid.grab_focus();
                } else {
                    list.grab_focus();
                }
            }
        });
    }
    for (name, active, key) in [
        ("show-list", false, "<Control>1"),
        ("show-grid", true, "<Control>2"),
    ] {
        let action = gio::SimpleAction::new(name, None);
        let toggle = view_toggle.downgrade();
        action.connect_activate(move |_, _| {
            if let Some(toggle) = toggle.upgrade() {
                toggle.set_active(active);
            }
        });
        window.add_action(&action);
        app.set_accels_for_action(&format!("win.{name}"), &[key]);
    }
    let panes = gtk::Paned::new(gtk::Orientation::Horizontal);
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 4);
    sidebar.append(&places);
    panes.set_start_child(Some(&sidebar));
    panes.set_end_child(Some(&views));
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
    let copy_status = gtk::Label::new(None);
    copy_status.set_xalign(0.0);
    copy_status.set_margin_start(12);
    copy_status.set_margin_bottom(6);
    copy_status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    copy_status.set_visible(false);
    layout.append(&copy_status);
    window.set_child(Some(&layout));
    let (wake, mut notifications) = Wake::channel();
    let scan_wake = wake.clone();
    let worker = match ScanWorker::start_notifying(LocalProvider, move || scan_wake.signal()) {
        Ok(worker) => worker,
        Err(e) => {
            status.set_text(&format!("无法启动目录读取：{e}"));
            window.present();
            return;
        }
    };
    let state = Rc::new(RefCell::new(Browser {
        worker,
        wake,
        tabs: Tabs::new(initial.into()),
        pending: None,
        show_hidden: false,
        entries: Rc::default(),
        parent: None,
        mutation: None,
        rename_undo: None,
        copy_item: None,
        activation: None,
        active_scan: None,
        automatic_scan: None,
        reload_due: None,
        dialog_open: false,
    }));
    preview_ui::install(&window, &state, &selection, &status);
    trash_ui::install(&window);
    copy_ui::install(&window, &file_actions, &state, &selection, &copy_status);
    let breadcrumb = Rc::new(breadcrumb::Breadcrumb::new({
        let (state, address, status) = (state.clone(), address.clone(), status.clone());
        move |path| start_scan(&state, path, &address, &status)
    }));
    location_bar.add_named(&breadcrumb.container, Some("path"));
    location_bar.set_visible_child_name("path");
    #[cfg(target_os = "linux")]
    {
        let tree_hidden = Rc::new(Cell::new(hidden.is_active()));
        let (tree_state, tree_address, tree_status) =
            (state.clone(), address.clone(), status.clone());
        match tree::TreeSidebar::new(tree_hidden.clone(), move |path| {
            start_scan(&tree_state, path, &tree_address, &tree_status);
        }) {
            Ok(tree) => {
                tree.set_root(initial.to_path_buf());
                let root_here = gtk::Button::with_label("以当前目录为树根");
                root_here.set_tooltip_text(Some("将已打开的目录设为侧栏目录树的根"));
                sidebar.append(&root_here);
                sidebar.append(&tree.view);
                let tree = Rc::new(tree);
                let root_path = Rc::new(RefCell::new(initial.to_path_buf()));
                {
                    let (tree, root_path, state) = (tree.clone(), root_path.clone(), state.clone());
                    root_here.connect_clicked(move |_| {
                        let path = state.borrow().path().clone();
                        *root_path.borrow_mut() = path.clone();
                        tree.set_root(path);
                    });
                }
                hidden.connect_toggled(move |toggle| {
                    tree_hidden.set(toggle.is_active());
                    tree.set_root(root_path.borrow().clone());
                });
            }
            Err(err) => {
                let unavailable = gtk::Label::new(Some("目录树不可用"));
                unavailable.set_tooltip_text(Some(&err.to_string()));
                sidebar.append(&unavailable);
            }
        }
    }
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
        let (weak_bar, weak_views) = (location_bar.downgrade(), views.downgrade());
        address.connect_activate(move |entry| {
            let input = entry.text();
            if input.is_empty() {
                return;
            }
            let target = state.borrow().requested_destination();
            if input.as_str() == target.path().to_string_lossy() {
                request_scan(&state, target, entry, &status);
            } else {
                let path = if input.as_str() == "~" {
                    home.clone()
                } else if let Some(tail) = input.strip_prefix("~/") {
                    home.join(tail)
                } else {
                    target.path().join(Path::new(input.as_str()))
                };
                start_scan(&state, path, entry, &status);
            }
            if let Some(bar) = weak_bar.upgrade() {
                bar.set_visible_child_name("path");
            }
            focus_current_view(&weak_views);
        });
    }
    let address_keys = gtk::EventControllerKey::new();
    address_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let state = state.clone();
        let (bar, entry, views) = (
            location_bar.downgrade(),
            address.downgrade(),
            views.downgrade(),
        );
        address_keys.connect_key_pressed(move |_, key, _, _| {
            if key != gtk::gdk::Key::Escape {
                return glib::Propagation::Proceed;
            }
            if let Some(entry) = entry.upgrade() {
                entry.set_text(&state.borrow().requested_path().to_string_lossy());
            }
            if let Some(bar) = bar.upgrade() {
                bar.set_visible_child_name("path");
            }
            focus_current_view(&views);
            glib::Propagation::Stop
        });
    }
    address.add_controller(address_keys);
    {
        let (state, address, status) = (state.clone(), address.clone(), status.clone());
        let activate = move |index| {
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
        };
        let list_activation = activate.clone();
        list.connect_activate(move |_, row| list_activation(row));
        grid.connect_activate(move |_, row| activate(row));
    }
    let action = gtk::gio::SimpleAction::new("location", None);
    let (weak, weak_bar) = (address.downgrade(), location_bar.downgrade());
    action.connect_activate(move |_, _| {
        if let (Some(address), Some(bar)) = (weak.upgrade(), weak_bar.upgrade()) {
            bar.set_visible_child_name("entry");
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
                    browser.tabs.active().navigation.back()
                } else {
                    browser.tabs.active().navigation.forward()
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
            show_name_dialog(&window, &mkdir_state, &mkdir_status, Mutation::NewFolder);
        }
    });
    window.add_action(&mkdir_action);
    new_folder.set_action_name(Some("win.new-folder"));
    app.set_accels_for_action("win.new-folder", &["<Control><Shift>n"]);
    let rename = gtk::gio::SimpleAction::new("rename", None);
    let weak_window = window.downgrade();
    let (rename_state, rename_status) = (state.clone(), status.clone());
    let rename_selection = selection.clone();
    rename.connect_activate(move |_, _| {
        let Some(window) = weak_window.upgrade() else {
            return;
        };
        let target = {
            let browser = rename_state.borrow();
            if browser.pending.is_some() {
                return;
            }
            browser
                .entries
                .get(rename_selection.selected() as usize)
                .and_then(|entry| Some((entry.name.clone(), entry.identity?)))
        };
        if let Some((source, identity)) = target {
            show_name_dialog(
                &window,
                &rename_state,
                &rename_status,
                Mutation::Rename { source, identity },
            );
        } else {
            rename_status.set_text("请先选择一个项目。");
        }
    });
    window.add_action(&rename);
    app.set_accels_for_action("win.rename", &["F2"]);

    let undo = gio::SimpleAction::new("undo-rename", None);
    undo.connect_activate({
        let (state, status) = (state.clone(), status.clone());
        move |_, _| start_undo_rename(&state, &status)
    });
    window.add_action(&undo);

    {
        let (state, tabs, breadcrumb, weak_window) = (
            state.clone(),
            tabs.clone(),
            breadcrumb.clone(),
            window.downgrade(),
        );
        let (address, status, location_bar, views, model, selection) = (
            address.clone(),
            status.clone(),
            location_bar.downgrade(),
            views.downgrade(),
            model.clone(),
            selection.clone(),
        );
        let open_tab = move |action: &gio::SimpleAction, parameter: Option<&glib::Variant>| {
            let Some(window) = weak_window.upgrade() else {
                return;
            };
            let path = {
                let mut browser = state.borrow_mut();
                if browser.dialog_open {
                    return;
                }
                let changed = match action.name().as_str() {
                    "new-tab" => browser.tabs.new_next(),
                    "close-tab" => browser.tabs.close_active(),
                    "next-tab" => browser.tabs.cycle(false),
                    "previous-tab" => browser.tabs.cycle(true),
                    "activate-tab" => parameter
                        .and_then(glib::Variant::get::<u64>)
                        .map(TabId::from_action_parameter)
                        .is_some_and(|id| browser.tabs.activate(id)),
                    _ => false,
                };
                if !changed {
                    if action.name().as_str() == "new-tab" {
                        status.set_text("最多可打开 16 个标签页。");
                    }
                    return;
                }
                let path = browser.path().clone();
                // A tab records navigation only, not stale rows, parent FDs,
                // or a pending result. The one scanner cancels its prior
                // generation before opening the newly active directory.
                browser.parent = None;
                browser.entries = Rc::default();
                browser.active_scan = None;
                browser.reload_due = None;
                path
            };
            start_scan(&state, path.clone(), &address, &status);
            tabs.sync(&state.borrow().tabs);
            breadcrumb.show(&path);
            window.set_title(Some("JWM Files · 正在读取"));
            if let Some(location_bar) = location_bar.upgrade() {
                location_bar.set_visible_child_name("path");
            }
            selection.set_selected(gtk::INVALID_LIST_POSITION);
            model.replace(Rc::default());
            focus_current_view(&views);
        };
        for (name, accel) in [
            ("new-tab", "<Control>t"),
            ("close-tab", "<Control>w"),
            ("next-tab", "<Control>Page_Down"),
            ("previous-tab", "<Control>Page_Up"),
            ("activate-tab", ""),
        ] {
            let parameter = (name == "activate-tab").then_some(glib::VariantTy::UINT64);
            let action = gio::SimpleAction::new(name, parameter);
            action.connect_activate(open_tab.clone());
            window.add_action(&action);
            if !accel.is_empty() {
                app.set_accels_for_action(&format!("win.{name}"), &[accel]);
            }
        }
    }

    {
        let state = state.clone();
        window.connect_close_request(move |_| {
            let mut state = state.borrow_mut();
            if let Some(active) = state.activation.take() {
                active.cancel.cancel();
            }
            if let Some((cancel, _)) = state.mutation.as_ref().and_then(|work| work.copy.as_ref()) {
                cancel.cancel();
            }
            glib::Propagation::Proceed
        });
    }
    start_scan(&state, initial.into(), &address, &status);
    let weak_window = window.downgrade();
    let process = move || {
        let Some(window) = weak_window.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let mut browser = state.borrow_mut();
        let mut replaced = false;
        let mut restore_row = None;
        if let Some(reply) = browser.worker.take_reply()
            && let Some((id, requested)) = browser.pending.take()
            && id == reply.id
        {
            let automatic = browser.automatic_scan.take() == Some(id);
            match reply.result {
                Ok(snapshot) => {
                    if automatic
                        && browser
                            .parent
                            .as_ref()
                            .zip(snapshot.local_parent.as_ref())
                            .is_some_and(|(old, new)| old.identity() == new.identity())
                    {
                        restore_row = browser
                            .entries
                            .get(selection.selected() as usize)
                            .and_then(|old| selection::find_restorable_row(old, &snapshot.entries))
                            .map(|row| (row, id));
                    }
                    browser.active_scan = Some(id);
                    browser.reload_due = None;
                    let path = requested.path().to_path_buf();
                    let tab = browser.tabs.active_mut();
                    if !tab.navigation.accept(requested) {
                        return glib::ControlFlow::Continue;
                    }
                    tab.path = path;
                    browser.entries = Rc::new(snapshot.entries);
                    browser.parent = snapshot.local_parent;
                    replaced = true;
                    let base = format!("{} 项", browser.entries.len());
                    let label = if snapshot.skipped_entries == 0 {
                        base
                    } else {
                        format!("{base} · {} 项读取失败", snapshot.skipped_entries)
                    };
                    status.set_text(&label);
                    window.set_title(Some(&format!("JWM Files · {}", browser.path().display())));
                }
                Err(error) => {
                    address.set_text(&browser.path().to_string_lossy());
                    status.set_text(&format!("无法读取目录：{error}"));
                    window.set_title(Some(&format!("JWM Files · {}", browser.path().display())));
                }
            }
        }
        if replaced {
            // A reply and a notice/mutation may have shared one wake. Process
            // remaining results in a separate turn after replacing GTK rows.
            browser.wake.signal();
            let path = browser.path().clone();
            let entries = browser.entries.clone();
            tabs.sync(&browser.tabs);
            drop(browser);
            breadcrumb.show(&path);
            selection.set_selected(gtk::INVALID_LIST_POSITION);
            model.replace(entries);
            if let Some((row, id)) = restore_row
                && let Ok(row) = u32::try_from(row)
                && {
                    let state = state.borrow();
                    state.active_scan == Some(id) && state.pending.is_none()
                }
                && selection.selected() == gtk::INVALID_LIST_POSITION
            {
                selection.set_selected(row);
            }
            return glib::ControlFlow::Continue;
        }
        let completed = browser
            .mutation
            .as_ref()
            .and_then(|work| match work.reply.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err("文件操作未返回结果".into())),
                Err(mpsc::TryRecvError::Empty) => None,
            });
        if let Some(result) = completed
            && let Some(work) = browser.mutation.take()
        {
            let result = result.map(|outcome| match outcome {
                MutationOutcome::Rename(receipt) => browser.rename_undo = receipt.map(Arc::new),
                MutationOutcome::RenameUndone => browser.rename_undo = None,
                MutationOutcome::FileOperation => {}
            });
            let should_refresh = browser.pending.is_none()
                && browser.path() == &work.path
                && browser
                    .parent
                    .as_ref()
                    .is_some_and(|p| p.identity() == work.parent.identity());
            if let Some((_, detail)) = &work.copy {
                let text = match &result {
                    Ok(()) => format!("已复制：{detail}"),
                    Err(error) => error.to_string(),
                };
                copy_ui::notice(&copy_status, &text);
            }
            if should_refresh {
                match result {
                    Ok(()) => {
                        let path = browser.path().clone();
                        drop(browser);
                        start_scan(&state, path, &address, &status);
                        return glib::ControlFlow::Continue;
                    }
                    Err(error) if work.copy.is_none() => {
                        status.set_text(&format!("无法{}：{error}", work.operation));
                    }
                    Err(_) => {}
                }
            }
        }
        if browser.pending.is_none()
            && browser.mutation.is_none()
            && !browser.dialog_open
            && browser.activation.is_none()
        {
            if let Some(notice) = browser.worker.take_directory_change()
                && browser.active_scan == Some(notice.id)
            {
                match notice.change {
                    DirectoryChange::Dirty => {
                        if browser.reload_due.is_none() {
                            let due = Rc::new(Cell::new(false));
                            browser.reload_due = Some(due.clone());
                            let wake = browser.wake.clone();
                            glib::timeout_add_local_once(Duration::from_millis(150), move || {
                                due.set(true);
                                wake.signal();
                            });
                        }
                    }
                    DirectoryChange::Invalidated => {
                        browser.reload_due = None;
                        browser.parent = None;
                        browser.active_scan = None;
                        selection.set_selected(gtk::INVALID_LIST_POSITION);
                        status.set_text("目录已移动或不可用，请刷新或重新打开。");
                    }
                    DirectoryChange::Unchanged => {}
                }
            }
            if browser.reload_due.as_ref().is_some_and(|due| due.get()) {
                browser.reload_due = None;
                let path = browser.path().clone();
                let id = browser.worker.submit(
                    Location::Local(path.clone()),
                    ListOptions {
                        show_hidden: browser.show_hidden,
                    },
                );
                browser.automatic_scan = Some(id);
                browser.pending = Some((id, Destination::Visit(path)));
            }
        }

        glib::ControlFlow::Continue
    };
    let pump = glib::MainContext::default().spawn_local(async move {
        while notifications.recv().await.is_ok() {
            if process() == glib::ControlFlow::Break {
                break;
            }
        }
    });
    window.connect_destroy(move |_| pump.abort());
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
    // GLib may consult the desktop's XDG user-directories file on first use.
    // Do so once before GTK event processing, not on the navigation path.
    let special = [
        glib::UserDirectory::Documents,
        glib::UserDirectory::Downloads,
    ]
    .map(|kind| (kind, glib::user_special_dir(kind)));
    app.connect_activate(move |app| open_window(app, &initial, &home, &special));
    app.run_with_args(&["jwm-files"])
}

#[cfg(test)]
mod activation_tests {
    use super::*;

    #[test]
    fn desktop_places_use_configured_paths_without_duplicates() {
        use glib::UserDirectory::{Documents, Downloads};
        let home = Path::new("/home/example");
        let custom = home.join("资料");
        let actual = place_locations(
            home,
            &[
                (Documents, Some(custom.clone())),
                (Downloads, Some(custom.clone())),
                (Downloads, Some(PathBuf::from("/"))),
                (Downloads, Some(PathBuf::from("relative"))),
                (Downloads, Some(home.to_path_buf())),
                (Downloads, None),
            ],
        );
        assert_eq!(
            actual,
            [
                ("主目录", home.to_path_buf()),
                ("文档", custom),
                ("根目录", PathBuf::from("/")),
            ]
        );
    }

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

#[cfg(all(test, target_os = "linux"))]
mod wayland_acceptance {
    use super::*;
    use std::{fs, os::unix::ffi::OsStrExt, time::Instant};

    fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut result = vec![widget.clone()];
        let mut child = widget.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            result.extend(descendants(&widget));
        }
        result
    }

    fn until(mut check: impl FnMut() -> bool, description: &str) {
        let context = glib::MainContext::default();
        let end = Instant::now() + Duration::from_secs(6);
        while !check() {
            assert!(Instant::now() < end, "Wayland timed out: {description}");
            for _ in 0..16 {
                if !context.iteration(false) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(8));
        }
    }

    fn first_label(model: &row_model::FileRowModel) -> Option<String> {
        model
            .item(0)
            .and_downcast::<glib::BoxedAnyObject>()
            .map(|row| row.borrow::<row_model::RowPresentation>().list_label())
    }

    #[test]
    #[ignore = "requires a private native Wayland compositor"]
    fn native_wayland_window_maps_and_navigates() {
        gtk::init().unwrap();
        let display = gtk::gdk::Display::default().unwrap();
        assert_eq!(display.type_().name(), "GdkWaylandDisplay");
        let root = PathBuf::from(std::env::var_os("JWM_FILE_SMOKE_FIXTURE").unwrap());
        let start = root.join("start");
        let next = root.join("next");
        fs::create_dir(&start).unwrap();
        fs::create_dir(&next).unwrap();
        fs::write(start.join("before.txt"), b"old").unwrap();
        fs::write(next.join("after.txt"), b"new").unwrap();
        let app = gtk::Application::builder()
            .application_id("org.jwm.Files.WaylandSmoke")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gio::Cancellable>).unwrap();
        open_window(&app, &start, &root, &[]);
        let window = app.active_window().unwrap();
        let main_model = descendants(window.upcast_ref())
            .into_iter()
            .find_map(|widget| {
                widget
                    .downcast::<gtk::ListView>()
                    .ok()?
                    .model()?
                    .downcast::<gtk::SingleSelection>()
                    .ok()?
                    .model()?
                    .downcast::<row_model::FileRowModel>()
                    .ok()
            })
            .unwrap();
        let first_title = format!("JWM Files · {}", start.display());
        until(
            || {
                window.is_mapped()
                    && window.surface().is_some_and(|surface| surface.is_mapped())
                    && window.title().as_deref() == Some(&first_title)
                    && first_label(&main_model).as_deref() == Some("before.txt")
            },
            "first directory",
        );
        let drew = Rc::new(Cell::new(false));
        window.add_tick_callback({
            let drew = drew.clone();
            move |_, _| {
                drew.set(true);
                glib::ControlFlow::Break
            }
        });
        until(|| drew.get(), "native frame clock");

        gtk::prelude::WidgetExt::activate_action(&window, "win.location", None).unwrap();
        let address = descendants(window.upcast_ref())
            .into_iter()
            .find_map(|widget| widget.downcast::<gtk::Entry>().ok())
            .unwrap();
        address.set_text(next.to_str().unwrap());
        address.emit_activate();
        let second_title = format!("JWM Files · {}", next.display());
        until(
            || {
                window.is_mapped()
                    && window.title().as_deref() == Some(&second_title)
                    && main_model.n_items() == 1
                    && first_label(&main_model).as_deref() == Some("after.txt")
            },
            "replacement directory",
        );

        let grid = descendants(window.upcast_ref())
            .into_iter()
            .find_map(|widget| widget.downcast::<gtk::GridView>().ok())
            .unwrap();
        let grid_selection = grid.model().and_downcast::<gtk::SingleSelection>().unwrap();
        grid_selection.set_selected(0);
        gtk::prelude::WidgetExt::activate_action(&window, "win.show-grid", None).unwrap();
        until(|| grid.is_mapped(), "icon view mapping");
        assert_eq!(grid_selection.selected(), 0);
        gtk::prelude::WidgetExt::activate_action(&window, "win.show-list", None).unwrap();
        until(|| !grid.is_mapped(), "return to list view");
        assert_eq!(grid_selection.selected(), 0);
        println!("GTK4 Wayland grid: list and icon modes kept the same selection");
        gtk::prelude::WidgetExt::activate_action(&window, "win.quick-preview", None).unwrap();
        let mut shown = None;
        until(
            || {
                shown = app
                    .windows()
                    .into_iter()
                    .find(|pane| pane.title().as_deref() == Some("JWM Files · Preview"));
                shown.is_some()
            },
            "preview window",
        );
        let preview_window = shown.unwrap();
        until(
            || {
                descendants(preview_window.upcast_ref())
                    .iter()
                    .any(|widget| {
                        widget.downcast_ref::<gtk::TextView>().is_some_and(|text| {
                            let buffer = text.buffer();
                            buffer
                                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                                .as_str()
                                == "new"
                        })
                    })
            },
            "exact preview text",
        );
        preview_window.close();
        println!("GTK4 Wayland preview: selected file text shown in the quick preview");

        // An unchanged destination alone cannot distinguish a no-op or a
        // still-running copy from an actual no-replace decision.
        let conflict = root.join("conflict");
        let target = root.join("copy-target");
        fs::create_dir(&conflict).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(conflict.join("after.txt"), b"existing").unwrap();
        let action = |name| {
            gtk::prelude::WidgetExt::activate_action(&window, name, None).unwrap();
        };
        let navigate = |path: &Path| {
            action("win.location");
            address.set_text(path.to_str().unwrap());
            address.emit_activate();
            let title = format!("JWM Files · {}", path.display());
            until(
                || window.title().as_deref() == Some(title.as_str()),
                "copy fixture directory",
            );
        };
        let status_matches = |check: &dyn Fn(&str) -> bool| {
            descendants(window.upcast_ref()).iter().any(|widget| {
                widget
                    .downcast_ref::<gtk::Label>()
                    .is_some_and(|label| label.is_visible() && check(label.text().as_str()))
            })
        };
        action("win.copy-file");
        assert!(status_matches(&|text| text.starts_with("已选定复制：")));
        navigate(&conflict);
        action("win.paste-file");
        until(
            || status_matches(&|text| text == "目标已有同名项目，未覆盖。"),
            "copy rejected a named conflict",
        );
        assert_eq!(fs::read(conflict.join("after.txt")).unwrap(), b"existing");
        navigate(&target);
        action("win.paste-file");
        until(
            || status_matches(&|text| text.starts_with("已复制：")),
            "copy confirmed after conflict",
        );
        assert_eq!(fs::read(target.join("after.txt")).unwrap(), b"new");
        assert_eq!(fs::read(next.join("after.txt")).unwrap(), b"new");
        assert_eq!(fs::read(conflict.join("after.txt")).unwrap(), b"existing");
        println!("GTK4 Wayland copy: delivered conflict status before a successful paste");
        navigate(&next);
        assert_eq!(first_label(&main_model).as_deref(), Some("after.txt"));
        grid_selection.set_selected(0);

        // The scanner can publish while a mutation dialog is open. The
        // pending invalidation must survive until GTK handles Cancel.
        gtk::prelude::WidgetExt::activate_action(&window, "win.new-folder", None).unwrap();
        let dialog = gtk::Window::list_toplevels()
            .into_iter()
            .find_map(|pane| {
                let pane = pane.downcast::<gtk::Dialog>().ok()?;
                (pane.title().as_deref() == Some("JWM Files · New Folder")
                    && pane.transient_for().as_ref() == Some(&window))
                .then_some(pane)
            })
            .unwrap();
        fs::rename(&next, root.join("moved")).unwrap();
        let allow_watch_signal = Instant::now() + Duration::from_millis(420);
        until(
            || Instant::now() >= allow_watch_signal,
            "watch while dialog is open",
        );
        assert_eq!(grid_selection.selected(), 0);
        dialog.response(gtk::ResponseType::Cancel);
        until(
            || {
                grid_selection.selected() == gtk::INVALID_LIST_POSITION
                    && descendants(window.upcast_ref()).iter().any(|widget| {
                        widget.downcast_ref::<gtk::Label>().is_some_and(|label| {
                            label.text().as_str() == "目录已移动或不可用，请刷新或重新打开。"
                        })
                    })
            },
            "active directory invalidation",
        );
        println!("GTK4 Wayland grid: moving the active directory cleared stale selection");
        println!("GTK4 Wayland wake: cancelling a dialog delivered deferred invalidation");
        println!("GTK4 Wayland smoke: native window mapped and navigation replaced the listing");

        // Two tabs must not share history or accept a late scan for the
        // directory that was requested in a tab we have left.
        navigate(&conflict);
        navigate(&start);
        action("win.new-tab");
        let at = |path: &Path, expected: &str| {
            let title = format!("JWM Files · {}", path.display());
            until(
                || {
                    window.title().as_deref() == Some(title.as_str())
                        && first_label(&main_model).as_deref() == Some(expected)
                },
                "confirmed tab path and first row",
            );
        };
        at(&start, "before.txt");
        action("win.location");
        address.set_text(target.to_str().unwrap());
        address.emit_activate();
        action("win.previous-tab");
        assert_eq!(grid_selection.selected(), gtk::INVALID_LIST_POSITION);
        at(&start, "before.txt");
        action("win.next-tab");
        at(&start, "before.txt");
        navigate(&target);
        at(&target, "after.txt");
        action("win.previous-tab");
        at(&start, "before.txt");
        action("win.back");
        at(&conflict, "after.txt");
        action("win.next-tab");
        at(&target, "after.txt");
        action("win.back");
        at(&start, "before.txt");
        let conflict_tab = descendants(window.upcast_ref())
            .into_iter()
            .find_map(|widget| {
                let button = widget.downcast::<gtk::Button>().ok()?;
                let label = button.child().and_downcast::<gtk::Label>()?;
                (button.is_mapped()
                    && button.action_name().as_deref() == Some("win.activate-tab")
                    && label.text().as_str() == "conflict")
                    .then_some(button)
            })
            .unwrap();
        conflict_tab.emit_clicked();
        at(&conflict, "after.txt");
        action("win.close-tab");
        at(&start, "before.txt");
        action("win.close-tab");
        assert!(window.is_mapped());
        println!("GTK4 Wayland tabs: separate history, cancelled scans and tab buttons verified");

        let jwm_source = root.join("jwm/src");
        let companion_source = root.join("companion/src");
        fs::create_dir_all(&jwm_source).unwrap();
        fs::create_dir_all(&companion_source).unwrap();
        fs::write(jwm_source.join("jwm.txt"), b"first").unwrap();
        fs::write(companion_source.join("companion.txt"), b"second").unwrap();
        navigate(&jwm_source);
        action("win.new-tab");
        at(&jwm_source, "jwm.txt");
        navigate(&companion_source);
        at(&companion_source, "companion.txt");
        let tab_button = |name: &str| {
            descendants(window.upcast_ref())
                .into_iter()
                .find_map(|widget| {
                    let button = widget.downcast::<gtk::Button>().ok()?;
                    let label = button.child().and_downcast::<gtk::Label>()?;
                    (button.is_mapped()
                        && button.action_name().as_deref() == Some("win.activate-tab")
                        && label.text().as_str() == name)
                        .then_some(button)
                })
                .unwrap()
        };
        tab_strip::assert_selected(&tab_button("jwm/src"), false);
        tab_strip::assert_selected(&tab_button("companion/src"), true);
        tab_button("jwm/src").emit_clicked();
        at(&jwm_source, "jwm.txt");
        tab_strip::assert_selected(&tab_button("jwm/src"), true);
        tab_strip::assert_selected(&tab_button("companion/src"), false);
        println!(
            "GTK4 Wayland tabs: matching folder names disambiguated and accessible selection changed"
        );
        let project_a = root.join("project-a");
        let project_b = root.join("project-b");
        fs::create_dir(&project_a).unwrap();
        fs::create_dir(&project_b).unwrap();
        fs::write(project_a.join("README.md"), b"# project A preview").unwrap();
        fs::write(project_b.join("README.md"), b"# project B preview").unwrap();
        navigate(&project_a);
        at(&project_a, "README.md");
        grid_selection.set_selected(0);
        action("win.quick-preview");
        let popup_for = || {
            app.windows()
                .into_iter()
                .find(|w| w.title().as_deref() == Some("JWM Files · Preview"))
        };
        let mut popup = None;
        until(
            || {
                popup = popup_for();
                popup.is_some()
            },
            "same-name preview popup",
        );
        let popup = popup.unwrap();
        let preview_at = |path: &Path, expected: &str| {
            let full = path.join("README.md");
            until(
                || {
                    let widgets = descendants(popup.upcast_ref());
                    widgets.iter().any(|w| {
                        w.downcast_ref::<gtk::Label>().is_some_and(|label| {
                            label.text().as_str() == full.to_string_lossy().as_ref()
                        })
                    }) && widgets.iter().any(|w| {
                        w.downcast_ref::<gtk::TextView>().is_some_and(|text| {
                            let buffer = text.buffer();
                            buffer
                                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                                .trim()
                                == expected
                        })
                    })
                },
                "current preview body and source path",
            );
        };
        preview_at(&project_a, "project A preview");
        action("win.next-tab");
        at(&companion_source, "companion.txt");
        navigate(&project_b);
        at(&project_b, "README.md");
        grid_selection.set_selected(0);
        action("win.quick-preview");
        assert_eq!(popup_for().as_ref(), Some(&popup));
        preview_at(&project_b, "project B preview");
        println!(
            "GTK4 Wayland preview: an open preview followed the newly selected same-name file"
        );
        let markdown_widgets = descendants(popup.upcast_ref());
        let buffer = markdown_widgets
            .iter()
            .find_map(|widget| {
                widget
                    .downcast_ref::<gtk::TextView>()
                    .map(|view| view.buffer())
            })
            .unwrap();
        assert!(
            buffer
                .start_iter()
                .tags()
                .iter()
                .any(|tag| tag.weight() >= 700)
        );
        let source_mode = markdown_widgets
            .iter()
            .find_map(|widget| {
                let toggle = widget.clone().downcast::<gtk::ToggleButton>().ok()?;
                (toggle.is_visible() && toggle.label().as_deref() == Some("查看源码"))
                    .then_some(toggle)
            })
            .unwrap();
        source_mode.set_active(true);
        assert_eq!(
            buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .as_str(),
            "# project B preview"
        );
        source_mode.set_active(false);
        preview_at(&project_b, "project B preview");
        println!("GTK4 Wayland Markdown: native heading style and exact source toggle verified");
        popup.close();

        let undo_folder = root.join("undo-fixture");
        fs::create_dir(&undo_folder).unwrap();
        let original = undo_folder.join("initial.txt");
        let renamed = undo_folder.join("renamed.txt");
        fs::write(&original, b"exact original").unwrap();
        navigate(&undo_folder);
        at(&undo_folder, "initial.txt");
        grid_selection.set_selected(0);
        action("win.rename");
        let dialog = gtk::Window::list_toplevels()
            .into_iter()
            .find_map(|pane| {
                let pane = pane.downcast::<gtk::Dialog>().ok()?;
                (pane.title().as_deref() == Some("JWM Files · Rename")
                    && pane.transient_for().as_ref() == Some(&window))
                .then_some(pane)
            })
            .unwrap();
        let rename_input = descendants(dialog.upcast_ref())
            .into_iter()
            .find_map(|widget| widget.downcast::<gtk::Entry>().ok())
            .unwrap();
        rename_input.set_text("renamed.txt");
        dialog.response(gtk::ResponseType::Accept);
        at(&undo_folder, "renamed.txt");

        navigate(&project_b);
        at(&project_b, "README.md");
        action("win.undo-rename");
        assert!(status_matches(
            &|text| text == "请回到执行重命名的目录再撤销。"
        ));
        assert_eq!(fs::read(&renamed).unwrap(), b"exact original");
        navigate(&undo_folder);
        at(&undo_folder, "renamed.txt");
        fs::write(&original, b"keep conflict").unwrap();
        action("win.undo-rename");
        until(
            || status_matches(&|text| text.contains("原名称已被占用，未覆盖任何文件。")),
            "undo conflict result",
        );
        assert_eq!(fs::read(&original).unwrap(), b"keep conflict");
        assert_eq!(fs::read(&renamed).unwrap(), b"exact original");
        fs::remove_file(&original).unwrap();
        action("win.undo-rename");
        until(
            || {
                fs::read(&original).is_ok_and(|bytes| bytes == b"exact original")
                    && !renamed.exists()
                    && first_label(&main_model).as_deref() == Some("initial.txt")
                    && status_matches(&|text| text == "1 项")
            },
            "successful undo and completed directory refresh",
        );
        action("win.undo-rename");
        assert!(status_matches(&|text| text == "没有可撤销的重命名。"));
        println!("GTK4 Wayland undo: directory-bound rename conflicts were non-destructive");

        let trash = glib::user_data_dir().join("Trash");
        fs::create_dir_all(trash.join("files")).unwrap();
        fs::create_dir_all(trash.join("info")).unwrap();
        let metadata = b"[Trash Info]\nPath=tests/note%25%80.txt\nDeletionDate=2026-11-26T12:26:00";
        for (slot, content) in [("slot-A", b"old"), ("slot-B", b"new")] {
            fs::write(trash.join("files").join(slot), content).unwrap();
            fs::write(
                trash.join("info").join(format!("{slot}.trashinfo")),
                metadata,
            )
            .unwrap();
        }
        fs::write(trash.join("files/orphan"), b"stay").unwrap();
        action("win.home-trash");
        let mut panel = None;
        until(
            || {
                panel = app
                    .windows()
                    .into_iter()
                    .find(|window| window.title().as_deref() == Some("JWM Files · Home Trash"));
                panel.is_some()
            },
            "read-only home Trash panel",
        );
        let panel = panel.unwrap();
        let (trash_selection, model) = descendants(panel.upcast_ref())
            .into_iter()
            .find_map(|widget| {
                let selection = widget
                    .downcast::<gtk::ListView>()
                    .ok()?
                    .model()?
                    .downcast::<gtk::SingleSelection>()
                    .ok()?;
                let rows = selection.model()?.downcast::<gtk::StringList>().ok()?;
                Some((selection, rows))
            })
            .unwrap();
        until(|| model.n_items() == 3, "home Trash snapshot published");
        for (i, slot) in ["slot-A", "slot-B"].into_iter().enumerate() {
            let actual = model.string(i as u32).unwrap();
            assert!(actual.contains("note%"));
            assert!(actual.contains("2026-11-26T12:26:00"));
            assert!(actual.ends_with(slot));
        }
        assert!(model.string(2).unwrap().starts_with("原路径未知"));
        assert_eq!(fs::read(trash.join("files/slot-A")).unwrap(), b"old");
        assert_eq!(fs::read(trash.join("files/slot-B")).unwrap(), b"new");
        println!("GTK4 Wayland Trash: same-path entries remained distinct in the home view");

        let original_folder = glib::user_data_dir().join("tests");
        fs::create_dir(&original_folder).unwrap();
        let original = original_folder.join(std::ffi::OsStr::from_bytes(b"note%\x80.txt"));
        fs::write(&original, b"occupied").unwrap();
        let recover = descendants(panel.upcast_ref())
            .into_iter()
            .find_map(|widget| {
                let button = widget.downcast::<gtk::Button>().ok()?;
                (button.label().as_deref() == Some("恢复副本")).then_some(button)
            })
            .unwrap();
        trash_selection.set_selected(2);
        assert!(!recover.is_sensitive(), "orphan must not offer recovery");
        trash_selection.set_selected(1);
        assert!(recover.is_sensitive());
        let confirm = |response| {
            recover.emit_clicked();
            let dialog = gtk::Window::list_toplevels()
                .into_iter()
                .find_map(|widget| {
                    let dialog = widget.downcast::<gtk::Dialog>().ok()?;
                    (dialog.title().as_deref() == Some("JWM Files · Recover Copy")
                        && dialog.transient_for().as_ref() == Some(&panel))
                    .then_some(dialog)
                })
                .unwrap();
            assert!(descendants(dialog.upcast_ref()).iter().any(|widget| {
                widget.downcast_ref::<gtk::Label>().is_some_and(|label| {
                    label.text().contains("项目：slot-B") && label.text().contains("保留回收站原件")
                })
            }));
            dialog.response(response);
        };
        let say = |expected| {
            descendants(panel.upcast_ref()).iter().any(|widget| {
                widget
                    .downcast_ref::<gtk::Label>()
                    .is_some_and(|label| label.text().as_str() == expected)
            })
        };
        confirm(gtk::ResponseType::Cancel);
        assert_eq!(fs::read(&original).unwrap(), b"occupied");
        confirm(gtk::ResponseType::Accept);
        until(
            || say("原位置已有同名项目，未覆盖。回收站原件保留。"),
            "occupied restore destination accurately reported",
        );
        assert_eq!(fs::read(&original).unwrap(), b"occupied");
        fs::remove_file(&original).unwrap();
        confirm(gtk::ResponseType::Accept);
        until(
            || say("已恢复副本，回收站原件保留。"),
            "confirmed recovery copy",
        );
        assert_eq!(fs::read(&original).unwrap(), b"new");
        assert_eq!(fs::read(trash.join("files/slot-A")).unwrap(), b"old");
        assert_eq!(fs::read(trash.join("files/slot-B")).unwrap(), b"new");
        assert_eq!(model.n_items(), 3);
        println!(
            "GTK4 Wayland Trash: selected the exact revision and kept both entries on recovery"
        );
        panel.close();
        window.close();
    }
}
