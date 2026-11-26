use super::Shared;
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use jwm_file_core::{Cancellation, FileKind, PreviewError};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::mpsc,
    thread,
    time::Duration,
};

#[derive(Default)]
struct PreviewState {
    busy: Cell<bool>,
    open: RefCell<Option<glib::WeakRef<gtk::Window>>>,
}

pub(super) fn install(
    parent: &gtk::ApplicationWindow,
    state: &Shared,
    selection: &gtk::SingleSelection,
    status: &gtk::Label,
) {
    let action = gio::SimpleAction::new("quick-preview", None);
    let weak_parent = parent.downgrade();
    let (state, selection, status) = (state.clone(), selection.clone(), status.clone());
    let preview = Rc::new(PreviewState::default());
    action.connect_activate(move |_, _| {
        let Some(parent) = weak_parent.upgrade() else {
            return;
        };
        if let Some(window) = preview
            .open
            .borrow()
            .as_ref()
            .and_then(glib::WeakRef::upgrade)
            && window.is_visible()
        {
            window.present();
            return;
        }
        if preview.busy.get() {
            status.set_text("上一项预览仍在停止，请稍候。");
            return;
        }
        let target = {
            let browser = state.borrow();
            if browser.pending.is_some() {
                return;
            }
            browser
                .parent
                .as_ref()
                .zip(browser.entries.get(selection.selected() as usize))
                .filter(|(_, entry)| entry.kind == FileKind::Regular)
                .and_then(|(handle, entry)| {
                    Some((
                        handle.clone(),
                        browser.path().clone(),
                        entry.name.clone(),
                        entry.identity?,
                    ))
                })
        };
        let Some((handle, path, name, identity)) = target else {
            status.set_text("请选择一个可预览的普通文件。");
            return;
        };
        let title = name.to_string_lossy().escape_debug().to_string();
        let cancel = Cancellation::default();
        let worker_cancel = cancel.clone();
        let (send, receive) = mpsc::sync_channel(1);
        if let Err(error) = thread::Builder::new()
            .name("jwm-files-preview".into())
            .spawn(move || {
                let _ = send.send(handle.read_text_preview(&path, &name, identity, &worker_cancel));
            })
        {
            status.set_text(&format!("无法启动预览：{error}"));
            return;
        }
        preview.busy.set(true);
        let window = gtk::Window::builder()
            .title("JWM Files · Preview")
            .transient_for(&parent)
            .destroy_with_parent(true)
            .default_width(680)
            .default_height(440)
            .build();
        window.set_application(parent.application().as_ref());
        let layout = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let heading = gtk::Label::new(Some(&title));
        heading.set_xalign(0.0);
        heading.set_selectable(true);
        heading.set_ellipsize(gtk::pango::EllipsizeMode::End);
        heading.set_margin_start(12);
        heading.set_margin_top(8);
        layout.append(&heading);
        let notice = gtk::Label::new(Some("正在读取预览…"));
        notice.set_xalign(0.0);
        notice.set_margin_start(12);
        notice.set_wrap(true);
        layout.append(&notice);
        let text = gtk::TextView::builder()
            .monospace(true)
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(12)
            .right_margin(12)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .child(&text)
            .build();
        layout.append(&scroll);
        window.set_child(Some(&layout));
        window.connect_destroy({
            let cancel = cancel.clone();
            move |_| cancel.cancel()
        });
        window.connect_close_request(move |_| {
            cancel.cancel();
            glib::Propagation::Proceed
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = window.downgrade();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            if (key == gtk::gdk::Key::Escape || key == gtk::gdk::Key::space)
                && !modifiers.intersects(
                    gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::ALT_MASK,
                )
                && let Some(window) = weak.upgrade()
            {
                window.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.add_controller(keys);
        *preview.open.borrow_mut() = Some(window.downgrade());
        let preview = preview.clone();
        let (weak_notice, weak_text) = (notice.downgrade(), text.downgrade());
        glib::timeout_add_local(Duration::from_millis(16), move || {
            let result = match receive.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => Err(PreviewError::Io(
                    std::io::Error::other("preview worker exited"),
                )),
            };
            preview.busy.set(false);
            if let (Some(notice), Some(text)) = (weak_notice.upgrade(), weak_text.upgrade()) {
                match result {
                    Ok(result) => {
                        notice.set_text(if result.truncated {
                            "大文件，仅显示开头（最多 128 KiB）。"
                        } else {
                            "纯文本预览"
                        });
                        text.buffer().set_text(&result.text);
                    }
                    Err(PreviewError::NotText | PreviewError::NotRegular) => {
                        notice.set_text("此文件不支持纯文本预览。")
                    }
                    Err(error) => notice.set_text(&format!("无法预览：{error}")),
                }
            }
            glib::ControlFlow::Break
        });
        window.present();
    });
    parent.add_action(&action);
}
