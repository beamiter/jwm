//! Toolkit-free native windows. Cairo/Pango owns the scene and hit regions;
//! winit owns X11/xdg-shell input, IME and window requests, softbuffer presents.
use crate::app::{App, Effect};
use gio::prelude::*;
use std::{
    collections::HashMap,
    num::NonZeroU32,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalPosition, LogicalSize},
    event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::ModifiersState,
    platform::{wayland::WindowAttributesExtWayland, x11::WindowAttributesExtX11},
    window::{CursorIcon, Fullscreen, ResizeDirection, Window, WindowId},
};

type Error = Box<dyn std::error::Error>;
struct NativeWindow {
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    _context: softbuffer::Context<Rc<Window>>,
    window: Rc<Window>,
    app: App,
    mods: ModifiersState,
    canvas: Option<cairo::ImageSurface>,
    last_click: Option<Instant>,
}
struct Host {
    id: String,
    initial: Option<(bool, Vec<PathBuf>)>,
    windows: HashMap<WindowId, NativeWindow>,
    clipboard: Option<arboard::Clipboard>,
    failure: Option<String>,
    requests: std::sync::mpsc::Receiver<Vec<PathBuf>>,
}
pub(crate) fn run(id: &str, notes: bool, paths: Vec<PathBuf>) -> Result<(), Error> {
    // GApplication supplies desktop activation/single-instance dispatch only;
    // it owns no widgets. Pump its context beside the native event loop.
    let application = gio::Application::new(Some(id), gio::ApplicationFlags::HANDLES_OPEN);
    let (sender, requests) = std::sync::mpsc::channel();
    let activate = sender.clone();
    application.connect_activate(move |_| {
        let _ = activate.send(vec![]);
    });
    application.connect_open(move |_, files, _| {
        let _ = sender.send(files.iter().filter_map(|file| file.path()).collect());
    });
    let registered = application.register(gio::Cancellable::NONE).is_ok();
    if registered && application.is_remote() {
        if paths.is_empty() {
            application.activate();
        } else {
            application.open(
                &paths.iter().map(gio::File::for_path).collect::<Vec<_>>(),
                "",
            );
        }
        return Ok(());
    }
    let event_loop = EventLoop::new()?;
    let mut host = Host {
        id: id.into(),
        initial: Some((notes, paths)),
        windows: HashMap::new(),
        clipboard: arboard::Clipboard::new().ok(),
        failure: None,
        requests,
    };
    event_loop.run_app(&mut host)?;
    if let Some(error) = host.failure {
        return Err(error.into());
    }
    Ok(())
}
impl Host {
    fn open(
        &mut self,
        event_loop: &ActiveEventLoop,
        notes: bool,
        paths: Vec<PathBuf>,
    ) -> Result<(), Error> {
        // Focus files already opened in this process rather than split their edits.
        if notes && let Some(path) = paths.first() {
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            for existing in self.windows.values() {
                if let crate::app::Document::Notes(n) = &existing.app.document
                    && n.document
                        .as_ref()
                        .and_then(|d| d.file.path())
                        .is_some_and(|p| std::fs::canonicalize(&p).unwrap_or(p) == canonical)
                {
                    existing.window.focus_window();
                    return Ok(());
                }
            }
        }
        let app = App::new(notes, paths)?;
        let identity = if notes {
            "org.jwm.Notes"
        } else {
            "org.jwm.ImageViewer"
        };
        let attrs = Window::default_attributes()
            .with_title(app.title())
            .with_inner_size(LogicalSize::new(960., 680.))
            .with_min_inner_size(LogicalSize::new(540., 360.))
            .with_decorations(false);
        let attrs = WindowAttributesExtWayland::with_name(attrs, identity, &self.id);
        let attrs = WindowAttributesExtX11::with_name(attrs, identity, &self.id);
        let window = Rc::new(event_loop.create_window(attrs)?);
        window.set_ime_allowed(app.ime_target() != 0);
        let context = softbuffer::Context::new(window.clone())?;
        let surface = softbuffer::Surface::new(&context, window.clone())?;
        window.request_redraw();
        self.windows.insert(
            window.id(),
            NativeWindow {
                surface,
                _context: context,
                window,
                app,
                mods: ModifiersState::empty(),
                canvas: None,
                last_click: None,
            },
        );
        Ok(())
    }
    fn effects(&mut self, event_loop: &ActiveEventLoop, id: WindowId) {
        let Some(native) = self.windows.get_mut(&id) else {
            return;
        };
        let effects = std::mem::take(&mut native.app.effects);
        for effect in effects {
            match effect {
                Effect::OpenNote(path) => {
                    if let Err(error) = self.open(event_loop, true, vec![path])
                        && let Some(native) = self.windows.get_mut(&id)
                    {
                        native.app.show_error(error.to_string());
                    }
                }
                Effect::Notebook => {
                    if let Err(error) = self.open(event_loop, true, vec![]) {
                        eprintln!("new notebook: {error}");
                    }
                }
                Effect::Close => {
                    self.windows.remove(&id);
                }
                effect => {
                    if let Some(native) = self.windows.get(&id) {
                        match effect {
                            Effect::Minimize => native.window.set_minimized(true),
                            Effect::Maximize => {
                                native.window.set_maximized(!native.window.is_maximized())
                            }
                            Effect::Fullscreen => native.window.set_fullscreen(
                                if native.window.fullscreen().is_some() {
                                    None
                                } else {
                                    Some(Fullscreen::Borderless(None))
                                },
                            ),
                            Effect::LeaveFullscreen => native.window.set_fullscreen(None),
                            _ => (),
                        }
                    }
                }
            }
        }
        if self.windows.is_empty() {
            event_loop.exit();
        }
    }
}
impl ApplicationHandler for Host {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let Some((notes, paths)) = self.initial.take() else {
            return;
        };
        if notes && !paths.is_empty() {
            for path in paths {
                if let Err(error) = self.open(event_loop, true, vec![path]) {
                    self.failure = Some(error.to_string());
                }
            }
        } else if let Err(error) = self.open(event_loop, notes, paths) {
            self.failure = Some(error.to_string());
        }
        if self.windows.is_empty() {
            event_loop.exit();
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(native) = self.windows.get_mut(&id) else {
            return;
        };
        let ime_target = native.app.ime_target();
        match event {
            WindowEvent::CloseRequested => native.app.close(),
            WindowEvent::ModifiersChanged(mods) => native.mods = mods.state(),
            WindowEvent::Focused(focused) => {
                native.app.focused = focused;
                if !focused {
                    native.app.clear_preedit();
                }
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => (),
            WindowEvent::CursorMoved { position, .. } => {
                let position = position.to_logical::<f64>(native.window.scale_factor());
                native.app.motion(position.x, position.y);
                native.window.set_cursor(
                    native
                        .resize_direction()
                        .map(CursorIcon::from)
                        .unwrap_or(CursorIcon::Default),
                );
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                if state == ElementState::Pressed {
                    if let Some(direction) = native.resize_direction() {
                        let _ = native.window.drag_resize_window(direction);
                    } else if native.app.press(native.mods) {
                        if native
                            .last_click
                            .is_some_and(|t| t.elapsed() < Duration::from_millis(350))
                        {
                            native.window.set_maximized(!native.window.is_maximized());
                            native.last_click = None;
                        } else {
                            native.last_click = Some(Instant::now());
                            let _ = native.window.drag_window();
                        }
                    }
                } else {
                    native.app.release();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } if native.app.pointer.1 < crate::ui::HEADER => {
                native.window.show_window_menu(LogicalPosition::new(
                    native.app.pointer.0,
                    native.app.pointer.1,
                ));
            }
            WindowEvent::MouseWheel { delta, .. } => native.app.scroll(
                match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / 36.,
                },
                native.mods,
            ),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } if event.state == ElementState::Pressed => {
                native.app.key(&event, native.mods, &mut self.clipboard)
            }
            WindowEvent::Ime(Ime::Commit(text)) => native.app.ime(text, true),
            WindowEvent::Ime(Ime::Preedit(text, _)) => native.app.ime(text, false),
            WindowEvent::Ime(Ime::Disabled) => native.app.clear_preedit(),
            WindowEvent::DroppedFile(path) => native.app.dropped(path),
            WindowEvent::RedrawRequested => {
                if let Err(error) = native.draw() {
                    eprintln!("native app rendering failed: {error}");
                    self.failure = Some(error.to_string());
                    event_loop.exit();
                }
                return;
            }
            _ => return,
        }
        if native.app.ime_target() != ime_target {
            // Internal focus changes must end the old composition before a
            // different editor/path field begins receiving committed text.
            native.app.clear_preedit();
            native.window.set_ime_allowed(false);
            if native.app.ime_target() != 0 {
                native.window.set_ime_allowed(true);
            }
        }
        native.window.set_title(&native.app.title());
        native.window.request_redraw();
        self.effects(event_loop, id);
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let context = gio::glib::MainContext::default();
        for _ in 0..16 {
            if !context.pending() {
                break;
            }
            context.iteration(false);
        }
        while let Ok(paths) = self.requests.try_recv() {
            if paths.is_empty()
                && let Some(native) = self.windows.values().next()
            {
                native.window.focus_window();
                continue;
            }
            let notes = self.id == "org.jwm.Notes";
            let groups = if notes {
                paths.into_iter().map(|p| vec![p]).collect()
            } else {
                vec![paths]
            };
            for paths in groups {
                if let Err(error) = self.open(event_loop, notes, paths)
                    && let Some(native) = self.windows.values_mut().next()
                {
                    native.app.show_error(error.to_string());
                    native.window.request_redraw();
                }
            }
        }
        let mut loading = false;
        for native in self.windows.values_mut() {
            if native.app.tick() {
                native.window.set_title(&native.app.title());
                native.window.request_redraw();
            }
            loading |= native.app.loading();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(if loading { 25 } else { 100 }),
        ));
    }
}
impl NativeWindow {
    fn resize_direction(&self) -> Option<ResizeDirection> {
        if self.window.is_maximized() || self.window.fullscreen().is_some() {
            return None;
        }
        let (x, y) = self.app.pointer;
        let (w, h) = self.app.size;
        match (x < 5., x >= w - 5., y < 5., y >= h - 5.) {
            (true, _, true, _) => Some(ResizeDirection::NorthWest),
            (_, true, true, _) => Some(ResizeDirection::NorthEast),
            (true, _, _, true) => Some(ResizeDirection::SouthWest),
            (_, true, _, true) => Some(ResizeDirection::SouthEast),
            (true, _, _, _) => Some(ResizeDirection::West),
            (_, true, _, _) => Some(ResizeDirection::East),
            (_, _, true, _) => Some(ResizeDirection::North),
            (_, _, _, true) => Some(ResizeDirection::South),
            _ => None,
        }
    }
    fn draw(&mut self) -> Result<(), Error> {
        let size = self.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return Ok(());
        };
        // Winit supplies physical sizes, while the entire scene/input uses logical
        // coordinates. The Cairo device scale keeps both aligned on HiDPI.
        if self
            .canvas
            .as_ref()
            .is_none_or(|c| c.width() != size.width as i32 || c.height() != size.height as i32)
        {
            self.canvas = Some(cairo::ImageSurface::create(
                cairo::Format::Rgb24,
                size.width as i32,
                size.height as i32,
            )?);
        }
        let scale = self.window.scale_factor();
        let canvas = self.canvas.as_mut().unwrap();
        canvas.set_device_scale(scale, scale);
        {
            let cr = cairo::Context::new(&*canvas)?;
            self.app.draw(
                &cr,
                size.width as f64 / scale,
                size.height as f64 / scale,
                self.window.is_maximized(),
            );
        }
        canvas.flush();
        self.surface.resize(w, h)?;
        let stride = canvas.stride() as usize;
        let data = canvas.data()?;
        let mut buffer = self.surface.buffer_mut()?;
        for (row, destination) in data
            .chunks_exact(stride)
            .zip(buffer.chunks_exact_mut(size.width as usize))
        {
            for (pixel, out) in row.chunks_exact(4).zip(destination) {
                *out = u32::from_ne_bytes(pixel.try_into().unwrap()) & 0x00ff_ffff;
            }
        }
        self.window.pre_present_notify();
        buffer.present()?;
        let (x, y) = self.app.ime_position();
        self.window
            .set_ime_cursor_area(LogicalPosition::new(x, y), LogicalSize::new(2., 20.));
        Ok(())
    }
}
