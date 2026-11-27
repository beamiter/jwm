//! Deliberate move to Home Trash from the primary file pane.
use crate::{MutationOutcome, MutationWork, Shared, SignalOnDrop, copy_ui, trash_ui};
use gtk::{gio, glib, prelude::*};
use gtk4 as gtk;
use jwm_file_core::{
    Cancellation, DirectoryHandle, FileIdentity, FileKind, HomeTrashWriteError,
    HomeTrashWriteOutcome, RegularMoveSource, trash_regular_at_home,
};
use std::{
    ffi::OsString,
    io::ErrorKind,
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
};

pub(super) const OPERATION: &str = "移到主目录回收站";
pub(super) const TITLE: &str = "JWM Files · Move to Home Trash";

#[derive(Clone)]
struct Picked {
    parent: Arc<DirectoryHandle>,
    path: PathBuf,
    name: OsString,
    identity: FileIdentity,
}

pub(super) fn install(
    window: &gtk::ApplicationWindow,
    state: &Shared,
    selection: &gtk::SingleSelection,
    status: &gtk::Label,
) {
    let action = gio::SimpleAction::new("move-to-home-trash", None);
    let (weak_window, state, selection, status) = (
        window.downgrade(),
        state.clone(),
        selection.clone(),
        status.clone(),
    );
    action.connect_activate(move |_, _| {
        let Some(window) = weak_window.upgrade() else {
            return;
        };
        if crate::comparison::has_focus(&window) {
            copy_ui::notice(&status, "请从主文件列表选择一个普通文件。");
            return;
        }
        let picked = {
            let mut browser = state.borrow_mut();
            if browser.mutation.is_some() || browser.dialog_open || browser.pending.is_some() {
                copy_ui::notice(&status, "请等当前文件操作或目录加载完成。");
                return;
            }
            let Some(picked) = browser
                .parent
                .as_ref()
                .zip(browser.entries.get(selection.selected() as usize))
                .filter(|(_, row)| row.kind == FileKind::Regular)
                .and_then(|(parent, row)| {
                    Some(Picked {
                        parent: parent.clone(),
                        path: browser.path().clone(),
                        name: row.name.clone(),
                        identity: row.identity?,
                    })
                })
            else {
                copy_ui::notice(&status, "请选择一个普通文件，文件夹和链接暂不支持。");
                return;
            };
            if let Some(activation) = browser.activation.take() {
                activation.cancel.cancel();
            }
            browser.dialog_open = true;
            picked
        };
        let dialog = gtk::Dialog::builder()
            .title(TITLE)
            .transient_for(&window)
            .modal(true)
            .destroy_with_parent(true)
            .build();
        let cancel = dialog.add_button("取消", gtk::ResponseType::Cancel);
        dialog.add_button(OPERATION, gtk::ResponseType::Accept);
        dialog.set_default_response(gtk::ResponseType::Cancel);
        let displayed = trash_ui::display_path(picked.path.join(&picked.name).as_os_str());
        let text = gtk::Label::new(Some(&format!(
            "项目：{displayed}\n仅移入同一挂载点的主目录回收站，不会永久删除。"
        )));
        text.set_selectable(true);
        text.set_wrap(true);
        text.set_margin_start(16);
        text.set_margin_end(16);
        text.set_margin_top(12);
        text.set_margin_bottom(12);
        dialog.content_area().append(&text);
        dialog.connect_destroy({
            let state = state.clone();
            move |_| {
                let mut browser = state.borrow_mut();
                browser.dialog_open = false;
                browser.wake.signal();
            }
        });
        dialog.connect_response({
            let (state, status) = (state.clone(), status.clone());
            move |dialog, response| {
                dialog.destroy();
                if response == gtk::ResponseType::Accept {
                    dispatch(&state, &status, &picked);
                }
            }
        });
        dialog.present();
        cancel.grab_focus();
    });
    window.add_action(&action);
}

fn dispatch(state: &Shared, status: &gtk::Label, picked: &Picked) {
    let mut browser = state.borrow_mut();
    if browser.mutation.is_some()
        || browser.pending.is_some()
        || browser.path() != &picked.path
        || browser
            .parent
            .as_ref()
            .is_none_or(|parent| !Arc::ptr_eq(parent, &picked.parent))
    {
        copy_ui::notice(status, "目录已变化，未移动文件。");
        return;
    }
    let data_home = glib::user_data_dir();
    let current = picked.clone();
    let display = trash_ui::display_path(picked.path.join(&picked.name).as_os_str());
    let cancellation = Cancellation::default();
    let on_worker = cancellation.clone();
    let wake = browser.wake.clone();
    let (send, reply) = mpsc::sync_channel(1);
    match thread::Builder::new()
        .name("jwm-file-home-trash".into())
        .spawn(move || {
            let _notify = SignalOnDrop(wake);
            let result = glib::DateTime::now_local()
                .and_then(|date| date.format("%Y-%m-%dT%H:%M:%S"))
                .map_err(|_| "无法取得本地时间，未移动文件。".to_owned())
                .and_then(|date| {
                    trash_regular_at_home(
                        RegularMoveSource {
                            parent: &current.parent,
                            displayed_parent: &current.path,
                            name: &current.name,
                            identity: current.identity,
                        },
                        &data_home,
                        date.as_str(),
                        &on_worker,
                    )
                    .map_err(error_message)
                })
                .map(|outcome| match outcome {
                    HomeTrashWriteOutcome::Moved { .. } => MutationOutcome::TrashMove(None),
                    HomeTrashWriteOutcome::MovedUnverified { storage_key } => {
                        MutationOutcome::TrashMove(Some(storage_key))
                    }
                });
            let _ = send.send(result);
        }) {
        Ok(_) => {
            browser.mutation = Some(MutationWork {
                parent: picked.parent.clone(),
                path: picked.path.clone(),
                operation: OPERATION,
                reply,
                copy: Some((cancellation, display.clone())),
            });
            copy_ui::notice(status, &format!("正在移到回收站：{display}"));
        }
        Err(error) => copy_ui::notice(status, &format!("无法开始移到回收站：{error}")),
    }
}

fn error_message(error: HomeTrashWriteError) -> String {
    match error {
        HomeTrashWriteError::Refused(cause) if cause.kind() == ErrorKind::CrossesDevices => {
            "主目录回收站不在同一挂载点，未移动文件。".into()
        }
        HomeTrashWriteError::Refused(cause) if cause.kind() == ErrorKind::NotFound => {
            "文件或用户数据目录不存在，未移动文件。".into()
        }
        HomeTrashWriteError::Refused(cause) if cause.kind() == ErrorKind::PermissionDenied => {
            "源目录或回收站权限不符合要求，未移动文件。".into()
        }
        HomeTrashWriteError::Refused(cause) if cause.kind() == ErrorKind::Interrupted => {
            "已取消，未移动文件。".into()
        }
        HomeTrashWriteError::Refused(cause) => format!("未能移到回收站：{cause}"),
        HomeTrashWriteError::SidecarMayRemain { storage_key, cause } => {
            let name = trash_ui::display_path(&storage_key);
            format!("未移动文件，但回收站可能保留记录 {name}。请检查，勿直接重试。{cause}")
        }
    }
}
