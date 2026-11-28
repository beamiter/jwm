//! Explicit confirmation and exact retired-metadata cleanup.
use gtk::{glib, prelude::*};
use gtk4 as gtk;
use jwm_file_core::{ManagedRetiredCleanup, cleanup_managed_retired, select_managed_retired};
use std::{
    cell::{Cell, RefCell},
    io,
    rc::Rc,
};

pub(super) const BUTTON: &str = "清理已移出的日志";
pub(super) const TITLE: &str = "JWM Files · 清理复制日志";

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

pub(super) fn install(
    window: &gtk::Window,
    button: &gtk::Button,
    feedback: &gtk::Label,
    refresh: &gtk::Button,
    busy: &Rc<Cell<bool>>,
) {
    let (window, feedback, refresh, busy) = (
        window.downgrade(),
        feedback.downgrade(),
        refresh.downgrade(),
        busy.clone(),
    );
    button.connect_clicked(move |button| {
        let Some(label) = feedback.upgrade() else {
            return;
        };
        if busy.replace(true) {
            label.set_text("上一项日志操作尚未结束。");
            return;
        }
        let lease = Lease(busy.clone());
        label.set_text("正在核对待清理日志…");
        let (send, receive) = futures_channel::oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("jwm-prepare-retired-cleanup".into())
            .spawn(move || {
                let selected = super::state_root().and_then(|root| select_managed_retired(&root));
                let _ = send.send(selected);
            });
        if worker.is_err() {
            label.set_text("无法启动旧日志核对。");
            return;
        }
        let (window, feedback, refresh, button) = (
            window.clone(),
            feedback.clone(),
            refresh.clone(),
            button.downgrade(),
        );
        glib::spawn_future_local(async move {
            let result = receive.await;
            let Some(window) = window.upgrade().filter(|window| window.is_visible()) else {
                return;
            };
            let Some((feedback_label, source_button)) = feedback.upgrade().zip(button.upgrade())
            else {
                return;
            };
            if !source_button.is_visible() {
                feedback_label.set_text("列表已变化，请刷新后再核对。");
                return;
            }
            let Ok(Ok(selected)) = result else {
                feedback_label.set_text("没有可以安全清理的旧日志，请刷新。");
                return;
            };
            feedback_label.set_text("");
            let dialog = gtk::Dialog::builder()
                .title(TITLE)
                .transient_for(&window)
                .modal(true)
                .destroy_with_parent(true)
                .build();
            dialog.set_application(window.application().as_ref());
            let explanation = gtk::Label::new(Some(
                "只清理已从列表移出的日志，不会删除复制的文件或其他记录。",
            ));
            explanation.set_wrap(true);
            explanation.set_margin_top(14);
            explanation.set_margin_bottom(14);
            explanation.set_margin_start(16);
            explanation.set_margin_end(16);
            dialog.content_area().append(&explanation);
            dialog.add_button("取消", gtk::ResponseType::Cancel);
            dialog.add_button("确认清理", gtk::ResponseType::Accept);
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
                if !weak_window.upgrade().is_some_and(|window| window.is_visible()) {
                    return;
                }
                label.set_text("正在清理旧日志…");
                let (send, receive) = futures_channel::oneshot::channel();
                let worker = std::thread::Builder::new()
                    .name("jwm-clean-retired-journal".into())
                    .spawn(move || {
                        let clean = super::state_root()
                            .and_then(|root| cleanup_managed_retired(&root, selected));
                        let _ = send.send(clean);
                    });
                if worker.is_err() {
                    label.set_text("无法启动旧日志清理。");
                    return;
                }
                let (window, feedback, refresh) =
                    (weak_window.clone(), feedback.clone(), refresh.clone());
                glib::spawn_future_local(async move {
                    let result = receive.await;
                    drop(lease);
                    if !window.upgrade().is_some_and(|window| window.is_visible()) {
                        return;
                    }
                    if let Some(label) = feedback.upgrade() {
                        label.set_text(result.map_or("未确认清理结果，请刷新查看。", confirmed));
                    }
                    if let Some(refresh) = refresh.upgrade() {
                        if refresh.is_sensitive() {
                            refresh.emit_clicked();
                        }
                    }
                });
            });
            dialog.present();
        });
    });
}
