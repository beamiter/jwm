//! Explicit opt-in save and restore for one Files window's tabs.
use crate::{Shared, WINDOW_LIMIT, browser_window_count, copy_ui, open_window_with_tabs};
use crate::{saved_session::SavedTabs, tabs::Tabs};
use gtk4::{gio, glib, prelude::*};
use jwm_file_core::{open_files_state_root, read_files_tab_state, write_files_tab_state};
use std::{
    cell::Cell,
    io,
    path::{Path, PathBuf},
    rc::Rc,
};

thread_local! {
    // All windows in this GTK process commit to the same one-record store.
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

pub(super) fn install(
    app: &gtk4::Application,
    window: &gtk4::ApplicationWindow,
    state: &Shared,
    status: &gtk4::Label,
    home: &Path,
    special: &[(glib::UserDirectory, Option<PathBuf>)],
    copy_buffer: &copy_ui::CopyBuffer,
) {
    for (name, keys, save) in [
        ("save-tabs", "<Control><Alt>s", true),
        ("restore-tabs", "<Control><Alt>r", false),
    ] {
        let action = gio::SimpleAction::new(name, None);
        let weak_window = window.downgrade();
        let weak_status = status.downgrade();
        let weak_state = Rc::downgrade(state);
        let (home, special, copy_buffer) =
            (home.to_path_buf(), special.to_vec(), copy_buffer.clone());
        action.connect_activate(move |_, _| {
            let Some((window, status)) = weak_window.upgrade().zip(weak_status.upgrade()) else {
                return;
            };
            let Some(app) = window.application() else {
                return;
            };
            if BUSY.with(Cell::get) {
                status.set_text("另一个标签页操作尚未结束。");
                return;
            }
            let Some(state) = weak_state.upgrade() else {
                return;
            };
            let snapshot = if save {
                let browser = state.borrow();
                if browser.dialog_open || browser.pending.is_some() || browser.parent.is_none() {
                    status.set_text("目录尚未就绪，暂不能保存标签页。");
                    return;
                }
                Some(browser.tabs.snapshot())
            } else {
                if state.borrow().dialog_open {
                    return;
                }
                if browser_window_count(&app) >= WINDOW_LIMIT {
                    status.set_text("窗口已达上限，无法恢复标签页。");
                    return;
                }
                None
            };
            let state_home = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
            // Do not write to the launch cwd when HOME is missing.
            let real_home = std::env::var_os("HOME").map(PathBuf::from);
            let (send, receive) = futures_channel::oneshot::channel();
            let spawned = std::thread::Builder::new()
                .name("jwm-files-tab-state".into())
                .spawn(move || {
                    let result: io::Result<Option<SavedTabs>> = (|| {
                        let root = open_files_state_root(
                            real_home.as_deref(),
                            state_home.as_deref(),
                            save,
                        )?;
                        if let Some(snapshot) = snapshot {
                            write_files_tab_state(&root, &snapshot.encode()?)?;
                            Ok(None)
                        } else {
                            let bytes = read_files_tab_state(&root)?;
                            SavedTabs::decode(&bytes).map(Some)
                        }
                    })();
                    let _ = send.send(result);
                });
            if let Err(error) = spawned {
                status.set_text(&format!("无法启动标签页操作：{error}"));
                return;
            }
            BUSY.with(|flag| flag.set(true));
            let (weak_window, weak_status) = (weak_window.clone(), weak_status.clone());
            let (home, special, copy_buffer) = (home.clone(), special.clone(), copy_buffer.clone());
            let save_hold = save.then(|| app.hold());
            glib::spawn_future_local(async move {
                let _keep_app_alive_during_save = save_hold;
                let result = receive.await;
                BUSY.with(|flag| flag.set(false));
                let Some((window, status)) = weak_window.upgrade().zip(weak_status.upgrade())
                else {
                    return;
                };
                match result {
                    Ok(Ok(None)) => status.set_text("已保存此窗口的标签页。"),
                    Ok(Ok(Some(saved))) => {
                        let Some(app) = window.application() else {
                            return;
                        };
                        if browser_window_count(&app) >= WINDOW_LIMIT {
                            status.set_text("窗口已达上限，无法恢复标签页。");
                            return;
                        }
                        if let Some(tabs) = Tabs::from_saved(saved) {
                            open_window_with_tabs(&app, tabs, &home, &special, &copy_buffer, None);
                        }
                    }
                    Ok(Err(error)) if save => {
                        status.set_text(&format!("未确认保存标签页：{error}"))
                    }
                    Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => {
                        status.set_text("没有可恢复的已保存标签页。");
                    }
                    Ok(Err(error)) => status.set_text(&format!("无法恢复标签页：{error}")),
                    Err(_) => status.set_text("标签页操作未返回结果。"),
                }
            });
        });
        window.add_action(&action);
        app.set_accels_for_action(&format!("win.{name}"), &[keys]);
    }
}
