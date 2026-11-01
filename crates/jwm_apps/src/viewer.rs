use crate::{
    action, error, local_path, neighbors,
    ui::{self, Icon, button},
};
use gtk::{gdk, gdk_pixbuf, gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
};

pub struct DecodedImage {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Decoding and directory scanning run off the GTK thread. Cap allocations so
/// a malformed image cannot exhaust the desktop's memory.
pub fn decode(path: &Path) -> Result<DecodedImage, String> {
    use image::ImageDecoder;
    let mut reader = image::ImageReader::open(path)
        .map_err(|error| error.to_string())?
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(32768);
    limits.max_image_height = Some(32768);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|error| error.to_string())?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > 64 * 1024 * 1024 {
        return Err("图片超过 6400 万像素，请选择较小的图片。".into());
    }
    let orientation = decoder.orientation().map_err(|error| error.to_string())?;
    let mut image =
        image::DynamicImage::from_decoder(decoder).map_err(|error| error.to_string())?;
    image.apply_orientation(orientation);
    let rgba = image.into_rgba8();
    Ok(DecodedImage {
        width: rgba.width(),
        height: rgba.height(),
        pixels: rgba.into_raw(),
    })
}

struct Viewer {
    window: gtk::ApplicationWindow,
    title: gtk::Label,
    status: gtk::Label,
    picture: gtk::Picture,
    scroll: gtk::ScrolledWindow,
    stack: gtk::Stack,
    placeholder: gtk::Label,
    previous: gtk::Button,
    next: gtk::Button,
    source: RefCell<Option<gdk_pixbuf::Pixbuf>>,
    displayed: RefCell<Option<gdk_pixbuf::Pixbuf>>,
    paths: RefCell<Vec<PathBuf>>,
    index: Cell<usize>,
    generation: Cell<u64>,
    zoom: Cell<f64>,
    fit: Cell<bool>,
    rotation: Cell<u8>,
    loading: Cell<bool>,
    closed: Cell<bool>,
    decoding: Cell<bool>,
    pending: RefCell<Option<(PathBuf, bool, u64)>>,
}

pub fn open(app: &gtk::Application, files: Vec<gio::File>) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("JWM 图片查看器")
        .default_width(1040)
        .default_height(740)
        .build();
    let title = gtk::Label::new(Some("图片查看器"));
    title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    let header = ui::header(&window, Icon::Image, "JWM 图片查看器", &title);
    let previous = button(Icon::Previous, "上一张 (←)", "win.previous");
    let next = button(Icon::Next, "下一张 (→)", "win.next");
    let fullscreen = button(Icon::Fullscreen, "全屏 (F11)", "win.fullscreen");
    header.pack_end(&ui::group(&[
        &button(Icon::ZoomOut, "缩小 (- / Ctrl+滚轮)", "win.zoom-out"),
        &button(Icon::ZoomIn, "放大 (+ / Ctrl+滚轮)", "win.zoom-in"),
        &button(Icon::Actual, "原始大小 (1)", "win.actual"),
        &button(Icon::Fit, "适应窗口 (F)", "win.fit"),
        &button(Icon::Rotate, "顺时针旋转 (R)", "win.rotate"),
        &fullscreen,
    ]));
    header.pack_end(&ui::group(&[
        &button(Icon::Folder, "打开图片 (Ctrl+O)", "win.open"),
        &previous,
        &next,
    ]));
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let picture = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Contain)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .child(&picture)
        .hexpand(true)
        .vexpand(true)
        .build();
    let placeholder = gtk::Label::builder()
        .label("打开或拖入图片\n\nCtrl+O 打开 · ← → 切换 · F 适应窗口 · F11 全屏")
        .justify(gtk::Justification::Center)
        .wrap(true)
        .margin_start(32)
        .margin_end(32)
        .build();
    placeholder.add_css_class("jwm-placeholder");
    let stack = gtk::Stack::builder().hexpand(true).vexpand(true).build();
    stack.add_css_class("jwm-image-canvas");
    stack.add_named(&placeholder, Some("empty"));
    stack.add_named(&scroll, Some("image"));
    root.append(&stack);
    let status = ui::status();
    root.append(&status);
    window.set_child(Some(&root));
    let viewer = Rc::new(Viewer {
        window,
        title,
        status,
        picture,
        scroll,
        stack,
        placeholder,
        previous,
        next,
        source: RefCell::new(None),
        displayed: RefCell::new(None),
        paths: RefCell::new(vec![]),
        index: Cell::new(0),
        generation: Cell::new(0),
        zoom: Cell::new(1.0),
        fit: Cell::new(true),
        rotation: Cell::new(0),
        loading: Cell::new(false),
        closed: Cell::new(false),
        decoding: Cell::new(false),
        pending: RefCell::new(None),
    });
    viewer.connect();
    viewer.open_files(files);
    viewer.window.present();
}

