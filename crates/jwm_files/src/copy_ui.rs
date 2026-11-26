use super::{Browser, MutationOutcome, MutationWork, Shared, SignalOnDrop};
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use jwm_file_core::{
    Cancellation, CopyDestination, CopyError, CopySource, DirectoryHandle, FileIdentity, FileKind,
};
use std::{
    cell::{OnceCell, RefCell},
    ffi::OsString,
    path::PathBuf,
    rc::{Rc, Weak},
    sync::{Arc, mpsc},
    thread,
};

#[derive(Clone)]
pub(super) struct StashedCopy {
    parent: Arc<DirectoryHandle>,
    path: PathBuf,
    name: OsString,
    identity: FileIdentity,
}

/// One file source shared by this application instance, never the text clipboard.
pub(super) type CopyBuffer = Rc<RefCell<Option<StashedCopy>>>;

pub(super) fn notice(label: &gtk::Label, text: &str) {
    label.set_text(text);
    label.set_visible(!text.is_empty());
}

fn stash(state: &Shared, buffer: &CopyBuffer, selected: u32, status: &gtk::Label) {
    let browser = state.borrow();
    if browser.mutation.is_some() || browser.dialog_open {
        return;
    }
    let candidate = copy_source(&browser, selected);
    let Some(candidate) = candidate else {
        // Refuse to leave an earlier, hidden choice armed after a failed copy.
        buffer.replace(None);
        notice(status, "请选择一个可复制的普通文件。");
        return;
    };
    notice(
        status,
        &format!(
            "已选定复制：{}",
            candidate
                .path
                .join(&candidate.name)
                .to_string_lossy()
                .escape_debug()
        ),
    );
    buffer.replace(Some(candidate));
}

fn copy_source(browser: &Browser, selected: u32) -> Option<StashedCopy> {
    browser
        .parent
        .as_ref()
        .filter(|_| browser.pending.is_none())
        .zip(browser.entries.get(selected as usize))
        .filter(|(_, entry)| entry.kind == FileKind::Regular)
        .and_then(|(parent, entry)| {
            Some(StashedCopy {
                parent: parent.clone(),
                path: browser.path().clone(),
                name: entry.name.clone(),
                identity: entry.identity?,
            })
        })
}

fn drag_trace(message: impl std::fmt::Display) {
    if std::env::var_os("JWM_FILES_TRACE_DRAG").is_some() {
        eprintln!("JWM Files drag: {message}");
    }
}

#[derive(Clone, glib::Boxed)]
#[boxed_type(name = "JwmFilesLocalDragCopy")]
struct DragCopy(StashedCopy);

#[derive(Clone, Default)]
pub(super) struct RowDrags(Rc<OnceCell<Weak<RefCell<Browser>>>>);

impl RowDrags {
    pub(super) fn bind_browser(&self, state: &Shared) {
        let _ = self.0.set(Rc::downgrade(state));
    }

    /// Resolve the row under the drag gesture, not whichever row happens to be selected.
    /// Read ListItem.position at gesture time because GTK reuses these widgets.
    pub(super) fn attach(&self, item: &gtk::ListItem, widget: &impl IsA<gtk::Widget>) {
        let source = gtk::DragSource::builder()
            .actions(gtk::gdk::DragAction::COPY)
            .build();
        let (link, item) = (self.clone(), item.downgrade());
        source.connect_prepare(move |_, _, _| {
            let item = item.upgrade()?;
            let state = link.0.get()?.upgrade()?;
            let browser = state.try_borrow().ok()?;
            if browser.dialog_open || browser.mutation.is_some() || item.item().is_none() {
                return None;
            }
            let file = copy_source(&browser, item.position())?;
            drag_trace("prepared a regular row");
            let typed = gtk::gdk::ContentProvider::for_value(&DragCopy(file).to_value());
            // XDND announces MIME atoms even for same-process top-level windows.
            // Advertise a valueless marker, never a filename or contents. Local
            // identity plus the non-serializable DragCopy is still mandatory.
            let marker = gtk::gdk::ContentProvider::for_bytes(
                "application/x-jwm-files-local-copy",
                &glib::Bytes::from_static(b"JWM Files local copy"),
            );
            Some(gtk::gdk::ContentProvider::new_union(&[typed, marker]))
        });
        source.connect_drag_begin(|_, _| drag_trace("started"));
        source.connect_drag_cancel(|_, _, reason| {
            drag_trace(format_args!("cancelled {reason:?}"));
            false
        });
        widget.add_controller(source);
    }
}

