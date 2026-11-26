use super::Shared;
use futures_channel::oneshot;
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use jwm_file_core::{
    Cancellation, DirectoryHandle, FileIdentity, FileKind, PreviewError, TextPreview,
};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    thread,
};

struct Target {
    parent: Arc<DirectoryHandle>,
    path: PathBuf,
    name: OsString,
    identity: FileIdentity,
}

struct Request {
    id: u64,
    target: Target,
}

#[derive(Default)]
struct Generation {
    latest: Cell<u64>,
    requested: Cell<Option<u64>>,
}

impl Generation {
    fn next(&self) -> u64 {
        let next = self
            .latest
            .get()
            .checked_add(1)
            .expect("preview generation exhausted");
        self.latest.set(next);
        self.requested.set(Some(next));
        next
    }

    fn accepts(&self, id: u64) -> bool {
        self.requested.get() == Some(id)
    }

    fn clear(&self) {
        self.requested.set(None);
    }
}

#[derive(Clone)]
struct View {
    window: glib::WeakRef<gtk::Window>,
    heading: glib::WeakRef<gtk::Label>,
    notice: glib::WeakRef<gtk::Label>,
    text: glib::WeakRef<gtk::TextView>,
}

#[derive(Default)]
struct PreviewState {
    generation: Generation,
    current: RefCell<Option<View>>,
    running: RefCell<Option<Cancellation>>,
    pending: RefCell<Option<Request>>,
}

impl PreviewState {
    fn cancel_window(&self, window: &gtk::Window) {
        let same = self
            .current
            .borrow()
            .as_ref()
            .and_then(|view| view.window.upgrade())
            .as_ref()
            == Some(window);
        if same {
            self.reset();
        }
    }

    fn reset(&self) {
        self.current.take();
        self.generation.clear();
        self.pending.take();
        if let Some(cancel) = self.running.borrow().as_ref() {
            cancel.cancel();
        }
    }

    fn show(self: &Rc<Self>, parent: &gtk::ApplicationWindow, target: Target) {
        let view = self.current.borrow().clone();
        let (view, window) = match view.and_then(|view| {
            let window = view.window.upgrade()?;
            window.is_visible().then_some((view, window))
        }) {
            Some(current) => current,
            None => self.build_view(parent),
        };
        let id = self.generation.next();
        if let Some(cancel) = self.running.borrow().as_ref() {
            cancel.cancel();
        }
        let source = target
            .path
            .join(&target.name)
            .to_string_lossy()
            .escape_debug()
            .to_string();
        if let Some(heading) = view.heading.upgrade() {
            heading.set_text(&source);
            heading.set_tooltip_text(Some(&source));
        }
        if let Some(notice) = view.notice.upgrade() {
            notice.set_text("正在读取预览…");
        }
        if let Some(text) = view.text.upgrade() {
            text.buffer().set_text("");
        }
        self.pending.replace(Some(Request { id, target }));
        window.present();
        self.start_pending();
    }

    fn start_pending(self: &Rc<Self>) {
        if self.running.borrow().is_some() {
            return;
        }
        let Some(Request { id, target }) = self.pending.take() else {
            return;
        };
        let cancel = Cancellation::default();
        let worker_cancel = cancel.clone();
        let (send, receive) = oneshot::channel();
        if let Err(error) = thread::Builder::new()
            .name("jwm-files-preview".into())
            .spawn(move || {
                let result = target.parent.read_text_preview(
                    &target.path,
                    &target.name,
                    target.identity,
                    &worker_cancel,
                );
                let _ = send.send(result);
            })
        {
            self.finish(id, Err(PreviewError::Io(error)));
            return;
        }
        self.running.replace(Some(cancel));
        let state = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let result = receive.await.unwrap_or_else(|_| {
                Err(PreviewError::Io(std::io::Error::other(
                    "preview worker exited",
                )))
            });
            state.running.take();
            state.finish(id, result);
            state.start_pending();
        });
    }

    fn finish(&self, id: u64, result: Result<TextPreview, PreviewError>) {
        // Completion may belong to a closed window or to an earlier selection.
        // It can release the worker slot but must not touch the current view.
        if !self.generation.accepts(id) {
            return;
        }
        let Some(view) = self.current.borrow().clone() else {
            return;
        };
        let (Some(notice), Some(text)) = (view.notice.upgrade(), view.text.upgrade()) else {
            return;
        };
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
                notice.set_text("此文件不支持纯文本预览。");
            }
            Err(error) => notice.set_text(&format!("无法预览：{error}")),
        }
    }

    fn build_view(self: &Rc<Self>, parent: &gtk::ApplicationWindow) -> (View, gtk::Window) {
        let window = gtk::Window::builder()
            .title("JWM Files · Preview")
            .transient_for(parent)
            .destroy_with_parent(true)
            .default_width(680)
            .default_height(440)
            .build();
        window.set_application(parent.application().as_ref());
        let layout = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let heading = gtk::Label::new(None);
        heading.set_xalign(0.0);
        heading.set_selectable(true);
        heading.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        heading.set_margin_start(12);
        heading.set_margin_end(12);
        heading.set_margin_top(8);
        layout.append(&heading);
        let notice = gtk::Label::new(None);
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
            let state = self.clone();
            move |window| state.cancel_window(window)
        });
        window.connect_close_request({
            let state = self.clone();
            move |window| {
                state.cancel_window(window);
                glib::Propagation::Proceed
            }
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
        let view = View {
            window: window.downgrade(),
            heading: heading.downgrade(),
            notice: notice.downgrade(),
            text: text.downgrade(),
        };
        self.current.replace(Some(view.clone()));
        (view, window)
    }
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
    parent.connect_destroy({
        let preview = preview.clone();
        move |_| preview.reset()
    });
    action.connect_activate(move |_, _| {
        let Some(parent) = weak_parent.upgrade() else {
            return;
        };
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
                    Some(Target {
                        parent: handle.clone(),
                        path: browser.path().clone(),
                        name: entry.name.clone(),
                        identity: entry.identity?,
                    })
                })
        };
        let Some(target) = target else {
            status.set_text("请选择一个可预览的普通文件。");
            return;
        };
        preview.show(&parent, target);
    });
    parent.add_action(&action);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_generation_rejects_superseded_and_closed_requests() {
        let generation = Generation::default();
        let old = generation.next();
        assert!(generation.accepts(old));
        let latest = generation.next();
        assert!(!generation.accepts(old));
        assert!(generation.accepts(latest));
        generation.clear();
        assert!(!generation.accepts(latest));
        let reopened = generation.next();
        assert!(!generation.accepts(latest));
        assert!(generation.accepts(reopened));
    }
}