impl Viewer {
    fn open_files(self: &Rc<Self>, files: Vec<gio::File>) {
        if files.is_empty() {
            self.update_controls();
            return;
        }
        let paths: Result<Vec<_>, _> = files.iter().map(local_path).collect();
        match paths {
            Ok(paths) => {
                *self.paths.borrow_mut() = paths;
                self.index.set(0);
                self.load(true);
            }
            Err(message) => error(&self.window, &message),
        }
    }

    fn load(self: &Rc<Self>, scan: bool) {
        let Some(path) = self.paths.borrow().get(self.index.get()).cloned() else {
            return;
        };
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        let scan = scan && self.paths.borrow().len() == 1;
        self.loading.set(true);
        self.title
            .set_text(&path.file_name().unwrap_or_default().to_string_lossy());
        self.window
            .set_title(Some(&format!("{} — JWM 图片查看器", self.title.text())));
        // Never leave the previous image visible beneath the next file's name.
        self.picture.set_paintable(gdk::Paintable::NONE);
        self.source.borrow_mut().take();
        self.displayed.borrow_mut().take();
        self.placeholder.set_text("正在载入图片…");
        self.stack.set_visible_child_name("empty");
        self.status.set_text(&path.to_string_lossy());
        self.update_controls();
        // Coalesce rapid navigation: each window has at most one active
        // decoder and one pending request, always for the most recent file.
        *self.pending.borrow_mut() = Some((path, scan, generation));
        self.start_decode();
    }