pub(super) fn install_drop_zone(zone: &impl IsA<gtk::Widget>, state: &Shared, status: &gtk::Label) {
    let target = gtk::DropTarget::new(DragCopy::static_type(), gtk::gdk::DragAction::COPY);
    target.connect_accept(|_, drop| {
        let local = drop.drag().is_some();
        let copy = drop.actions().contains(gtk::gdk::DragAction::COPY);
        let typed = drop.formats().contains_type(DragCopy::static_type());
        drag_trace(format_args!(
            "entered target: local={local} copy={copy} typed={typed}"
        ));
        local && copy && typed
    });
    let (state, status) = (state.clone(), status.clone());
    target.connect_drop(move |_, data, _, _| {
        if let Ok(DragCopy(source)) = data.get::<DragCopy>() {
            let queued = queue_copy(&state, source, &status);
            drag_trace(format_args!("drop queued={queued}"));
            queued
        } else {
            drag_trace("drop had an unexpected type");
            false
        }
    });
    target.set_preload(true);
    zone.add_controller(target);
}

fn copy_error_message(error: CopyError) -> String {
    use std::io::ErrorKind;
    match error {
        CopyError::Cancelled => "已取消，未写入目标文件。".into(),
        CopyError::DestinationExists => "目标已有同名项目，未覆盖。".into(),
        CopyError::ParentChanged | CopyError::SourceChanged | CopyError::NotRegular => {
            "源或目标已改变，未写入目标文件。".into()
        }
        CopyError::StagingUnavailable(ref reason) if reason.kind() == ErrorKind::Unsupported => {
            "目标文件系统不支持安全复制。请选择其他目标目录。".into()
        }
        CopyError::StagingUnavailable(ref reason)
            if reason.kind() == ErrorKind::PermissionDenied =>
        {
            "无法写入目标目录，请检查目录权限。".into()
        }
        CopyError::Io(ref reason) if reason.kind() == ErrorKind::PermissionDenied => {
            "无法读取源文件或写入目标目录，请检查权限。".into()
        }
        _ => format!("复制失败：{error}"),
    }
}

fn paste(state: &Shared, buffer: &CopyBuffer, status: &gtk::Label) {
    let Some(source) = buffer.borrow().clone() else {
        notice(status, "请先选择要复制的普通文件。");
        return;
    };
    queue_copy(state, source, status);
}

fn queue_copy(state: &Shared, source: StashedCopy, status: &gtk::Label) -> bool {
    let mut browser = state.borrow_mut();
    if browser.mutation.is_some() || browser.dialog_open {
        notice(status, "有文件操作正在进行，请稍候。");
        return false;
    }
    let Some(parent) = browser.parent.clone().filter(|_| browser.pending.is_none()) else {
        notice(status, "请先打开目标目录。");
        return false;
    };
    if parent.identity() == source.parent.identity() {
        notice(status, "请选择不同的目标目录。同名项目不会被覆盖。");
        return false;
    }
    let path = browser.path().clone();
    let (target, target_path) = (parent.clone(), path.clone());
    let cancellation = Cancellation::default();
    let on_worker = cancellation.clone();
    let label = format!(
        "{} → {}",
        source
            .path
            .join(&source.name)
            .to_string_lossy()
            .escape_debug(),
        path.to_string_lossy().escape_debug()
    );
    let (send, reply) = mpsc::sync_channel(1);
    let wake = browser.wake.clone();
    match thread::Builder::new()
        .name("jwm-files-copy".into())
        .spawn(move || {
            let _notify = SignalOnDrop(wake);
            let send = send;
            let result = target
                .copy_regular_no_replace(
                    CopySource {
                        parent: &source.parent,
                        displayed_parent: &source.path,
                        name: &source.name,
                        expected_file: source.identity,
                    },
                    CopyDestination {
                        displayed_parent: &target_path,
                        name: &source.name,
                    },
                    &on_worker,
                )
                .map(|_| MutationOutcome::FileOperation)
                .map_err(copy_error_message);
            let _ = send.send(result);
        }) {
        Ok(_) => {
            browser.mutation = Some(MutationWork {
                parent,
                path,
                operation: "复制",
                reply,
                copy: Some((cancellation, label.clone())),
            });
            notice(status, &format!("正在复制：{label}"));
            true
        }
        Err(error) => {
            notice(status, &format!("无法启动复制：{error}"));
            false
        }
    }
}

