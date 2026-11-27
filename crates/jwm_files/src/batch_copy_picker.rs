//! Explicit multi-selection without changing ordinary single-row shortcuts.
use crate::{
    Shared,
    copy_ui::{BatchSource, CopyBuffer, StoredCopy, notice},
    row_model::FileRowModel,
    trash_ui::display_path,
};
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use jwm_file_core::{COPY_BATCH_LIMIT, FileEntry, FileIdentity, FileKind};
use std::{ffi::OsString, rc::Rc, sync::Arc};

const TITLE: &str = "JWM Files · 选择多个文件复制";

fn selected_files(
    model: &gtk::MultiSelection,
    rows: &[FileEntry],
) -> Option<Vec<(OsString, FileIdentity)>> {
    let bits = model.selection();
    let count = u32::try_from(bits.size()).ok()?;
    if !(1..=COPY_BATCH_LIMIT as u32).contains(&count) {
        return None;
    }
    (0..count)
        .map(|i| {
            let row = rows.get(bits.nth(i) as usize)?;
            if row.kind != FileKind::Regular {
                return None;
            }
            Some((row.name.clone(), row.identity?))
        })
        .collect()
}

pub(super) fn install(
    window: &gtk::ApplicationWindow,
    state: &Shared,
    buffer: &CopyBuffer,
    status: &gtk::Label,
) {
    let action = gio::SimpleAction::new("pick-batch-copy", None);
    let (window_ref, status_ref) = (window.downgrade(), status.downgrade());
    let state_ref = Rc::downgrade(state);
    let buffer = buffer.clone();
    action.connect_activate(move |_, _| {
        let (Some(window), Some(state), Some(status)) = (
            window_ref.upgrade(),
            state_ref.upgrade(),
            status_ref.upgrade(),
        ) else {
            return;
        };
        show(&window, &state, &buffer, &status);
    });
    window.add_action(&action);
}

fn show(
    parent_window: &gtk::ApplicationWindow,
    state: &Shared,
    buffer: &CopyBuffer,
    status: &gtk::Label,
) {
    let Some(application) = parent_window.application() else {
        return;
    };
    let (path, held, snapshot) = {
        let mut browser = state.borrow_mut();
        if browser.pending.is_some() || browser.mutation.is_some() || browser.dialog_open {
            return;
        }
        let Some(held) = browser.parent.clone() else {
            notice(status, "请先打开要复制文件的本地目录。");
            return;
        };
        browser.dialog_open = true;
        (browser.path().clone(), held, browser.entries.clone())
    };
    let panel = gtk::Window::builder()
        .application(&application)
        .transient_for(parent_window)
        .destroy_with_parent(true)
        .modal(true)
        .title(TITLE)
        .default_width(600)
        .default_height(440)
        .build();
    let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
    column.set_margin_start(16);
    column.set_margin_end(16);
    column.set_margin_top(12);
    column.set_margin_bottom(12);
    let location = gtk::Label::new(Some(&display_path(path.as_os_str())));
    location.set_xalign(0.0);
    location.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    column.append(&location);
    let hint = gtk::Label::new(Some(
        "用 Ctrl 或 Shift 选择普通文件。粘贴时会跳过同名项目。",
    ));
    hint.set_xalign(0.0);
    hint.set_wrap(true);
    column.append(&hint);

    let model = FileRowModel::new();
    model.replace(snapshot.clone());
    let selection = gtk::MultiSelection::new(Some(model));
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, object| {
        if let Some(item) = object.downcast_ref::<gtk::ListItem>() {
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_margin_start(8);
            label.set_margin_end(8);
            label.set_margin_top(5);
            label.set_margin_bottom(5);
            item.set_child(Some(&label));
        }
    });
    factory.connect_bind({
        let snapshot = snapshot.clone();
        move |_, object| {
            let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(label) = item.child().and_downcast::<gtk::Label>() else {
                return;
            };
            let Some(row) = snapshot.get(item.position() as usize) else {
                label.set_text("");
                return;
            };
            let prefix = match row.kind {
                FileKind::Directory => "▸ ",
                FileKind::Symlink => "↪ ",
                _ => "",
            };
            label.set_text(&format!("{prefix}{}", display_path(&row.name)));
        }
    });
    let list = gtk::ListView::new(Some(selection.clone()), Some(factory));
    list.set_widget_name("jwm-files-batch-selection");
    column.append(
        &gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&list)
            .build(),
    );
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.set_halign(gtk::Align::End);
    let confirm = gtk::Button::with_label("选定复制");
    confirm.set_widget_name("jwm-files-batch-confirm");
    confirm.set_sensitive(false);
    footer.append(&confirm);
    let close = gtk::Button::with_label("取消");
    footer.append(&close);
    column.append(&footer);
    panel.set_child(Some(&column));
    {
        let (confirm, hint) = (confirm.downgrade(), hint.downgrade());
        let snapshot = snapshot.clone();
        selection.connect_selection_changed(move |selection, _, _| {
            let count = selection.selection().size();
            let valid = selected_files(selection, &snapshot).is_some();
            if let Some(confirm) = confirm.upgrade() {
                confirm.set_sensitive(valid);
            }
            if let Some(hint) = hint.upgrade() {
                hint.set_text(&if valid || count == 0 {
                    format!("已选 {count} 项。粘贴时会跳过同名项目。")
                } else {
                    format!("已选 {count} 项。请选择 1 至 {COPY_BATCH_LIMIT} 个普通文件。")
                });
            }
        });
    }

    let panel_ref = panel.downgrade();
    close.connect_clicked(move |_| {
        if let Some(panel) = panel_ref.upgrade() {
            panel.close();
        }
    });
    let key = gtk::EventControllerKey::new();
    let panel_ref = panel.downgrade();
    key.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape
            && let Some(panel) = panel_ref.upgrade()
        {
            panel.close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    panel.add_controller(key);

    let state_ref = Rc::downgrade(state);
    panel.connect_close_request(move |_| {
        if let Some(state) = state_ref.upgrade() {
            let mut browser = state.borrow_mut();
            browser.dialog_open = false;
            browser.wake.signal();
        }
        glib::Propagation::Proceed
    });
    let state_ref = Rc::downgrade(state);
    let status = status.downgrade();
    let panel_ref = panel.downgrade();
    let buffer = buffer.clone();
    confirm.connect_clicked(move |_| {
        let (Some(state), Some(status), Some(panel)) =
            (state_ref.upgrade(), status.upgrade(), panel_ref.upgrade())
        else {
            return;
        };
        let current = {
            let browser = state.borrow();
            browser.dialog_open
                && browser.pending.is_none()
                && browser.mutation.is_none()
                && browser.path() == &path
                && Arc::ptr_eq(&browser.entries, &snapshot)
                && browser
                    .parent
                    .as_ref()
                    .is_some_and(|current| current.identity() == held.identity())
        };
        if !current {
            buffer.replace(None);
            notice(&status, "源目录已变化，未选定复制项目。请重新选择。");
            panel.close();
            return;
        }
        let Some(files) = selected_files(&selection, &snapshot) else {
            return;
        };
        let count = files.len();
        buffer.replace(Some(StoredCopy::Batch(BatchSource {
            parent: held.clone(),
            path: path.clone(),
            files,
        })));
        notice(
            &status,
            &format!("已选定复制 {count} 个文件。请到目标目录粘贴。"),
        );
        panel.close();
    });
    panel.present();
    list.grab_focus();
}
