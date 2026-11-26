use super::{MutationWork, Shared, SignalOnDrop};
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use jwm_file_core::{
    Cancellation, CopyDestination, CopyError, CopySource, DirectoryHandle, FileIdentity, FileKind,
};
use std::{
    ffi::OsString,
    path::PathBuf,
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

pub(super) fn notice(label: &gtk::Label, text: &str) {
    label.set_text(text);
    label.set_visible(!text.is_empty());
}

fn stash(state: &Shared, selected: u32, status: &gtk::Label) {
    let mut browser = state.borrow_mut();
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
                path: browser.path.clone(),
                name: entry.name.clone(),
                identity: entry.identity?,
            })
        });
    let Some(candidate) = candidate else {
        // Refuse to leave an earlier, hidden choice armed after a failed copy.
        browser.copy_item = None;
        notice(status, "请选择一个可复制的普通文件。");
        return;
    };
    notice(
        status,
        &format!(
            "已选定复制：{}",
            candidate.name.to_string_lossy().escape_debug()
        ),
    );
    browser.copy_item = Some(candidate);
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

fn paste(state: &Shared, status: &gtk::Label) {
    let mut browser = state.borrow_mut();
    if browser.mutation.is_some() || browser.dialog_open {
        notice(status, "有文件操作正在进行，请稍候。");
        return;
    }
    let Some(source) = browser.copy_item.clone() else {
        notice(status, "请先选择要复制的普通文件。");
        return;
    };
    let Some(parent) = browser.parent.clone().filter(|_| browser.pending.is_none()) else {
        notice(status, "请先打开目标目录。");
        return;
    };
    if parent.identity() == source.parent.identity() {
        notice(status, "请选择不同的目标目录。同名项目不会被覆盖。");
        return;
    }
    let path = browser.path.clone();
    let (target, target_path) = (parent.clone(), path.clone());
    let cancellation = Cancellation::default();
    let on_worker = cancellation.clone();
    let label = format!(
        "{} → {}",
        source.name.to_string_lossy().escape_debug(),
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
                .map(|_| ())
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
        }
        Err(error) => notice(status, &format!("无法启动复制：{error}")),
    }
}

pub(super) fn install(
    window: &gtk::ApplicationWindow,
    menu_button: &gtk::MenuButton,
    state: &Shared,
    selection: &gtk::SingleSelection,
    status: &gtk::Label,
) {
    let menu = gio::Menu::new();
    menu.append(Some("快速预览（空格）"), Some("win.quick-preview"));
    for (name, label) in [
        ("copy-file", "复制选中文件（Ctrl+C）"),
        ("paste-file", "粘贴到当前目录（Ctrl+V）"),
        ("cancel-file-copy", "取消正在进行的复制"),
    ] {
        let action = gio::SimpleAction::new(name, None);
        let (state, selection, status) = (state.clone(), selection.clone(), status.clone());
        action.connect_activate(move |action, _| match action.name().as_str() {
            "copy-file" => stash(&state, selection.selected(), &status),
            "paste-file" => paste(&state, &status),
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
