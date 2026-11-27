use super::{MutationOutcome, MutationWork, Shared, SignalOnDrop};
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use jwm_file_core::{
    Cancellation, CopyDestination, CopyError, CopySource, DirectoryHandle, FileIdentity, FileKind,
    ListError, TreeCopyError, TreeCopyPlanError, TreeCopyTargetError, copy_tree_no_replace,
    plan_tree_copy,
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

#[derive(Clone)]
pub(super) struct BatchSource {
    pub(super) parent: Arc<DirectoryHandle>,
    pub(super) path: PathBuf,
    pub(super) files: Vec<(OsString, FileIdentity)>,
}

#[derive(Clone)]
pub(super) enum StoredCopy {
    Single(StashedCopy),
    Tree(StashedCopy),
    Batch(BatchSource),
}

/// Sources are shared by this application instance, never the text clipboard.
pub(super) type CopyBuffer = Rc<RefCell<Option<StoredCopy>>>;

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
        .filter(|(_, entry)| matches!(entry.kind, FileKind::Regular | FileKind::Directory))
        .and_then(|(parent, entry)| {
            let source = StashedCopy {
                parent: parent.clone(),
                path: browser.path().clone(),
                name: entry.name.clone(),
                identity: entry.identity?,
            };
            Some((entry.kind, source))
        });
    let Some((kind, candidate)) = candidate else {
        // Refuse to leave an earlier, hidden choice armed after a failed copy.
        buffer.replace(None);
        notice(status, "请选择可复制的普通文件或文件夹。");
        return;
    };
    let path = candidate.path.join(&candidate.name);
    let display = crate::trash_ui::display_path(path.as_os_str());
    if kind == FileKind::Directory {
        notice(status, &format!("已选定复制文件夹：{display}"));
        buffer.replace(Some(StoredCopy::Tree(candidate)));
    } else {
        notice(status, &format!("已选定复制：{display}"));
        buffer.replace(Some(StoredCopy::Single(candidate)));
    }
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

pub(super) fn copy_error_message(error: CopyError) -> String {
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
        notice(status, "请先选择要复制的文件或文件夹。");
        return;
    };
    let source = match source {
        StoredCopy::Single(source) => source,
        StoredCopy::Tree(source) => {
            drop(browser);
            if keep_both {
                notice(status, "文件夹复制不支持自动改名，请使用 Ctrl+V。");
            } else {
                paste_tree(state, status, parent, path, source);
            }
            return;
        }
        StoredCopy::Batch(source) => {
            drop(browser);
            if keep_both {
                notice(status, "批量复制仅支持跳过同名项目，请使用 Ctrl+V。");
            } else {
                paste_batch(state, status, parent, path, source);
            }
            return;
        }
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

fn plan_error(error: TreeCopyPlanError) -> String {
    match error {
        TreeCopyPlanError::Walk(ListError::Cancelled) => "已取消，未发布目标文件夹。".into(),
        TreeCopyPlanError::Walk(ListError::ReadDirectory { .. }) => {
            "无法读取源文件夹，或源文件夹已变化。".into()
        }
        TreeCopyPlanError::Incomplete | TreeCopyPlanError::PathBudget => {
            "目录扫描不完整或超出限制，未发布目标文件夹。".into()
        }
        TreeCopyPlanError::NestedMount => "源文件夹内有挂载点，未发布目标文件夹。".into(),
        TreeCopyPlanError::Unsupported(path) => format!(
            "源文件夹含有链接或特殊文件，未复制：{}",
            crate::trash_ui::display_path(path.as_os_str())
        ),
    }
}

fn tree_error(error: TreeCopyError, target: &std::path::Path) -> String {
    match error {
        TreeCopyError::InvalidName => "目标名称无效。".into(),
        TreeCopyError::TargetNotPrivate => {
            "整目录复制只支持你拥有、且未向组或其他用户开放写入的目标目录。".into()
        }
        TreeCopyError::Target(TreeCopyTargetError::TargetInsideSource) => {
            "不能将文件夹复制到自身或子目录。".into()
        }
        TreeCopyError::Target(
            TreeCopyTargetError::TargetUnavailable(_) | TreeCopyTargetError::TargetChanged,
        ) => "目标文件夹已变化或无法读取。".into(),
        TreeCopyError::Target(TreeCopyTargetError::SourceChanged)
        | TreeCopyError::SourceChanged => "源文件夹或内容已变化，未发布目标文件夹。".into(),
        TreeCopyError::Target(TreeCopyTargetError::Plan(error)) => plan_error(error),
        TreeCopyError::DestinationExists => "目标已有同名项目，未覆盖。".into(),
        TreeCopyError::Cancelled => "已取消，未发布目标文件夹。".into(),
        TreeCopyError::StagingChanged => "临时文件夹发生变化，未发布目标文件夹。".into(),
        TreeCopyError::ByteBudget => "文件夹内容超过 1 GiB，未复制。".into(),
        TreeCopyError::File(error) => format!("未发布目标文件夹：{}", copy_error_message(error)),
        TreeCopyError::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            "无法访问复制所需的目录或文件，请检查访问权限。".into()
        }
        TreeCopyError::Io(error) => format!("无法复制文件夹：{error}"),
        TreeCopyError::CleanupIncomplete {
            cause,
            staging_name,
        } => format!(
            "{} 临时目录可能保留：{}",
            tree_error(*cause, target),
            crate::trash_ui::display_path(target.join(staging_name).as_os_str())
        ),
    }
}

#[cfg(test)]
pub(super) static TREE_COPY_TEST_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn paste_tree(
    state: &Shared,
    status: &gtk::Label,
    parent: Arc<DirectoryHandle>,
    path: PathBuf,
    source: StashedCopy,
) {
    let mut browser = state.borrow_mut();
    if browser.mutation.is_some() || browser.dialog_open {
        return;
    }
    let source_root = source.path.join(&source.name);
    let detail = format!(
        "{} → {}",
        crate::trash_ui::display_path(source_root.as_os_str()),
        crate::trash_ui::display_path(path.as_os_str())
    );
    let (target, target_path) = (parent.clone(), path.clone());
    let cancellation = Cancellation::default();
    let on_worker = cancellation.clone();
    let wake = browser.wake.clone();
    let (send, reply) = mpsc::sync_channel(1);
    match thread::Builder::new()
        .name("jwm-files-copy-tree".into())
        .spawn(move || {
            let _notify = SignalOnDrop(wake);
            #[cfg(test)]
            let _test_gate = TREE_COPY_TEST_GATE.lock().unwrap();
            let result = plan_tree_copy(&source_root, source.identity, &on_worker)
                .map_err(plan_error)
                .and_then(|plan| {
                    copy_tree_no_replace(&plan, &target, &target_path, &source.name, &on_worker)
                        .map(MutationOutcome::TreeCopy)
                        .map_err(|error| tree_error(error, &target_path))
                })
                .map_err(|error| {
                    let destination = crate::trash_ui::display_path(target_path.as_os_str());
                    format!("{error} 目标：{destination}")
                });
            let _ = send.send(result);
        }) {
        Ok(_) => {
            browser.mutation = Some(MutationWork {
                parent,
                path,
                operation: "复制文件夹",
                reply,
                copy: Some((cancellation, detail.clone())),
            });
            notice(status, &format!("正在复制文件夹：{detail}"));
        }
        Err(error) => notice(status, &format!("无法启动文件夹复制：{error}")),
    }
}

fn paste_batch(
    state: &Shared,
    status: &gtk::Label,
    parent: Arc<DirectoryHandle>,
    path: PathBuf,
    source: BatchSource,
) {
    let mut browser = state.borrow_mut();
    if browser.mutation.is_some() || browser.dialog_open {
        return;
    }
    if parent.identity() == source.parent.identity() {
        notice(status, "请选择不同的目标目录。同名项目不会被覆盖。");
        return;
    }
    let detail = format!(
        "{} 个文件 → {}",
        source.files.len(),
        crate::trash_ui::display_path(path.as_os_str())
    );
    let target = parent.clone();
    let target_path = path.clone();
    let cancel = Cancellation::default();
    let on_worker = cancel.clone();
    let wake = browser.wake.clone();
    let (send, reply) = mpsc::sync_channel(1);
    match thread::Builder::new()
        .name("jwm-files-copy-batch".into())
        .spawn(move || {
            let _notify = SignalOnDrop(wake);
            let sources = source
                .files
                .iter()
                .map(|(name, identity)| CopySource {
                    parent: &source.parent,
                    displayed_parent: &source.path,
                    name,
                    expected_file: *identity,
                })
                .collect::<Vec<_>>();
            let result = target
                .copy_regular_batch_no_replace(&sources, &target_path, &on_worker)
                .map(|results| {
                    MutationOutcome::BatchCopy(
                        source
                            .files
                            .iter()
                            .map(|(name, _)| name.clone())
                            .zip(results)
                            .collect(),
                    )
                })
                .map_err(|error| format!("批量复制未开始：{error:?}"));
            let _ = send.send(result);
        }) {
        Ok(_) => {
            browser.last_copy_batch = None;
            browser.mutation = Some(MutationWork {
                parent,
                path,
                operation: "批量复制",
                reply,
                copy: Some((cancel, detail.clone())),
            });
            notice(status, &format!("正在批量复制：{detail}"));
        }
        Err(error) => notice(status, &format!("无法启动批量复制：{error}")),
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
    menu.append(Some("查找子目录文件名"), Some("win.search-subdirectories"));
    menu.append(Some("在此处打开终端"), Some("win.folder-terminal"));
    menu.append(Some("目录对照栏（F3）"), Some("win.comparison-pane"));
    menu.append(Some("快速预览（空格）"), Some("win.quick-preview"));
    menu.append(Some("属性（Alt+Enter）"), Some("win.item-properties"));
    menu.append(Some("主目录回收站"), Some("win.home-trash"));
    menu.append(Some("撤销重命名（Ctrl+Z）"), Some("win.undo-rename"));
    menu.append(Some("选择多个文件复制"), Some("win.pick-batch-copy"));
    crate::batch_copy_picker::install(window, state, buffer, status);
    menu.append(Some("查看批量复制结果"), Some("win.batch-copy-result"));
    crate::batch_copy_result::install(window, state, status);
    for (name, label) in [
        ("copy-file", "复制选中项目（Ctrl+C）"),
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
