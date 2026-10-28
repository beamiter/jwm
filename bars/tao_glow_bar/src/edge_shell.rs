//! Companion X11/XWayland surfaces for the official bar. Only the narrow
//! reveal sensors remain mapped while hidden; the desktop is never covered by
//! a fullscreen input window. One event-driven worker owns all three surfaces.
use super::{OpaqueWallpaper, RootPixmapSource, UserEvent};
#[path = "edge_painter.rs"]
mod painter;
use anyhow::{Context as _, Result};
use cairo::{Context, Format, ImageSurface};
use painter::{Painter, rounded};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;
use tao::event_loop::EventLoopProxy;
use x11rb::connection::Connection as _;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    self, AtomEnum, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, ImageFormat,
    PropMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use xbar_core::config::BarConfig;
use xbar_core::edge_shell::{EdgeReveal, EdgeShellConfig, RevealMode, panel_geometry};
use xbar_core::glass::{GlassBackdrop, GlassParams};
use xbar_core::model::{BarSnapshot, UserAction};
use xbar_core::{BarEdge, BarPlacement, MonitorGeometry, ThemeMode};

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Model(UserAction),
    Level { audio: bool, percent: i32 },
    FocusWindow(u64),
    CycleTasks,
    Files,
    Terminal,
    Browser,
}

struct Shared {
    latest: Option<(MonitorGeometry, BarSnapshot)>,
    stopped: bool,
}

pub struct EdgeShell {
    shared: Arc<Mutex<Shared>>,
    wake: UnixStream,
    worker: Option<JoinHandle<()>>,
}

