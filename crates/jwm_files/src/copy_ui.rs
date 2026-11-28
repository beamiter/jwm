use super::{MutationOutcome, MutationWork, Shared, SignalOnDrop};
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use jwm_file_core::{
    Cancellation, CopyDestination, CopyError, CopyEvidence, CopySource, DirectoryHandle,
    FileIdentity, FileKind, FolderMoveError, FolderMoveOutcome, FolderMoveSource, ListError,
    RegularMoveError, RegularMoveOutcome, RegularMoveSource, TrackedCopyError, TrackedCopyReceipt,
    TreeCopyError, TreeCopyPlanError, TreeCopyTargetError, copy_tree_no_replace,
    move_folder_no_replace, move_regular_no_replace, plan_tree_copy,
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
    pub(super) parent: Arc<DirectoryHandle>,
    pub(super) path: PathBuf,
    pub(super) name: OsString,
    pub(super) identity: FileIdentity,
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
    Cut(StashedCopy, MoveKind),
    Batch(BatchSource),
}

#[derive(Clone, Copy)]
pub(super) enum MoveKind {
    Regular,
    Folder,
}

/// Sources are shared by this application instance, never the text clipboard.
pub(super) type CopyBuffer = Rc<RefCell<Option<StoredCopy>>>;

pub(super) fn notice(label: &gtk::Label, text: &str) {
    label.set_text(text);
    label.set_visible(!text.is_empty());
}

