//! Explicit confirmation for changes to managed copy-journal metadata.
use gtk::{glib, prelude::*};
use gtk4 as gtk;
use jwm_file_core::{
    CopyJournalRecord, CopyJournalState, ManagedCopyJournalSelection, ManagedJournalRetirement,
    ManagedRetiredCleanup, ManagedRetiredSelection, cleanup_managed_retired,
    retire_managed_copy_journal, select_managed_retired, select_matching_managed_copy_journal,
};
use std::{
    cell::{Cell, RefCell},
    io,
    rc::Rc,
};

pub(super) const BUTTON: &str = "清理已移出的日志";
const TITLE: &str = "JWM Files · 清理复制日志";
pub(super) const RETIRE_BUTTON: &str = "移出记录";
const RETIRE_TITLE: &str = "JWM Files · 移出复制记录";

#[derive(Clone)]
pub(super) enum Action {
    Cleanup,
    Retire(u8, CopyJournalRecord),
}

enum Selection {
    Cleanup(ManagedRetiredSelection),
    Retire(ManagedCopyJournalSelection),
}

impl Action {
    fn select(&self) -> io::Result<Selection> {
        let root = super::state_root()?;
        match self {
            Self::Cleanup => select_managed_retired(&root).map(Selection::Cleanup),
            Self::Retire(slot, record) => {
                select_matching_managed_copy_journal(&root, *slot, record).map(Selection::Retire)
            }
        }
    }

    fn explanation(&self) -> String {
        match self {
            Self::Cleanup => "只清理已从列表移出的日志，不会删除复制的文件或其他记录。".into(),
            Self::Retire(slot, record) => {
                let destination = record.destination_parent.join(&record.name);
                let state = match record.state {
                    CopyJournalState::Prepared => "复制结果未知",
                    CopyJournalState::Published(_) => "有保存的写入记录",
                };
                format!(
                    "移出记录 {:02}（{state}）？\n{}\n只移动日志，不会删除或撤销复制的文件。日志可稍后单独清理。",
                    *slot + 1,
                    crate::trash_ui::display_path(destination.as_os_str())
                )
            }
        }
    }
}

