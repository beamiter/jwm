use super::{MutationOutcome, MutationWork, Shared, SignalOnDrop};
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use jwm_file_core::{
    Cancellation, CopyDestination, CopyError, CopySource, DirectoryHandle, FileIdentity, FileKind,
};
use std::{
    cell::RefCell,
    ffi::OsString,
    path::PathBuf,
    rc::Rc,
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
    let candidate = browser
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
        });
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

fn copy_path(window: &gtk::ApplicationWindow, state: &Shared, selected: u32, status: &gtk::Label) {
    if crate::comparison::has_focus(window) {
        return;
    }
    let path = {
        let browser = state.borrow();
        if browser.pending.is_some() || browser.dialog_open || browser.parent.is_none() {
            return;
        }
        browser
            .entries
            .get(selected as usize)
            .map(|entry| browser.path().join(&entry.name))
    };
    let Some(path) = path else {
        notice(status, "请选择要复制路径的项目。");
        return;
    };
    let (text, uri) = crate::native_path::path_text(&path);
    gtk::prelude::WidgetExt::display(window)
        .clipboard()
        .set_text(&text);
    notice(
        status,
        if uri {
            "已复制文件 URI，保留完整路径。"
        } else {
            "已复制完整路径。"
        },
    );
}

fn copy_error_message(error: CopyError) -> String {
    use std::io::ErrorKind;
    match error {
        CopyError::Cancelled => "已取消，未写入目标文件。".into(),
        CopyError::DestinationExists => "目标已有同名项目，未覆盖。".into(),
        CopyError::SiblingNamesExhausted => "没有可用的副本名，已有文件未更改。".into(),
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

fn paste(state: &Shared, buffer: &CopyBuffer, status: &gtk::Label, keep_both: bool) {
    let destination = {
        let browser = state.borrow();
        browser
            .parent
            .clone()
            .filter(|_| browser.pending.is_none())
            .map(|parent| (parent, browser.path().clone()))
    };
    let Some((parent, path)) = destination else {
        notice(status, "请先打开目标目录。");
        return;
    };
    paste_with_mode(state, buffer, status, parent, path, keep_both);
}

/// The destination is the clicked pane's committed directory, never the
/// currently focused path at a later point in the worker.
pub(super) fn paste_into(
    state: &Shared,
    buffer: &CopyBuffer,
    status: &gtk::Label,
    parent: Arc<DirectoryHandle>,
    path: PathBuf,
) {
    paste_with_mode(state, buffer, status, parent, path, false);
}

fn paste_with_mode(
    state: &Shared,
    buffer: &CopyBuffer,
    status: &gtk::Label,
    parent: Arc<DirectoryHandle>,
    path: PathBuf,
    keep_both: bool,
) {
    let mut browser = state.borrow_mut();
    if browser.mutation.is_some() || browser.dialog_open {
        notice(status, "有文件操作正在进行，请稍候。");
        return;
    }
    let Some(source) = buffer.borrow().clone() else {
        notice(status, "请先选择要复制的普通文件。");
        return;
    };
    if !keep_both && parent.identity() == source.parent.identity() {
        notice(status, "请选择不同的目标目录。同名项目不会被覆盖。");
        return;
    }
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
            let from = CopySource {
                parent: &source.parent,
                displayed_parent: &source.path,
                name: &source.name,
                expected_file: source.identity,
            };
            let result = if keep_both {
                target
                    .copy_regular_keep_both(from, &target_path, &on_worker)
                    .map(|receipt| MutationOutcome::CopyAsSibling(receipt.name))
            } else {
                target
                    .copy_regular_no_replace(
                        from,
                        CopyDestination {
                            displayed_parent: &target_path,
                            name: &source.name,
                        },
                        &on_worker,
                    )
                    .map(|_| MutationOutcome::FileOperation)
            };
            let _ = send.send(result.map_err(copy_error_message));
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
        }
        Err(error) => notice(status, &format!("无法启动复制：{error}")),
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
    let reopen_label = "恢复关闭的标签页（Ctrl+Shift+T）";
    menu.append(Some(reopen_label), Some("win.reopen-tab"));
    menu.append(
        Some("保存此窗口标签页（Ctrl+Alt+S）"),
        Some("win.save-tabs"),
    );
    menu.append(
        Some("恢复已保存标签页（Ctrl+Alt+R）"),
        Some("win.restore-tabs"),
    );
    menu.append(Some("在当前目录查找（Ctrl+F）"), Some("win.find-in-folder"));
    menu.append(Some("目录对照栏（F3）"), Some("win.comparison-pane"));
    menu.append(Some("快速预览（空格）"), Some("win.quick-preview"));
    menu.append(Some("属性（Alt+Enter）"), Some("win.item-properties"));
    menu.append(Some("主目录回收站"), Some("win.home-trash"));
    menu.append(Some("撤销重命名（Ctrl+Z）"), Some("win.undo-rename"));
    for (name, label) in [
        ("copy-file", "复制选中文件（Ctrl+C）"),
        ("copy-item-path", "复制完整路径（Ctrl+Shift+C）"),
        ("paste-file", "粘贴到当前目录（Ctrl+V）"),
        ("paste-as-copy", "粘贴为副本（Ctrl+Shift+V）"),
        ("cancel-file-copy", "取消正在进行的复制"),
    ] {
        let action = gio::SimpleAction::new(name, None);
        let (state, buffer, selection, status) = (
            state.clone(),
            buffer.clone(),
            selection.clone(),
            status.clone(),
        );
        let weak_window = window.downgrade();
        action.connect_activate(move |action, _| match action.name().as_str() {
            "copy-file" => stash(&state, &buffer, selection.selected(), &status),
            "copy-item-path" => {
                if let Some(window) = weak_window.upgrade() {
                    copy_path(&window, &state, selection.selected(), &status);
                }
            }
            "paste-file" => paste(&state, &buffer, &status, false),
            "paste-as-copy" => paste(&state, &buffer, &status, true),
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
            || modifiers.intersects(ModifierType::ALT_MASK | ModifierType::SUPER_MASK)
        {
            return glib::Propagation::Proceed;
        }
        let shifted = modifiers.contains(ModifierType::SHIFT_MASK);
        let action = match (key, shifted) {
            (Key::c | Key::C, true) => "win.copy-item-path",
            (Key::c, false) => "win.copy-file",
            (Key::v, false) => "win.paste-file",
            (Key::v | Key::V, true) => "win.paste-as-copy",
            (Key::z, false) => "win.undo-rename",
            _ => return glib::Propagation::Proceed,
        };
        let Some(window) = weak_window.upgrade() else {
            return glib::Propagation::Proceed;
        };
        if crate::comparison::has_focus(&window) {
            return glib::Propagation::Proceed;
        }
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
    fn copied_text_is_literal_or_round_trips_raw_bytes_without_a_newline() {
        use crate::native_path::path_text as text_path;
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::Path};

        let ordinary = Path::new("/tmp/a folder % 你好");
        let expected = (ordinary.to_str().unwrap().into(), false);
        assert_eq!(text_path(ordinary), expected);
        let raw = Path::new(OsStr::from_bytes(b"/tmp/path%\n\x80"));
        let (text, uri) = text_path(raw);
        assert!(uri && text.starts_with("file:///") && !text.contains('\n'));
        assert_eq!(gio::File::for_uri(&text).path().as_deref(), Some(raw));
    }

    #[test]
    fn compound_archive_sibling_retains_native_filename_mime_type() {
        let mime = |name: &str| gio::content_type_guess(Some(name), None::<&[u8]>).0;
        let original = mime("archive.tar.gz");
        assert_eq!(original, "application/x-compressed-tar");
        assert_ne!(mime("archive.tar (copy).gz"), original);
        assert_eq!(mime("archive (copy).tar.gz"), original);
        assert_eq!(mime("archive (copy 2).TAR.XZ"), mime("archive.TAR.XZ"));
    }

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