#[derive(Clone, Copy)]
enum SelectionIntent {
    Copy,
    Cut,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PasteMode {
    Main,
    Sibling,
    ComparisonCopy,
    Managed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyPolicy {
    NoReplace,
    KeepBoth,
    Managed,
}

fn stash(
    state: &Shared,
    buffer: &CopyBuffer,
    selected: u32,
    status: &gtk::Label,
    intent: SelectionIntent,
) {
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
        let message = match intent {
            SelectionIntent::Cut => "请选择可剪切的普通文件或文件夹。",
            SelectionIntent::Copy => "请选择可复制的普通文件或文件夹。",
        };
        notice(status, message);
        return;
    };
    let path = candidate.path.join(&candidate.name);
    let display = crate::trash_ui::display_path(path.as_os_str());
    if matches!(intent, SelectionIntent::Cut) {
        notice(status, &format!("已剪切：{display}。粘贴后才会移动。"));
        let kind = if kind == FileKind::Directory {
            MoveKind::Folder
        } else {
            MoveKind::Regular
        };
        buffer.replace(Some(StoredCopy::Cut(candidate, kind)));
    } else if kind == FileKind::Directory {
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

fn paste(state: &Shared, buffer: &CopyBuffer, status: &gtk::Label, mode: PasteMode) {
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
    paste_with_mode(state, buffer, status, parent, path, mode);
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
    paste_with_mode(
        state,
        buffer,
        status,
        parent,
        path,
        PasteMode::ComparisonCopy,
    );
}

fn paste_with_mode(
    state: &Shared,
    buffer: &CopyBuffer,
    status: &gtk::Label,
    parent: Arc<DirectoryHandle>,
    path: PathBuf,
    mode: PasteMode,
) {
    let browser = state.borrow();
    if browser.mutation.is_some() || browser.dialog_open {
        notice(status, "有文件操作正在进行，请稍候。");
        return;
    }
    let Some(source) = buffer.borrow().clone() else {
        notice(status, "请先复制或剪切要粘贴的项目。");
        return;
    };
    if mode == PasteMode::Managed && !matches!(&source, StoredCopy::Single(_)) {
        notice(
            status,
            "保留复制记录仅支持单个普通文件，不会执行剪切或批量操作。",
        );
        return;
    }
    let source = match source {
        StoredCopy::Single(source) => source,
        StoredCopy::Cut(source, kind) => {
            drop(browser);
            if mode != PasteMode::Main {
                notice(status, "剪切只支持粘贴到主文件列表，不支持粘贴为副本。");
            } else if parent.identity() == source.parent.identity() {
                notice(status, "请选择不同的目标目录。");
            } else {
                // Consume on dispatch, before any other Files window can start
                // using the same cut. A failed move requires a fresh selection.
                buffer.replace(None);
                paste_move(state, status, parent, path, source, kind);
            }
            return;
        }
        StoredCopy::Tree(source) => {
            drop(browser);
            if mode == PasteMode::Sibling {
                notice(status, "文件夹复制不支持自动改名，请使用 Ctrl+V。");
            } else {
                dispatch_tree_copy(state, status, parent, path, source);
            }
            return;
        }
        StoredCopy::Batch(source) => {
            drop(browser);
            if mode == PasteMode::Sibling {
                notice(status, "批量复制仅支持跳过同名项目，请使用 Ctrl+V。");
            } else {
                paste_batch(state, status, parent, path, source);
            }
            return;
        }
    };
    drop(browser);
    let policy = match mode {
        PasteMode::Managed => CopyPolicy::Managed,
        PasteMode::Sibling => CopyPolicy::KeepBoth,
        PasteMode::Main | PasteMode::ComparisonCopy => CopyPolicy::NoReplace,
    };
    let _ = dispatch_regular_copy_with_policy(state, status, parent, path, source, policy);
}

/// True indicates worker dispatch, not a disk result. A future internal
/// drop may acknowledge the gesture here, but only the worker publishes.
pub(super) fn dispatch_regular_copy(
    state: &Shared,
    status: &gtk::Label,
    parent: Arc<DirectoryHandle>,
    path: PathBuf,
    source: StashedCopy,
    keep_both: bool,
) -> bool {
    let policy = if keep_both {
        CopyPolicy::KeepBoth
    } else {
        CopyPolicy::NoReplace
    };
    dispatch_regular_copy_with_policy(state, status, parent, path, source, policy)
}

fn dispatch_regular_copy_with_policy(
    state: &Shared,
    status: &gtk::Label,
    parent: Arc<DirectoryHandle>,
    path: PathBuf,
    source: StashedCopy,
    policy: CopyPolicy,
) -> bool {
    let mut browser = state.borrow_mut();
    if browser.mutation.is_some() || browser.dialog_open {
        notice(status, "有文件操作正在进行，请稍候。");
        return false;
    }
    if policy != CopyPolicy::KeepBoth && parent.identity() == source.parent.identity() {
        notice(status, "请选择不同的目标目录。同名项目不会被覆盖。");
        return false;
    }
    let (target, target_path) = (parent.clone(), path.clone());
    let cancellation = Cancellation::default();
    let on_worker = cancellation.clone();
    let label = format!(
        "{} → {}",
        crate::trash_ui::display_path(source.path.join(&source.name).as_os_str()),
        crate::trash_ui::display_path(path.as_os_str())
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
            let destination = CopyDestination {
                displayed_parent: &target_path,
                name: &source.name,
            };
            let result = match policy {
                CopyPolicy::KeepBoth => target
                    .copy_regular_keep_both(from, &target_path, &on_worker)
                    .map(|receipt| MutationOutcome::CopyAsSibling(receipt.name))
                    .map_err(copy_error_message),
                CopyPolicy::NoReplace => target
                    .copy_regular_no_replace(from, destination, &on_worker)
                    .map(|_| MutationOutcome::FileOperation)
                    .map_err(copy_error_message),
                CopyPolicy::Managed => managed_copy(&target, from, destination, &on_worker)
                    .map(MutationOutcome::ManagedCopy),
            };
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

fn managed_copy(
    target: &DirectoryHandle,
    source: CopySource<'_>,
    destination: CopyDestination<'_>,
    cancel: &Cancellation,
) -> Result<TrackedCopyReceipt, String> {
    if cancel.check().is_err() {
        return Err(copy_error_message(CopyError::Cancelled));
    }
    let state = crate::copy_journal_ui::open_state(true)
        .map_err(|_| String::from("无法打开私有复制记录，目标未写入。"))?;
    target
        .copy_regular_managed(source, destination, &state, cancel)
        .map_err(|error| match error {
            TrackedCopyError::Intent(error) if error.kind() == std::io::ErrorKind::StorageFull => {
                "复制记录已满。请先移出旧记录；目标未写入。".into()
            }
            TrackedCopyError::Intent(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                "另有复制日志操作正在进行，目标未写入。".into()
            }
            TrackedCopyError::Intent(_) => "无法保存复制开始记录，目标未写入。".into(),
            TrackedCopyError::Copy { error, intent_slot } => {
                let mut message = copy_error_message(error);
                if let Some(slot) = intent_slot {
                    message.push_str(&format!(
                        " 已留下开始记录 {:02}，可在「查看复制记录」核对。",
                        slot + 1
                    ));
                }
                message
            }
        })
}

pub(super) fn managed_copy_notice(receipt: TrackedCopyReceipt) -> String {
    let slot = receipt.slot + 1;
    match receipt.evidence {
        CopyEvidence::Recorded => format!("已复制并保存记录 {slot:02}。"),
        CopyEvidence::Unattributed | CopyEvidence::RecordUnavailable(_) => {
            format!("已写入目标，但记录 {slot:02} 未能确认结果。请检查目标，不要直接重试。")
        }
    }
}

pub(super) const MOVE_OPERATION: &str = "移动文件";

fn move_error(error: RegularMoveError) -> String {
    match error {
        RegularMoveError::InvalidName => "文件名无效，未移动。".into(),
        RegularMoveError::SameDirectory => "源和目标是同一个目录，未移动。".into(),
        RegularMoveError::ParentChanged => "源或目标目录已变化，未移动。".into(),
        RegularMoveError::ParentNotPrivate => {
            "只支持你拥有、且未向组或其他用户开放写入的源和目标目录。".into()
        }
        RegularMoveError::SourceChanged | RegularMoveError::NotRegular => {
            "源文件已变化或不再是普通文件，未移动。".into()
        }
        RegularMoveError::DestinationExists => "目标已有同名项目，未覆盖，源文件未移动。".into(),
        RegularMoveError::CrossMount => "暂不支持跨挂载点移动，源文件未移动。".into(),
        RegularMoveError::Cancelled => "已取消，源文件未移动。".into(),
        RegularMoveError::Io(error) => format!("无法移动文件：{error}"),
    }
}

fn folder_move_error(error: FolderMoveError) -> String {
    match error {
        FolderMoveError::Guard(RegularMoveError::SourceChanged) | FolderMoveError::NotDirectory => {
            "源文件夹已变化或不再是普通文件夹，未移动。".into()
        }
        FolderMoveError::Guard(RegularMoveError::DestinationExists) => {
            "目标已有同名项目，未覆盖，源文件夹未移动。".into()
        }
        FolderMoveError::Guard(RegularMoveError::Cancelled)
        | FolderMoveError::Plan(TreeCopyPlanError::Walk(ListError::Cancelled))
        | FolderMoveError::Target(TreeCopyTargetError::Plan(TreeCopyPlanError::Walk(
            ListError::Cancelled,
        ))) => "已取消，源文件夹未移动。".into(),
        FolderMoveError::Guard(RegularMoveError::CrossMount)
        | FolderMoveError::MountIdentityUnavailable(_) => {
            "未能确认源和目标在同一挂载点，源文件夹未移动。".into()
        }
        FolderMoveError::Guard(RegularMoveError::Io(error)) => {
            format!("无法移动文件夹：{error}")
        }
        FolderMoveError::Guard(other) => move_error(other),
        FolderMoveError::SourceNotPrivate => {
            "只支持你拥有、且未向组或其他用户开放写入的源文件夹。".into()
        }
        FolderMoveError::Target(TreeCopyTargetError::TargetInsideSource) => {
            "不能将文件夹移动到自身或子目录。".into()
        }
        FolderMoveError::Plan(TreeCopyPlanError::NestedMount)
        | FolderMoveError::Target(TreeCopyTargetError::Plan(TreeCopyPlanError::NestedMount)) => {
            "源文件夹内有其他挂载点，未移动。".into()
        }
        FolderMoveError::Plan(TreeCopyPlanError::Unsupported(_))
        | FolderMoveError::Target(TreeCopyTargetError::Plan(TreeCopyPlanError::Unsupported(_))) => {
            "源文件夹含链接或特殊文件，未移动。".into()
        }
        FolderMoveError::Plan(_) | FolderMoveError::Target(_) => {
            "无法完整核对源文件夹或目标目录，未移动。".into()
        }
    }
}

fn paste_move(
    state: &Shared,
    status: &gtk::Label,
    parent: Arc<DirectoryHandle>,
    path: PathBuf,
    source: StashedCopy,
    kind: MoveKind,
) {
    let mut browser = state.borrow_mut();
    let detail = format!(
        "{} → {}",
        crate::trash_ui::display_path(source.path.join(&source.name).as_os_str()),
        crate::trash_ui::display_path(path.join(&source.name).as_os_str())
    );
    let (target, target_path) = (parent.clone(), path.clone());
    let cancellation = Cancellation::default();
    let on_worker = cancellation.clone();
    let wake = browser.wake.clone();
    let (send, reply) = mpsc::sync_channel(1);
    match thread::Builder::new()
        .name("jwm-files-move".into())
        .spawn(move || {
            let _notify = SignalOnDrop(wake);
            #[cfg(test)]
            let _test_gate = MOVE_TEST_GATE.lock().unwrap();
            let result = match kind {
                MoveKind::Regular => move_regular_no_replace(
                    RegularMoveSource {
                        parent: &source.parent,
                        displayed_parent: &source.path,
                        name: &source.name,
                        identity: source.identity,
                    },
                    &target,
                    &target_path,
                    &on_worker,
                )
                .map(|outcome| match outcome {
                    RegularMoveOutcome::Moved => MutationOutcome::FileOperation,
                    RegularMoveOutcome::MovedUnverified => MutationOutcome::MoveUnverified,
                })
                .map_err(move_error),
                MoveKind::Folder => move_folder_no_replace(
                    FolderMoveSource {
                        parent: &source.parent,
                        displayed_parent: &source.path,
                        name: &source.name,
                        identity: source.identity,
                    },
                    &target,
                    &target_path,
                    &on_worker,
                )
                .map(|outcome| match outcome {
                    FolderMoveOutcome::Moved => MutationOutcome::FileOperation,
                    FolderMoveOutcome::MovedUnverified => MutationOutcome::MoveUnverified,
                })
                .map_err(folder_move_error),
            };
            let _ = send.send(result);
        }) {
        Ok(_) => {
            browser.mutation = Some(MutationWork {
                parent,
                path,
                operation: MOVE_OPERATION,
                reply,
                copy: Some((cancellation, detail.clone())),
            });
            notice(status, &format!("正在移动：{detail}"));
        }
        Err(error) => notice(status, &format!("无法启动文件移动：{error}")),
    }
}

#[cfg(test)]
pub(super) static MOVE_TEST_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

/// Acknowledges only a worker dispatch, not publication of a copied tree.
pub(super) fn dispatch_tree_copy(
    state: &Shared,
    status: &gtk::Label,
    parent: Arc<DirectoryHandle>,
    path: PathBuf,
    source: StashedCopy,
) -> bool {
    let mut browser = state.borrow_mut();
    if browser.mutation.is_some() || browser.dialog_open {
        notice(status, "有文件操作正在进行，请稍候。");
        return false;
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
            if !DirectoryHandle::open(&source.path)
                .is_ok_and(|current| current.identity() == source.parent.identity())
            {
                let _ = send.send(Err("源目录已变化，未写入目标文件夹。".into()));
                return;
            }
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
            true
        }
        Err(error) => {
            notice(status, &format!("无法启动文件夹复制：{error}"));
            false
        }
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
    primary: &gtk::Stack,
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
    menu.append(
        Some(crate::trash_move_ui::OPERATION),
        Some("win.move-to-home-trash"),
    );
    menu.append(Some("撤销重命名（Ctrl+Z）"), Some("win.undo-rename"));
    menu.append(Some("选择多个文件复制"), Some("win.pick-batch-copy"));
    crate::batch_copy_picker::install(window, state, buffer, status);
    menu.append(Some("查看批量复制结果"), Some("win.batch-copy-result"));
    menu.append(Some("查看复制记录"), Some("win.copy-journal"));
    crate::batch_copy_result::install(window, state, status);
    for (name, label) in [
        ("copy-file", "复制选中项目（Ctrl+C）"),
        ("cut-file", "剪切选中文件或文件夹（Ctrl+X）"),
        ("copy-item-path", "复制完整路径（Ctrl+Shift+C）"),
        ("paste-file", "粘贴到当前目录（Ctrl+V）"),
        ("paste-as-copy", "粘贴为副本（Ctrl+Shift+V）"),
        ("paste-with-record", "粘贴并保留复制记录"),
        ("cancel-file-copy", "取消正在进行的复制或移动"),
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
            "copy-file" => stash(
                &state,
                &buffer,
                selection.selected(),
                &status,
                SelectionIntent::Copy,
            ),
            "cut-file" => stash(
                &state,
                &buffer,
                selection.selected(),
                &status,
                SelectionIntent::Cut,
            ),
            "copy-item-path" => {
                if let Some(window) = weak_window.upgrade() {
                    copy_path(&window, &state, selection.selected(), &status);
                }
            }
            "paste-file" => paste(&state, &buffer, &status, PasteMode::Main),
            "paste-as-copy" => paste(&state, &buffer, &status, PasteMode::Sibling),
            "paste-with-record" => paste(&state, &buffer, &status, PasteMode::Managed),
            _ => {
                let browser = state.borrow();
                if let Some(work) = &browser.mutation
                    && let Some((cancel, _)) = &work.copy
                {
                    cancel.cancel();
                    let text = if work.operation == MOVE_OPERATION
                        || work.operation == crate::trash_move_ui::OPERATION
                    {
                        "正在取消移动…"
                    } else {
                        "正在取消复制…"
                    };
                    notice(&status, text);
                }
            }
        });
        window.add_action(&action);
        menu.append(Some(label), Some(&format!("win.{name}")));
    }
    menu_button.set_menu_model(Some(&menu));

    let keys = gtk::EventControllerKey::new();
    #[cfg(test)]
    keys.set_property("name", "jwm-files-file-shortcuts");
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak_window = window.downgrade();
    let primary = primary.downgrade();
    let buffer = buffer.clone();
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
            (Key::x, false) => "win.cut-file",
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
        let focus_guarded_move = action == "win.cut-file"
            || (action == "win.paste-file"
                && matches!(buffer.borrow().as_ref(), Some(StoredCopy::Cut(..))));
        let primary = primary.upgrade();
        let mut in_primary = false;
        let mut focused = gtk::prelude::GtkWindowExt::focus(&window);
        while let Some(widget) = focused {
            if widget.is::<gtk::Editable>() || widget.is::<gtk::TextView>() {
                return glib::Propagation::Proceed;
            }
            in_primary |= primary
                .as_ref()
                .is_some_and(|pane| pane.upcast_ref::<gtk::Widget>() == &widget);
            focused = widget.parent();
        }
        if focus_guarded_move && !in_primary {
            return glib::Propagation::Proceed;
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
