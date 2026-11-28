use super::Shared;
use futures_channel::oneshot;
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4::{gio, glib};
use jwm_file_core::{
    Cancellation, DirectoryHandle, FileIdentity, FileKind, ImagePreview, PreviewError, TextPreview,
};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    path::{Path, PathBuf},
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

enum PreviewPayload {
    Text(PreviewBody),
    Image(ImagePreview, &'static str),
}

struct PreviewBody {
    source: TextPreview,
    rendered: Option<String>,
    markdown: bool,
}

impl PreviewBody {
    fn from_text(source: TextPreview, name: &Path) -> Self {
        let markdown = name.extension().is_some_and(|ext| {
            ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown")
        });
        let rendered = markdown
            .then(|| jwm_markdown::pango_markup(&source.text, jwm_file_core::TEXT_PREVIEW_LIMIT))
            .flatten()
            .filter(|markup| gtk::pango::parse_markup(markup, '\0').is_ok());
        Self {
            source,
            rendered,
            markdown,
        }
    }

    fn label(&self, raw: bool) -> &'static str {
        match (
            self.markdown,
            self.rendered.is_some(),
            raw,
            self.source.truncated,
        ) {
            (_, _, _, true) => "大文件，仅显示开头（最多 128 KiB）。",
            (true, true, false, false) => "Markdown 预览",
            (true, true, true, false) => "Markdown 源码",
            (true, false, _, false) => "无法渲染 Markdown，显示源码。",
            (false, _, _, false) => "纯文本预览",
        }
    }
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
    text_scroll: glib::WeakRef<gtk::ScrolledWindow>,
    picture: glib::WeakRef<gtk::Picture>,
    source_toggle: glib::WeakRef<gtk::ToggleButton>,
    body: Rc<RefCell<Option<PreviewBody>>>,
}

impl View {
    fn paint_body(&self, show_source: bool) {
        let body = self.body.borrow();
        let (Some(body), Some(notice), Some(text)) =
            (body.as_ref(), self.notice.upgrade(), self.text.upgrade())
        else {
            return;
        };
        let markup = body.rendered.as_deref().filter(|_| !show_source);
        let buffer = text.buffer();
        text.set_monospace(markup.is_none());
        notice.set_text(body.label(show_source));
        if let Some(markup) = markup {
            buffer.set_text("");
            buffer.insert_markup(&mut buffer.start_iter(), markup);
        } else {
            buffer.set_text(&body.source.text);
        }
    }
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
        view.body.take();
        if let Some(toggle) = view.source_toggle.upgrade() {
            toggle.set_visible(false);
            toggle.set_active(false);
        }
        if let Some(notice) = view.notice.upgrade() {
            notice.set_text("正在读取预览…");
        }
        if let Some(picture) = view.picture.upgrade() {
            picture.set_paintable(None::<&gtk::gdk::Paintable>);
            picture.set_visible(false);
        }
        if let Some(scroll) = view.text_scroll.upgrade() {
            scroll.set_visible(true);
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
                let path = Path::new(&target.name);
                let ext = path.extension();
                let result = if ext.is_some_and(|ext| ext.eq_ignore_ascii_case("png")) {
                    target
                        .parent
                        .read_png_preview(
                            &target.path,
                            &target.name,
                            target.identity,
                            &worker_cancel,
                        )
                        .map(|image| PreviewPayload::Image(image, "PNG"))
                } else if ext.is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg")
                }) {
                    target
                        .parent
                        .read_jpeg_preview(
                            &target.path,
                            &target.name,
                            target.identity,
                            &worker_cancel,
                        )
                        .map(|image| PreviewPayload::Image(image, "JPEG"))
                } else {
                    target
                        .parent
                        .read_text_preview(
                            &target.path,
                            &target.name,
                            target.identity,
                            &worker_cancel,
                        )
                        .map(|source| PreviewPayload::Text(PreviewBody::from_text(source, path)))
                };
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

    fn finish(&self, id: u64, result: Result<PreviewPayload, PreviewError>) {
        // Completion may belong to a closed window or to an earlier selection.
        // It can release the worker slot but must not touch the current view.
        if !self.generation.accepts(id) {
            return;
        }
        let Some(view) = self.current.borrow().clone() else {
            return;
        };
        let Some(notice) = view.notice.upgrade() else {
            return;
        };
        match result {
            Ok(PreviewPayload::Text(result)) => {
                if let Some(toggle) = view.source_toggle.upgrade() {
                    toggle.set_visible(result.rendered.is_some());
                }
                view.body.replace(Some(result));
                view.paint_body(false);
            }
            Ok(PreviewPayload::Image(image, format_name)) => {
                if let (Some(picture), Some(scroll)) =
                    (view.picture.upgrade(), view.text_scroll.upgrade())
                {
                    let description =
                        format!("{format_name} 图片 · {} × {}", image.width, image.height);
                    let texture = gtk::gdk::MemoryTexture::new(
                        image.width as i32,
                        image.height as i32,
                        gtk::gdk::MemoryFormat::R8g8b8a8,
                        &glib::Bytes::from_owned(image.rgba),
                        image.width as usize * 4,
                    );
                    picture.set_paintable(Some(&texture));
                    picture.set_alternative_text(Some(&description));
                    notice.set_text(&description);
                    scroll.set_visible(false);
                    picture.set_visible(true);
                }
            }
            Err(PreviewError::NotStaticPng) => notice.set_text("不是有效的静态 PNG，无法预览。"),
            Err(PreviewError::NotJpeg) => notice.set_text("不是受支持的 JPEG，无法预览。"),
            Err(PreviewError::ImageLimit) => notice.set_text("图片超过快速预览的大小限制。"),
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
        let source_toggle = gtk::ToggleButton::with_label("查看源码");
        source_toggle.set_halign(gtk::Align::Start);
        source_toggle.set_margin_start(12);
        source_toggle.set_visible(false);
        layout.append(&source_toggle);
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
        let picture = gtk::Picture::builder()
            .hexpand(true)
            .vexpand(true)
            .can_shrink(true)
            .visible(false)
            .build();
        layout.append(&picture);
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
        let source_choice = source_toggle.downgrade();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let focused_toggle = source_choice
                .upgrade()
                .is_some_and(|button| button.is_mapped() && button.has_focus());
            // Let GTK activate the focused control instead of closing the preview.
            let dismiss = key == gtk::gdk::Key::Escape
                || (key == gtk::gdk::Key::space && !focused_toggle);
            if dismiss
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
            text_scroll: scroll.downgrade(),
            picture: picture.downgrade(),
            source_toggle: source_toggle.downgrade(),
            body: Rc::default(),
        };
        source_toggle.connect_toggled({
            let view = view.clone();
            move |button| view.paint_body(button.is_active())
        });
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
    fn markdown_preview_formats_only_markdown_and_keeps_raw_source() {
        let raw = "# 标题\n\n**粗体** [link](https://invalid.example)";
        let source = || TextPreview {
            text: raw.to_owned(),
            truncated: false,
        };
        let markdown = PreviewBody::from_text(source(), Path::new("README.MD"));
        let markup = markdown.rendered.as_deref().unwrap();
        assert!(markup.contains("<b>粗体</b>"));
        assert!(!markup.contains("https://"));
        assert_eq!(markdown.source.text, raw);
        assert_eq!(markdown.label(false), "Markdown 预览");
        assert_eq!(markdown.label(true), "Markdown 源码");
        let tsx = PreviewBody::from_text(source(), Path::new("example.tsx"));
        assert!(tsx.rendered.is_none());
        assert_eq!(tsx.source.text, raw);
    }

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