    fn start_decode(self: &Rc<Self>) {
        if self.decoding.get() {
            return;
        }
        let Some((path, scan, generation)) = self.pending.borrow_mut().take() else {
            return;
        };
        self.decoding.set(true);
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let result = gio::spawn_blocking(move || {
                let entries = scan.then(|| neighbors(&path));
                (decode(&path), entries, path)
            })
            .await;
            let Some(this) = weak.upgrade() else {
                return;
            };
            this.decoding.set(false);
            if this.closed.get() {
                return;
            }
            if this.generation.get() != generation {
                this.start_decode();
                return;
            }
            this.loading.set(false);
            let (decoded, entries, path) = match result {
                Ok(value) => value,
                Err(_) => {
                    this.load_error("图片解码失败，请选择其他图片。");
                    return;
                }
            };
            if let Some(entries) = entries {
                this.index.set(
                    entries
                        .iter()
                        .position(|candidate| candidate == &path)
                        .unwrap_or(0),
                );
                *this.paths.borrow_mut() = entries;
            }
            match decoded {
                Ok(decoded) => {
                    let bytes = glib::Bytes::from_owned(decoded.pixels);
                    let pixbuf = gdk_pixbuf::Pixbuf::from_bytes(
                        &bytes,
                        gdk_pixbuf::Colorspace::Rgb,
                        true,
                        8,
                        decoded.width as i32,
                        decoded.height as i32,
                        decoded.width as i32 * 4,
                    );
                    *this.source.borrow_mut() = Some(pixbuf);
                    this.rotation.set(0);
                    this.fit.set(true);
                    this.render();
                    this.stack.set_visible_child_name("image");
                }
                Err(message) => this.load_error(&message),
            }
            this.update_controls();
        });
    }

    fn load_error(&self, message: &str) {
        self.placeholder.set_text(&format!(
            "无法打开图片\n\n{message}\n\n可用 ← → 切换或 Ctrl+O 打开其他图片。"
        ));
        self.stack.set_visible_child_name("empty");
        self.status.set_text("载入失败");
        self.update_controls();
    }

    fn navigate(self: &Rc<Self>, direction: isize) {
        let length = self.paths.borrow().len();
        if length < 2 {
            return;
        }
        self.index
            .set((self.index.get() as isize + direction).rem_euclid(length as isize) as usize);
        self.load(false);
    }

    fn render(&self) {
        let source = self.source.borrow();
        let Some(source) = source.as_ref() else {
            return;
        };
        let rotated = match self.rotation.get() % 4 {
            1 => source.rotate_simple(gdk_pixbuf::PixbufRotation::Clockwise),
            2 => source.rotate_simple(gdk_pixbuf::PixbufRotation::Upsidedown),
            3 => source.rotate_simple(gdk_pixbuf::PixbufRotation::Counterclockwise),
            _ => Some(source.clone()),
        };
        if let Some(pixbuf) = rotated {
            self.picture
                .set_paintable(Some(&gdk::Texture::for_pixbuf(&pixbuf)));
            *self.displayed.borrow_mut() = Some(pixbuf);
            self.layout();
        }
    }

    fn layout(&self) {
        let displayed = self.displayed.borrow();
        let Some(pixbuf) = displayed.as_ref() else {
            return;
        };
        if self.fit.get() {
            self.picture.set_size_request(-1, -1);
            self.picture.set_halign(gtk::Align::Fill);
            self.picture.set_valign(gtk::Align::Fill);
            self.scroll
                .set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
        } else {
            self.scroll
                .set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
            self.picture.set_halign(gtk::Align::Center);
            self.picture.set_valign(gtk::Align::Center);
            self.picture.set_size_request(
                (f64::from(pixbuf.width()) * self.zoom.get()).round() as i32,
                (f64::from(pixbuf.height()) * self.zoom.get()).round() as i32,
            );
        }
        let path = self
            .paths
            .borrow()
            .get(self.index.get())
            .cloned()
            .unwrap_or_default();
        let zoom = if self.fit.get() {
            "适应窗口".to_owned()
        } else {
            format!("{:.0}%", self.zoom.get() * 100.0)
        };
        self.status.set_text(&format!(
            "{} / {}  ·  {} × {}  ·  {zoom}  ·  {}",
            self.index.get() + 1,
            self.paths.borrow().len(),
            pixbuf.width(),
            pixbuf.height(),
            path.to_string_lossy()
        ));
    }

    fn zoom_by(&self, factor: f64) {
        if self.fit.get() {
            let displayed = self.displayed.borrow();
            let Some(pixbuf) = displayed.as_ref() else {
                return;
            };
            self.zoom.set(
                (f64::from(self.scroll.width().max(1)) / f64::from(pixbuf.width()))
                    .min(f64::from(self.scroll.height().max(1)) / f64::from(pixbuf.height())),
            );
        }
        self.fit.set(false);
        self.zoom.set((self.zoom.get() * factor).clamp(0.05, 16.0));
        self.layout();
    }

    fn update_controls(&self) {
        let multiple = self.paths.borrow().len() > 1;
        self.previous.set_sensitive(multiple);
        self.next.set_sensitive(multiple);
        for name in ["rotate", "fit", "actual", "zoom-in", "zoom-out"] {
            if let Some(action) = self
                .window
                .lookup_action(name)
                .and_downcast::<gio::SimpleAction>()
            {
                action.set_enabled(!self.loading.get() && self.source.borrow().is_some());
            }
        }
    }

    fn connect(self: &Rc<Self>) {
        let this = self.clone();
        action(&self.window, "open", &["<Control>o"], move || {
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("图片"));
            for mime in [
                "image/png",
                "image/jpeg",
                "image/gif",
                "image/webp",
                "image/bmp",
                "image/tiff",
                "image/x-icon",
                "image/vnd.microsoft.icon",
                "image/x-portable-anymap",
                "image/x-portable-pixmap",
                "image/x-portable-graymap",
                "image/x-portable-bitmap",
            ] {
                filter.add_mime_type(mime);
            }
            let filters = gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            let dialog = gtk::FileDialog::builder()
                .title("打开图片")
                .filters(&filters)
                .build();
            let weak = Rc::downgrade(&this);
            dialog.open_multiple(Some(&this.window), gio::Cancellable::NONE, move |result| {
                if let (Some(this), Ok(files)) = (weak.upgrade(), result) {
                    this.open_files(files.iter::<gio::File>().filter_map(Result::ok).collect());
                }
            });
        });
        let this = self.clone();
        action(&self.window, "previous", &["Left", "Page_Up"], move || {
            this.navigate(-1)
        });
        let this = self.clone();
        action(
            &self.window,
            "next",
            &["Right", "Page_Down", "space"],
            move || this.navigate(1),
        );
        let this = self.clone();
        action(
            &self.window,
            "zoom-in",
            &["plus", "equal", "<Control>plus", "<Control>equal"],
            move || this.zoom_by(1.25),
        );
        let this = self.clone();
        action(
            &self.window,
            "zoom-out",
            &["minus", "<Control>minus"],
            move || this.zoom_by(0.8),
        );
        let this = self.clone();
        action(&self.window, "fit", &["f", "0"], move || {
            this.fit.set(true);
            this.layout();
        });
        let this = self.clone();
        action(&self.window, "actual", &["1"], move || {
            this.fit.set(false);
            this.zoom.set(1.0);
            this.layout();
        });
        let this = self.clone();
        action(&self.window, "rotate", &["r"], move || {
            this.rotation.set((this.rotation.get() + 1) % 4);
            this.render();
        });
        let this = self.clone();
        action(&self.window, "fullscreen", &["F11"], move || {
            if this.window.is_fullscreen() {
                this.window.unfullscreen();
            } else {
                this.window.fullscreen();
            }
        });
        let this = self.clone();
        action(&self.window, "escape", &["Escape"], move || {
            this.window.unfullscreen()
        });
        let this = self.clone();
        action(&self.window, "close", &["<Control>w"], move || {
            this.window.close()
        });
        let controller = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let weak = Rc::downgrade(self);
        controller.connect_scroll(move |controller, _, dy| {
            if controller
                .current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK)
                && let Some(this) = weak.upgrade()
            {
                this.zoom_by(if dy < 0.0 { 1.25 } else { 0.8 });
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.scroll.add_controller(controller);
        let drag = gtk::GestureDrag::new();
        let origin = Rc::new(Cell::new((0.0, 0.0)));
        let weak = Rc::downgrade(self);
        let start = origin.clone();
        drag.connect_drag_begin(move |_, _, _| {
            if let Some(this) = weak.upgrade() {
                start.set((
                    this.scroll.hadjustment().value(),
                    this.scroll.vadjustment().value(),
                ));
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_update(move |_, x, y| {
            if let Some(this) = weak.upgrade() {
                this.scroll.hadjustment().set_value(origin.get().0 - x);
                this.scroll.vadjustment().set_value(origin.get().1 - y);
            }
        });
        self.picture.add_controller(drag);
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let weak = Rc::downgrade(self);
        drop.connect_drop(move |_, value, _, _| {
            if let (Some(this), Ok(files)) = (weak.upgrade(), value.get::<gdk::FileList>()) {
                this.open_files(files.files());
                return true;
            }
            false
        });
        self.window.add_controller(drop);
        let weak = Rc::downgrade(self);
        self.window.connect_destroy(move |window| {
            if let Some(this) = weak.upgrade() {
                this.closed.set(true);
            }
            for name in window.list_actions() {
                window.remove_action(&name);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_png_and_rejects_corruption_without_a_display() {
        let path = std::env::temp_dir().join(format!("jwm-viewer-test-{}.png", std::process::id()));
        image::RgbaImage::from_pixel(3, 2, image::Rgba([20, 40, 60, 255]))
            .save(&path)
            .unwrap();
        let decoded = decode(&path).unwrap();
        assert_eq!((decoded.width, decoded.height), (3, 2));
        assert_eq!(decoded.pixels.len(), 24);
        assert_eq!(&decoded.pixels[..4], &[20, 40, 60, 255]);
        std::fs::write(&path, b"not an image").unwrap();
        assert!(decode(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