pub(super) fn install(
    window: &gtk::ApplicationWindow,
    menu_button: &gtk::MenuButton,
    state: &Shared,
    buffer: &CopyBuffer,
    selection: &gtk::SingleSelection,
    status: &gtk::Label,
) {
    let menu = gio::Menu::new();
    menu.append(Some("新建窗口"), Some("win.new-window"));
    menu.append(Some("新建标签页（Ctrl+T）"), Some("win.new-tab"));
    menu.append(Some("快速预览（空格）"), Some("win.quick-preview"));
    menu.append(Some("主目录回收站"), Some("win.home-trash"));
    menu.append(Some("撤销重命名（Ctrl+Z）"), Some("win.undo-rename"));
    for (name, label) in [
        ("copy-file", "复制选中文件（Ctrl+C）"),
        ("paste-file", "粘贴到当前目录（Ctrl+V）"),
        ("cancel-file-copy", "取消正在进行的复制"),
    ] {
        let action = gio::SimpleAction::new(name, None);
        let (state, buffer, selection, status) = (
            state.clone(),
            buffer.clone(),
            selection.clone(),
            status.clone(),
        );
        action.connect_activate(move |action, _| match action.name().as_str() {
            "copy-file" => stash(&state, &buffer, selection.selected(), &status),
            "paste-file" => paste(&state, &buffer, &status),
            _ => {
                if let Some((cancel, _)) = state
                    .borrow()
                    .mutation
                    .as_ref()
                    .and_then(|work| work.copy.as_ref())
                {
                    cancel.cancel();
                    notice(&status, "正在取消复制…");
                }
            }
        });
        window.add_action(&action);
        menu.append(Some(label), Some(&format!("win.{name}")));
    }
    menu_button.set_menu_model(Some(&menu));

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak_window = window.downgrade();
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        use gtk::gdk::{Key, ModifierType};
        if !modifiers.contains(ModifierType::CONTROL_MASK)
            || modifiers.intersects(
                ModifierType::ALT_MASK | ModifierType::SUPER_MASK | ModifierType::SHIFT_MASK,
            )
        {
            return glib::Propagation::Proceed;
        }
        let action = match key {
            Key::c => "win.copy-file",
            Key::v => "win.paste-file",
            Key::z => "win.undo-rename",
            _ => return glib::Propagation::Proceed,
        };
        let Some(window) = weak_window.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let mut focused = gtk::prelude::GtkWindowExt::focus(&window);
        while let Some(widget) = focused {
            if widget.is::<gtk::Editable>() || widget.is::<gtk::TextView>() {
                return glib::Propagation::Proceed;
            }
            focused = widget.parent();
        }
        if gtk::prelude::WidgetExt::activate_action(&window, action, None).is_ok() {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    #[test]
    fn copy_errors_distinguish_unsupported_storage_permissions_and_cancellation() {
        let unsupported =
            copy_error_message(CopyError::StagingUnavailable(ErrorKind::Unsupported.into()));
        let denied = copy_error_message(CopyError::StagingUnavailable(Error::from(
            ErrorKind::PermissionDenied,
        )));
        let cancelled = copy_error_message(CopyError::Cancelled);
        assert!(unsupported.contains("不支持安全复制"));
        assert!(denied.contains("目录权限"));
        assert!(cancelled.contains("已取消"));
    }
}