impl Selection {
    fn apply(self) -> &'static str {
        let root = super::state_root();
        match self {
            Self::Cleanup(selected) => {
                #[cfg(test)]
                super::refresh_test_gate::before_cleanup();
                confirmed(root.and_then(|root| cleanup_managed_retired(&root, selected)))
            }
            Self::Retire(selected) => {
                match root.and_then(|root| retire_managed_copy_journal(&root, selected)) {
                    Ok(ManagedJournalRetirement::Retired) => "已移出记录，复制的文件未改变。",
                    Ok(ManagedJournalRetirement::RetiredUnverified(_)) => {
                        "记录已移出，但未能确认保存结果，请刷新查看。"
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        "有复制日志操作正在进行，未移出记录。"
                    }
                    Err(_) => "记录已变化或有旧日志待清理，未移出记录。",
                }
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct Controls {
    window: glib::WeakRef<gtk::Window>,
    feedback: glib::WeakRef<gtk::Label>,
    refresh: glib::WeakRef<gtk::Button>,
    busy: Rc<Cell<bool>>,
    refresh_after_read: Rc<Cell<bool>>,
}

struct Lease(Rc<Cell<bool>>);

impl Drop for Lease {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

fn confirmed(result: io::Result<ManagedRetiredCleanup>) -> &'static str {
    match result {
        Ok(ManagedRetiredCleanup::Cleaned) => "已清理旧日志，复制的文件未改变。",
        Ok(ManagedRetiredCleanup::Incomplete(_)) => "旧日志清理未完成；复制的文件不受影响。",
        Ok(ManagedRetiredCleanup::RemovalUnverified(_)) => {
            "旧日志已移除，但未能确认保存结果，请刷新查看。"
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            "有复制日志操作正在进行，未清理。"
        }
        Err(_) => "旧日志已变化或无法安全清理，未启动清理。",
    }
}

impl Controls {
    pub(super) fn new(
        window: &gtk::Window,
        feedback: &gtk::Label,
        refresh: &gtk::Button,
        busy: Rc<Cell<bool>>,
        refresh_after_read: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            window: window.downgrade(),
            feedback: feedback.downgrade(),
            refresh: refresh.downgrade(),
            busy,
            refresh_after_read,
        }
    }

    pub(super) fn install(&self, button: &gtk::Button, action: Action) {
        let Self {
            window,
            feedback,
            refresh,
            busy,
            refresh_after_read,
        } = self.clone();
        button.connect_clicked(move |button| {
            let Some(label) = feedback.upgrade() else {
                return;
            };
            if busy.replace(true) {
                label.set_text("上一项日志操作尚未结束。");
                return;
            }
            let lease = Lease(busy.clone());
            if refresh.upgrade().is_some_and(|button| !button.is_sensitive()) {
                label.set_text("列表正在刷新，请稍后再试。");
                return;
            }
            label.set_text("正在核对复制日志…");
            let (send, receive) = futures_channel::oneshot::channel();
            let worker_action = action.clone();
            let worker = std::thread::Builder::new()
                .name("jwm-select-copy-journal-action".into())
                .spawn(move || {
                    let _ = send.send(worker_action.select());
                });
            if worker.is_err() {
                label.set_text("无法启动日志核对。");
                return;
            }
            let (window, feedback, refresh, button) = (
                window.clone(),
                feedback.clone(),
                refresh.clone(),
                button.downgrade(),
            );
            let refresh_after_read = refresh_after_read.clone();
            let action = action.clone();
            glib::spawn_future_local(async move {
                let result = receive.await;
                let Some(window) = window.upgrade().filter(|window| window.is_visible()) else {
                    return;
                };
                let Some((feedback_label, source_button)) = feedback.upgrade().zip(button.upgrade())
                else {
                    return;
                };
                if !source_button.is_visible()
                    || !source_button.root().is_some_and(|root| {
                        root.downcast::<gtk::Window>().is_ok_and(|root| root == window)
                    })
                    || refresh.upgrade().is_some_and(|button| !button.is_sensitive())
                {
                    feedback_label.set_text("列表已变化，请刷新后再核对。");
                    return;
                }
                let Ok(Ok(selected)) = result else {
                    let message = match action {
                        Action::Cleanup => "没有可以安全清理的旧日志，请刷新。",
                        Action::Retire(_, _) => "所选日志已变化或无法核对，请刷新。",
                    };
                    feedback_label.set_text(message);
                    return;
                };
                feedback_label.set_text("");
                let (title, accept) = match action {
                    Action::Cleanup => (TITLE, "确认清理"),
                    Action::Retire(_, _) => (RETIRE_TITLE, "确认移出"),
                };
                let dialog = gtk::Dialog::builder()
                    .title(title)
                    .transient_for(&window)
                    .modal(true)
                    .destroy_with_parent(true)
                    .build();
                dialog.set_application(window.application().as_ref());
                let explanation = gtk::Label::new(Some(&action.explanation()));
                explanation.set_wrap(true);
                explanation.set_margin_top(14);
                explanation.set_margin_bottom(14);
                explanation.set_margin_start(16);
                explanation.set_margin_end(16);
                dialog.content_area().append(&explanation);
                dialog.add_button("取消", gtk::ResponseType::Cancel);
                dialog.add_button(accept, gtk::ResponseType::Accept);
                dialog.set_default_response(gtk::ResponseType::Cancel);
                if let Some(cancel) = dialog.widget_for_response(gtk::ResponseType::Cancel) {
                    cancel.grab_focus();
                }
                let held = Rc::new(RefCell::new(Some((lease, selected))));
                dialog.connect_destroy({
                    let held = held.clone();
                    move |_| {
                        held.borrow_mut().take();
                    }
                });
                let weak_window = window.downgrade();
                dialog.connect_response(move |dialog, response| {
                    let captured = held.borrow_mut().take();
                    dialog.close();
                    let Some((lease, selected)) = captured else {
                        return;
                    };
                    let Some(label) = feedback.upgrade() else {
                        return;
                    };
                    if response != gtk::ResponseType::Accept {
                        label.set_text("已取消，日志未改变。");
                        return;
                    }
                    if !weak_window
                        .upgrade()
                        .is_some_and(|window| window.is_visible())
                    {
                        return;
                    }
                    label.set_text("正在更新复制日志…");
                    let (send, receive) = futures_channel::oneshot::channel();
                    let worker = std::thread::Builder::new()
                        .name("jwm-apply-copy-journal-action".into())
                        .spawn(move || {
                            let _ = send.send(selected.apply());
                        });
                    if worker.is_err() {
                        label.set_text("无法启动日志操作。");
                        return;
                    }
                    let (window, feedback, refresh) =
                        (weak_window.clone(), feedback.clone(), refresh.clone());
                    let refresh_after_read = refresh_after_read.clone();
                    glib::spawn_future_local(async move {
                        let result = receive.await;
                        drop(lease);
                        if !window.upgrade().is_some_and(|window| window.is_visible()) {
                            return;
                        }
                        if let Some(label) = feedback.upgrade() {
                            label.set_text(result.unwrap_or("未确认日志操作结果，请刷新查看。"));
                        }
                        if let Some(refresh) = refresh.upgrade() {
                            if refresh.is_sensitive() {
                                refresh.emit_clicked();
                            } else {
                                refresh_after_read.set(true);
                            }
                        }
                    });
                });
                dialog.present();
            });
        });
    }
}