impl EdgeShell {
    pub fn start(
        config: BarConfig,
        output: MonitorGeometry,
        snapshot: BarSnapshot,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<Option<Self>> {
        if !config.edge_shell.enabled || config.presentation.edge != BarEdge::Top {
            return Ok(None);
        }
        let shared = Arc::new(Mutex::new(Shared {
            latest: Some((output, snapshot)),
            stopped: false,
        }));
        let worker_shared = shared.clone();
        let (wake, mut receiver) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        receiver.set_nonblocking(true)?;
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("edge-shell".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let mut host = Host::new(config, proxy)?;
                    host.update(
                        output,
                        worker_shared.lock().unwrap().latest.take().unwrap().1,
                    )?;
                    let _ = ready_tx.send(Ok::<_, String>(()));
                    loop {
                        let now = Instant::now();
                        host.events(now)?;
                        host.timers(now)?;
                        {
                            let mut shared = worker_shared.lock().unwrap();
                            if shared.stopped {
                                break;
                            }
                            if let Some((output, snapshot)) = shared.latest.take() {
                                drop(shared);
                                host.update(output, snapshot)?;
                            }
                        }
                        host.conn.flush()?;
                        host.events(Instant::now())?;
                        let timeout = host
                            .panels
                            .iter()
                            .filter(|panel| panel.pressed.is_none())
                            .filter_map(|panel| panel.reveal.deadline())
                            .min()
                            .map_or(-1, |deadline| {
                                deadline
                                    .saturating_duration_since(Instant::now())
                                    .as_millis()
                                    .min(i32::MAX as u128) as i32
                            });
                        let mut fds = [
                            libc::pollfd {
                                fd: host.conn.stream().as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                            libc::pollfd {
                                fd: receiver.as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                        ];
                        // Both descriptors remain owned until this worker exits.
                        let result =
                            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
                        if result < 0
                            && std::io::Error::last_os_error().kind()
                                != std::io::ErrorKind::Interrupted
                        {
                            return Err(std::io::Error::last_os_error().into());
                        }
                        let mut bytes = [0; 64];
                        while receiver.read(&mut bytes).is_ok_and(|read| read > 0) {}
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = ready_tx.send(Err(error.to_string()));
                    log::error!("edge shell stopped: {error:#}");
                }
            })?;
        let shell = Self {
            shared,
            wake,
            worker: Some(worker),
        };
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(10))?
            .map_err(anyhow::Error::msg)?;
        Ok(Some(shell))
    }

    pub fn update(&mut self, output: MonitorGeometry, snapshot: BarSnapshot) {
        self.shared.lock().unwrap().latest = Some((output, snapshot));
        let _ = self.wake.write(&[1]);
    }
}

impl Drop for EdgeShell {
    fn drop(&mut self) {
        self.shared.lock().unwrap().stopped = true;
        let _ = self.wake.write(&[1]);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Clone)]
struct Hit {
    rect: [f64; 4],
    action: Action,
    /// Normalized slider, mapped to an existing model adjustment on input.
    slider: Option<(bool, f64, f64)>,
}

impl Hit {
    fn contains(&self, x: f64, y: f64) -> bool {
        let [left, top, width, height] = self.rect;
        x >= left && x < left + width && y >= top && y < top + height
    }
}

struct Panel {
    task_offset: usize,
    edge: BarEdge,
    window: u32,
    gc: u32,
    reveal: EdgeReveal,
    placement: BarPlacement,
    hits: Vec<Hit>,
    hovered: Option<usize>,
    pressed: Option<Hit>,
}

struct Host {
    conn: Rc<RustConnection>,
    screen: usize,
    depth: u8,
    visual: u32,
    colormap: u32,
    config: EdgeShellConfig,
    panels: Vec<Panel>,
    output: MonitorGeometry,
    snapshot: Option<BarSnapshot>,
    tasks: Vec<super::shell_ipc::Task>,
    last_task_refresh: Option<Instant>,
    shown: bool,
    glass: GlassBackdrop<OpaqueWallpaper>,
    frosted_params: GlassParams,
    proxy: EventLoopProxy<UserEvent>,
}

impl Host {
    fn new(config: BarConfig, proxy: EventLoopProxy<UserEvent>) -> Result<Self> {
        let (conn, screen) =
            x11rb::connect(None).context("four-edge panels require X11 or XWayland")?;
        let conn = Rc::new(conn);
        let root = &conn.setup().roots[screen];
        // Prefer a true ARGB visual; the compositor blends the capsule and
        // transparent passage. 24-bit X servers use the wallpaper fallback.
        let composited = super::compositor_active(&conn, screen);
        let visual = root
            .allowed_depths
            .iter()
            .filter(|_| composited)
            .find(|depth| depth.depth == 32)
            .and_then(|depth| {
                depth
                    .visuals
                    .iter()
                    .find(|visual| visual.class == xproto::VisualClass::TRUE_COLOR)
            })
            .map(|visual| (32, visual.visual_id))
            .unwrap_or((root.root_depth, root.root_visual));
        let colormap = conn.generate_id()?;
        conn.create_colormap(xproto::ColormapAlloc::NONE, colormap, root.root, visual.1)?
            .check()?;
        let source = config
            .glass
            .file_source(
                u32::from(root.width_in_pixels),
                u32::from(root.height_in_pixels),
                [180, 215, 246],
            )
            .map_or_else(
                || OpaqueWallpaper::Root(RootPixmapSource::new(conn.clone(), screen)),
                |file| OpaqueWallpaper::FileThenRoot {
                    file,
                    root: RootPixmapSource::new(conn.clone(), screen),
                },
            );
        let frosted_params = config.glass.params();
        let glass = GlassBackdrop::new(source, frosted_params).with_fallback([180, 215, 246]);
        let output = MonitorGeometry {
            x: 0,
            y: 0,
            width: u32::from(root.width_in_pixels),
            height: u32::from(root.height_in_pixels),
        };
        let mut host = Self {
            conn,
            screen,
            depth: visual.0,
            visual: visual.1,
            colormap,
            config: config.edge_shell,
            panels: Vec::new(),
            output,
            snapshot: None,
            tasks: Vec::new(),
            last_task_refresh: None,
            shown: true,
            glass,
            frosted_params,
            proxy,
        };
        for edge in [BarEdge::Left, BarEdge::Right, BarEdge::Bottom] {
            let mode = host.config.mode(edge);
            if mode == RevealMode::Disabled {
                continue;
            }
            let window = host.conn.generate_id()?;
            let gc = host.conn.generate_id()?;
            let root = host.conn.setup().roots[screen].root;
            host.conn
                .create_window(
                    host.depth,
                    window,
                    root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    host.visual,
                    &CreateWindowAux::new()
                        .override_redirect(1)
                        .background_pixel(0)
                        .border_pixel(0)
                        .colormap(host.colormap)
                        .event_mask(
                            EventMask::EXPOSURE
                                | EventMask::ENTER_WINDOW
                                | EventMask::LEAVE_WINDOW
                                | EventMask::POINTER_MOTION
                                | EventMask::BUTTON_PRESS
                                | EventMask::BUTTON_RELEASE,
                        ),
                )?
                .check()?;
            host.conn
                .create_gc(gc, window, &CreateGCAux::new())?
                .check()?;
            let name = format!("jwm-edge-{}", edge.as_str());
            host.conn.change_property8(
                PropMode::REPLACE,
                window,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                name.as_bytes(),
            )?;
            host.conn.change_property8(
                PropMode::REPLACE,
                window,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                b"jwm-edge-shell\0JwmEdgeShell\0",
            )?;
            let atom = |name: &[u8]| -> Result<u32> {
                Ok(host.conn.intern_atom(false, name)?.reply()?.atom)
            };
            let dock = atom(b"_NET_WM_WINDOW_TYPE_DOCK")?;
            host.conn.change_property32(
                PropMode::REPLACE,
                window,
                atom(b"_NET_WM_WINDOW_TYPE")?,
                AtomEnum::ATOM,
                &[dock],
            )?;
            host.panels.push(Panel {
                task_offset: 0,
                edge,
                window,
                gc,
                reveal: EdgeReveal::new(mode),
                placement: BarPlacement {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
                hits: Vec::new(),
                hovered: None,
                pressed: None,
            });
        }
        Ok(host)
    }

    fn update(&mut self, mut output: MonitorGeometry, snapshot: BarSnapshot) -> Result<()> {
        let was_shown = self.shown;
        if snapshot.wm_available {
            // The WM owns visibility and monitor geometry, including native
            // Wayland windows, output locks and per-tag bar preferences.
            self.shown = false;
            if let Ok(Some(monitor)) = super::shell_ipc::monitor(snapshot.monitor.0) {
                self.shown = monitor.show_bar && !monitor.has_visible_fullscreen && !monitor.locked;
                output = MonitorGeometry {
                    x: monitor.x,
                    y: monitor.y,
                    width: monitor.w,
                    height: monitor.h,
                };
            }
        }
        if was_shown && !self.shown {
            for panel in &mut self.panels {
                self.conn.unmap_window(panel.window)?;
                panel.placement.width = 0;
                panel.reveal = EdgeReveal::new(self.config.mode(panel.edge));
                panel.pressed = None;
            }
        }
        let moved = self.output != output;
        let changed = self.snapshot.as_ref() != Some(&snapshot);
        self.output = output;
        self.snapshot = Some(snapshot);
        let tasks_changed = self.refresh_tasks();
        for index in 0..self.panels.len() {
            self.place(index)?;
            if changed || moved || tasks_changed || was_shown != self.shown {
                self.paint(index)?;
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    fn place(&mut self, index: usize) -> Result<()> {
        if !self.shown {
            return Ok(());
        }
        let panel = &mut self.panels[index];
        let rect = panel_geometry(
            self.output,
            panel.edge,
            panel.reveal.visible(),
            self.config.trigger_pixels,
        );
        if panel.placement == rect && panel.placement.width != 0 {
            return Ok(());
        }
        panel.placement = rect;
        self.conn.configure_window(
            panel.window,
            &xproto::ConfigureWindowAux::new()
                .x(rect.x)
                .y(rect.y)
                .width(rect.width)
                .height(rect.height)
                .stack_mode(xproto::StackMode::ABOVE),
        )?;
        // Only pinned panels reserve space. Interior monitor edges must never
        // advertise a deep root strut that eats a neighbouring output.
        let root = self
            .conn
            .get_geometry(self.conn.setup().roots[self.screen].root)?
            .reply()?;
        let mut strut = [0u32; 12];
        if self.config.mode(panel.edge) == RevealMode::Always {
            let (x, y) = (self.output.x.max(0) as u32, self.output.y.max(0) as u32);
            match panel.edge {
                BarEdge::Left if self.output.x == 0 => {
                    strut[0] = rect.width;
                    strut[4] = y;
                    strut[5] = y + self.output.height.saturating_sub(1);
                }
                BarEdge::Right
                    if i64::from(self.output.x) + i64::from(self.output.width)
                        == i64::from(root.width) =>
                {
                    strut[1] = rect.width;
                    strut[6] = y;
                    strut[7] = y + self.output.height.saturating_sub(1);
                }
                BarEdge::Bottom
                    if i64::from(self.output.y) + i64::from(self.output.height)
                        == i64::from(root.height) =>
                {
                    strut[3] = rect.height;
                    strut[10] = x;
                    strut[11] = x + self.output.width.saturating_sub(1);
                }
                _ => {}
            }
        }
        let atom = self
            .conn
            .intern_atom(false, b"_NET_WM_STRUT_PARTIAL")?
            .reply()?
            .atom;
        self.conn.change_property32(
            PropMode::REPLACE,
            panel.window,
            atom,
            AtomEnum::CARDINAL,
            &strut,
        )?;
        self.conn.map_window(panel.window)?;
        Ok(())
    }

    fn timers(&mut self, now: Instant) -> Result<()> {
        if !self.shown {
            return Ok(());
        }
        for index in 0..self.panels.len() {
            if self.panels[index].pressed.is_none() && self.panels[index].reveal.tick(now) {
                if self.panels[index].edge == BarEdge::Bottom && self.panels[index].reveal.visible()
                {
                    self.refresh_tasks();
                }
                self.panels[index].hovered = None;
                self.place(index)?;
                self.paint(index)?;
            }
        }
        Ok(())
    }

    fn refresh_tasks(&mut self) -> bool {
        if !self
            .panels
            .iter()
            .any(|panel| panel.edge == BarEdge::Bottom && panel.reveal.visible())
        {
            return false;
        }
        let now = Instant::now();
        if self
            .last_task_refresh
            .is_some_and(|last| now.duration_since(last) < std::time::Duration::from_millis(500))
        {
            return false;
        }
        self.last_task_refresh = Some(now);
        let Some(snapshot) = self.snapshot.as_ref() else {
            return false;
        };
        let tasks = match super::shell_ipc::tasks(snapshot.monitor.0) {
            Ok(tasks) => tasks,
            Err(error) => {
                log::debug!("task inventory unavailable: {error:#}");
                Vec::new()
            }
        };
        let changed = self.tasks != tasks;
        self.tasks = tasks;
        changed
    }

    fn events(&mut self, now: Instant) -> Result<()> {
        while let Some(event) = self.conn.poll_for_event()? {
            let window = match &event {
                Event::Expose(e) => e.window,
                Event::EnterNotify(e) => e.event,
                Event::LeaveNotify(e) => e.event,
                Event::MotionNotify(e) => e.event,
                Event::ButtonPress(e) => e.event,
                Event::ButtonRelease(e) => e.event,
                _ => continue,
            };
            let Some(index) = self.panels.iter().position(|panel| panel.window == window) else {
                continue;
            };
            match event {
                Event::Expose(e) if e.count == 0 => self.paint(index)?,
                Event::EnterNotify(_) => self.panels[index].reveal.enter(now, &self.config),
                Event::LeaveNotify(_) => {
                    self.panels[index].reveal.leave(now, &self.config);
                    self.panels[index].hovered = None;
                    self.paint(index)?;
                }
                Event::MotionNotify(e) => {
                    let panel = &mut self.panels[index];
                    let next = panel
                        .hits
                        .iter()
                        .position(|hit| hit.contains(f64::from(e.event_x), f64::from(e.event_y)));
                    let repaint = panel.hovered != next;
                    panel.hovered = next;
                    if let Some(hit) = panel.pressed.clone()
                        && hit.slider.is_some()
                    {
                        self.activate_slider(&hit, f64::from(e.event_y));
                    }
                    if repaint {
                        self.paint(index)?;
                    }
                }
                Event::ButtonPress(e) => {
                    if !self.panels[index].reveal.visible() {
                        continue;
                    }
                    let hit = self.panels[index]
                        .hits
                        .iter()
                        .find(|hit| hit.contains(f64::from(e.event_x), f64::from(e.event_y)))
                        .cloned();
                    if e.detail == 1 {
                        self.panels[index].pressed = hit.clone();
                        if let Some(hit) = hit
                            && hit.slider.is_some()
                        {
                            self.activate_slider(&hit, f64::from(e.event_y));
                        }
                    } else if let Some(hit) = hit
                        && matches!(e.detail, 4 | 5)
                        && let Some((audio, _, _)) = hit.slider
                    {
                        self.send(Action::Model(if audio {
                            UserAction::AdjustVolume(if e.detail == 4 { 5 } else { -5 })
                        } else {
                            UserAction::AdjustBrightness(if e.detail == 4 { 5 } else { -5 })
                        }));
                    }
                }
                Event::ButtonRelease(e) if e.detail == 1 => {
                    if let Some(pressed) = self.panels[index].pressed.take()
                        && pressed.slider.is_none()
                        && let Some(hit) = self.panels[index]
                            .hits
                            .iter()
                            .find(|hit| hit.contains(f64::from(e.event_x), f64::from(e.event_y)))
                        && hit.action == pressed.action
                    {
                        if pressed.action == Action::CycleTasks {
                            self.panels[index].task_offset =
                                (self.panels[index].task_offset + 1) % self.tasks.len().max(1);
                            self.paint(index)?;
                        } else {
                            self.send(pressed.action);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn send(&self, action: Action) {
        let _ = self.proxy.send_event(UserEvent::EdgeAction(action));
    }

    fn activate_slider(&self, hit: &Hit, y: f64) {
        let Some((audio, top, height)) = hit.slider else {
            return;
        };
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let current = if audio {
            snapshot.audio.volume_percent
        } else {
            snapshot.brightness.percent
        };
        let Some(current) = current else {
            return;
        };
        let target = ((1.0 - (y - top) / height).clamp(0.0, 1.0) * 100.0).round() as i32;
        if target != i32::from(current.rounded()) {
            self.send(Action::Level {
                audio,
                percent: target,
            });
        }
    }

    fn paint(&mut self, index: usize) -> Result<()> {
        if !self.shown {
            return Ok(());
        }
        let panel = &mut self.panels[index];
        let p = panel.placement;
        let mut surface = ImageSurface::create(Format::ARgb32, p.width as i32, p.height as i32)?;
        let ctx = Context::new(&surface)?;
        panel.hits.clear();
        self.glass.cache_mut().set_params(GlassParams::clear());
        if self.depth != 32
            && let Some(backdrop) = self.glass.ensure(p.x, p.y, p.width, p.height)
        {
            ctx.set_source_surface(backdrop, 0.0, 0.0)?;
            ctx.paint()?;
        }
        self.glass.cache_mut().set_params(self.frosted_params);
        if panel.reveal.visible()
            && let Some(snapshot) = self.snapshot.as_ref()
        {
            let (x, y, w, h) = if panel.edge.is_vertical() {
                (
                    12.0,
                    0.0,
                    f64::from(p.width).max(24.0) - 24.0,
                    f64::from(p.height),
                )
            } else {
                (
                    0.0,
                    0.0,
                    f64::from(p.width),
                    f64::from(p.height).max(24.0) - 12.0,
                )
            };
            rounded(
                &ctx,
                x + 0.5,
                y + 0.5,
                (w - 1.0).max(1.0),
                (h - 1.0).max(1.0),
                26.0,
            );
            ctx.save()?;
            ctx.clip();
            if self.depth != 32
                && let Some(backdrop) = self.glass.ensure(p.x, p.y, p.width, p.height)
            {
                ctx.set_source_surface(backdrop, 0.0, 0.0)?;
                ctx.paint()?;
            }
            let dark = snapshot.theme == ThemeMode::Dark;
            if dark {
                ctx.set_source_rgba(0.06, 0.12, 0.22, 0.80);
            } else {
                ctx.set_source_rgba(0.80, 0.91, 1.0, 0.58);
            }
            ctx.paint()?;
            ctx.restore()?;
            rounded(
                &ctx,
                x + 0.5,
                y + 0.5,
                (w - 1.0).max(1.0),
                (h - 1.0).max(1.0),
                26.0,
            );
            ctx.set_source_rgba(1.0, 1.0, 1.0, if dark { 0.22 } else { 0.72 });
            ctx.set_line_width(1.0);
            ctx.stroke()?;
            let mut painter = Painter {
                ctx: &ctx,
                panel,
                snapshot,
                tasks: &self.tasks,
                dark,
            };
            ctx.save()?;
            ctx.rectangle(x, y, w, h);
            ctx.clip();
            match painter.panel.edge {
                BarEdge::Left => painter.left(x, y, w, h)?,
                BarEdge::Right => painter.right(x, y, w, h)?,
                _ => painter.bottom(x, y, w, h)?,
            }
            ctx.restore()?;
        } else if self.depth != 32 {
            // A 24-bit sensor stays subtle when no compositor is present.
            ctx.set_source_rgb(0.43, 0.70, 0.96);
            ctx.paint()?;
        }
        drop(ctx);
        surface.flush();
        // Limit each request to 32 KiB; large high-DPI docks must also work
        // without the BIG-REQUESTS extension.
        let stride = surface.stride() as usize;
        let rows = (32_768 / stride).max(1);
        let bytes = surface.data()?;
        for start in (0..p.height as usize).step_by(rows) {
            let count = rows.min(p.height as usize - start);
            self.conn.put_image(
                ImageFormat::Z_PIXMAP,
                panel.window,
                panel.gc,
                p.width as u16,
                count as u16,
                0,
                start as i16,
                0,
                self.depth,
                &bytes[start * stride..(start + count) * stride],
            )?;
        }
        Ok(())
    }
}
