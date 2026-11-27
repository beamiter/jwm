//! Presentation for a completed worker response, never for a lost worker.
use crate::{Shared, copy_ui::notice};
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use jwm_file_core::CopyBatchItem;
use std::{ffi::OsString, rc::Rc};

#[derive(Clone)]
pub(super) struct BatchCopyReport {
    pub(super) summary: String,
    pub(super) details: String,
    pub(super) has_publication: bool,
}

impl BatchCopyReport {
    pub(super) fn from_items(items: Vec<(OsString, CopyBatchItem)>) -> Self {
        let (mut published, mut existing, mut cancelled, mut failed, mut not_started) =
            (0, 0, 0, 0, 0);
        let details = items
            .into_iter()
            .map(|(name, status)| {
                let result = match status {
                    CopyBatchItem::Published(_) => {
                        published += 1;
                        "已复制".into()
                    }
                    CopyBatchItem::ExistingTarget => {
                        existing += 1;
                        "目标同名，未覆盖".into()
                    }
                    CopyBatchItem::CancelledBeforePublication => {
                        cancelled += 1;
                        "已取消，此项未写入".into()
                    }
                    CopyBatchItem::Failed(error) => {
                        failed += 1;
                        crate::copy_ui::copy_error_message(error)
                    }
                    CopyBatchItem::NotStarted => {
                        not_started += 1;
                        "未开始".into()
                    }
                };
                format!(
                    "{}：{result}",
                    crate::trash_ui::display_path(name.as_os_str())
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let summary = format!(
            "批量复制：已复制 {published}，同名跳过 {existing}，已取消 {cancelled}，失败 {failed}，未开始 {not_started}。"
        );
        Self {
            summary,
            details,
            has_publication: published != 0,
        }
    }
}

pub(super) const TITLE: &str = "JWM Files · 批量复制结果";

pub(super) fn install(window: &gtk::ApplicationWindow, state: &Shared, status: &gtk::Label) {
    let action = gio::SimpleAction::new("batch-copy-result", None);
    let (window_ref, status_ref) = (window.downgrade(), status.downgrade());
    let state_ref = Rc::downgrade(state);
    let dialog_ref = glib::WeakRef::<gtk::Window>::new();
    action.connect_activate(move |_, _| {
        if let Some(dialog) = dialog_ref.upgrade()
            && dialog.is_visible()
        {
            dialog.present();
            return;
        }
        let (Some(window), Some(state), Some(status)) = (
            window_ref.upgrade(),
            state_ref.upgrade(),
            status_ref.upgrade(),
        ) else {
            return;
        };
        let report = {
            let state = state.borrow();
            if state.dialog_open {
                return;
            }
            state.last_copy_batch.clone()
        };
        let Some(report) = report else {
            notice(&status, "当前窗口没有可查看的批量复制结果。");
            return;
        };
        let Some(application) = window.application() else {
            return;
        };
        let panel = gtk::Window::builder()
            .application(&application)
            .transient_for(&window)
            .destroy_with_parent(true)
            .title(TITLE)
            .default_width(620)
            .default_height(350)
            .build();
        let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
        column.set_margin_start(16);
        column.set_margin_end(16);
        column.set_margin_top(12);
        column.set_margin_bottom(12);
        let summary = gtk::Label::new(Some(&report.summary));
        summary.set_xalign(0.0);
        summary.set_wrap(true);
        column.append(&summary);
        let details = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .build();
        details.set_widget_name("jwm-files-batch-report");
        details.buffer().set_text(&report.details);
        column.append(
            &gtk::ScrolledWindow::builder()
                .vexpand(true)
                .child(&details)
                .build(),
        );
        let close = gtk::Button::with_label("关闭");
        close.set_halign(gtk::Align::End);
        let panel_ref = panel.downgrade();
        close.connect_clicked(move |_| {
            if let Some(panel) = panel_ref.upgrade() {
                panel.close();
            }
        });
        column.append(&close);
        panel.set_child(Some(&column));
        dialog_ref.set(Some(&panel));
        panel.present();
    });
    window.add_action(&action);
}

#[cfg(test)]
mod tests {
    use super::*;
    use jwm_file_core::CopyReceipt;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn mixed_result_reports_cancel_without_negating_published_items() {
        let report = BatchCopyReport::from_items(vec![
            (
                "文档".into(),
                CopyBatchItem::Published(CopyReceipt { bytes: 5 }),
            ),
            ("同名".into(), CopyBatchItem::ExistingTarget),
            (
                OsString::from_vec(b"raw%\x80".to_vec()),
                CopyBatchItem::CancelledBeforePublication,
            ),
            ("第四项".into(), CopyBatchItem::NotStarted),
        ]);
        assert!(report.has_publication);
        assert_eq!(
            report.summary,
            "批量复制：已复制 1，同名跳过 1，已取消 1，失败 0，未开始 1。"
        );
        let lines = report.details.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "文档：已复制");
        assert_eq!(lines[1], "同名：目标同名，未覆盖");
        assert_eq!(lines[2], "raw%\\x80：已取消，此项未写入");
        assert_eq!(lines[3], "第四项：未开始");
    }
}
