//! Companion X11/XWayland surfaces for the official bar. Only the narrow
//! reveal sensors remain mapped while hidden; the desktop is never covered by
//! a fullscreen input window. One event-driven worker owns all three surfaces.
use super::{OpaqueWallpaper, RootPixmapSource, UserEvent};
#[path = "edge_painter.rs"]
mod painter;
use anyhow::{Context as _, Result};
use cairo::{Context, Format, ImageSurface};
use painter::{Painter, material, rounded};
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
use xbar_core::edge_shell::{
    EdgeReveal, EdgeShellConfig, RevealMode, panel_geometry, trigger_geometry,
};
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
    MediaPrevious,
    MediaNext,
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
        if !config.edge_shell.enabled {
            return Ok(None);
        }
        if config.presentation.edge != BarEdge::Top {
            log::warn!(
                "The four-edge desktop needs a top primary bar. Set status_bar.position='top' in the JWM configuration (or presentation.position='top' for a standalone bar); the current single-edge placement is preserved."
            );
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
                        host.conn.flush()?;
                        let pointer_poll = host.shown
                            && host.panels.iter().any(|panel| {
                                panel.mapped && host.config.mode(panel.edge) == RevealMode::Hover
                            });
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
                        let timeout = if pointer_poll {
                            if timeout < 0 { 50 } else { timeout.min(50) }
                        } else {
                            timeout
                        };
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

#[derive(Clone, PartialEq)]
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
    sensor: u32,
    sensor_rect: BarPlacement,
    mapped: bool,
    sensor_mapped: bool,
    applied_mode: Option<RevealMode>,
    gc: u32,
    back_buffer: Option<(u32, u32, u32)>,
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
    visibility_known: bool,
    config_stamp: Option<(std::time::SystemTime, u64)>,
    last_config_check: Instant,
    last_pointer_check: Instant,
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
        // Rootless XWayland cannot route compositor pointer events to an
        // InputOnly X window: it has no Wayland surface. Give its trigger a
        // transparent ARGB backing surface; ordinary X11 can use InputOnly.
        let mapped_triggers = std::env::var_os("WAYLAND_DISPLAY")
            .is_some_and(|value| !value.is_empty())
            || conn
                .query_extension(b"XWAYLAND")?
                .reply()
                .is_ok_and(|extension| extension.present);
        let trigger_visual = if mapped_triggers {
            let visual = root
                .allowed_depths
                .iter()
                .find(|depth| depth.depth == 32)
                .and_then(|depth| {
                    depth
                        .visuals
                        .iter()
                        .find(|visual| visual.class == xproto::VisualClass::TRUE_COLOR)
                })
                .context("XWayland edge triggers require a transparent 32-bit visual")?;
            let map = conn.generate_id()?;
            conn.create_colormap(
                xproto::ColormapAlloc::NONE,
                map,
                root.root,
                visual.visual_id,
            )?
            .check()?;
            Some((visual.visual_id, map))
        } else {
            None
        };
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
        // RootPixmapSource subscribes to wallpaper properties on this same
        // connection. Merge the root mask *after* constructing it, so that
        // subscription cannot discard the window-stacking notifications.
        conn.change_window_attributes(
            root.root,
            &xproto::ChangeWindowAttributesAux::new()
                .event_mask(EventMask::SUBSTRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
        )?
        .check()?;
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
            visibility_known: false,
            config_stamp: config_stamp(),
            last_config_check: Instant::now(),
            last_pointer_check: Instant::now(),
            glass,
            frosted_params,
            proxy,
        };
        for edge in [BarEdge::Left, BarEdge::Right, BarEdge::Bottom] {
            let mode = host.config.mode(edge);
            let window = host.conn.generate_id()?;
            let sensor = host.conn.generate_id()?;
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
            let mut trigger_attributes = CreateWindowAux::new().override_redirect(1).event_mask(
                EventMask::ENTER_WINDOW | EventMask::LEAVE_WINDOW | EventMask::POINTER_MOTION,
            );
            let (trigger_depth, trigger_class, trigger_visual_id) =
                if let Some((visual, map)) = trigger_visual {
                    trigger_attributes = trigger_attributes
                        .background_pixel(0)
                        .border_pixel(0)
                        .colormap(map);
                    (32, WindowClass::INPUT_OUTPUT, visual)
                } else {
                    (0, WindowClass::INPUT_ONLY, 0)
                };
            host.conn
                .create_window(
                    trigger_depth,
                    sensor,
                    root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    trigger_class,
                    trigger_visual_id,
                    &trigger_attributes,
                )?
                .check()?;
            let trigger_name = format!("jwm-edge-trigger-{}", edge.as_str());
            host.conn.change_property8(
                PropMode::REPLACE,
                sensor,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                trigger_name.as_bytes(),
            )?;
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
            host.conn.change_property32(
                PropMode::REPLACE,
                sensor,
                atom(b"_NET_WM_WINDOW_TYPE")?,
                AtomEnum::ATOM,
                &[dock],
            )?;
            host.conn.change_property8(
                PropMode::REPLACE,
                sensor,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                b"jwm-edge-trigger\0JwmEdgeShell\0",
            )?;
            host.panels.push(Panel {
                task_offset: 0,
                edge,
                window,
                sensor,
                sensor_rect: BarPlacement {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
                mapped: false,
                sensor_mapped: false,
                applied_mode: None,
                gc,
                back_buffer: None,
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
        if self
            .snapshot
            .as_ref()
            .is_some_and(|old| old.wm_session_id != snapshot.wm_session_id)
        {
            self.tasks.clear();
            self.last_task_refresh = None;
            self.visibility_known = false;
            for panel in &mut self.panels {
                panel.pressed = None;
            }
        }
        let config_changed = self.reload_config()?;
        let was_shown = self.shown;
        if snapshot.wm_available {
            // Transient IPC timeouts must not masquerade as a hide/lock event.
            // Before the first authoritative observation, stay hidden.
            match super::shell_ipc::monitor(snapshot.monitor.0) {
                Ok(Some(monitor)) => {
                    self.visibility_known = true;
                    self.shown =
                        monitor.show_bar && !monitor.has_visible_fullscreen && !monitor.locked;
                    output = MonitorGeometry {
                        x: monitor.x,
                        y: monitor.y,
                        width: monitor.w,
                        height: monitor.h,
                    };
                }
                _ if !self.visibility_known => self.shown = false,
                _ => {}
            }
        }
        if was_shown && !self.shown {
            for panel in &mut self.panels {
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
            if changed || moved || tasks_changed || config_changed || was_shown != self.shown {
                self.paint(index)?;
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    fn pointer_window(&self) -> Result<u32> {
        let root = self.conn.setup().roots[self.screen].root;
        let p = self.conn.query_pointer(root)?.reply()?;
        // Query the sprite window, not just its coordinates. Rootless
        // XWayland can retain the last X11 coordinates after a native client
        // receives the pointer, but clears the root's child in that case.
        // This also reports the actual window during an X11 pointer grab,
        // whose synthetic crossing events describe a different destination.
        Ok(p.child)
    }

    fn pointer_in_panel(&self, index: usize) -> Result<bool> {
        let panel = &self.panels[index];
        let window = self.pointer_window()?;
        Ok((panel.sensor_mapped && window == panel.sensor)
            || (panel.mapped && window == panel.window))
    }

    fn reload_config(&mut self) -> Result<bool> {
        if self.last_config_check.elapsed() < std::time::Duration::from_millis(200) {
            return Ok(false);
        }
        self.last_config_check = Instant::now();
        let stamp = config_stamp();
        if stamp == self.config_stamp {
            return Ok(false);
        }
        self.config_stamp = stamp;
        let next = match BarConfig::load_default() {
            Ok(config) => config.edge_shell,
            Err(error) => {
                log::warn!("Keeping previous edge modes: {error}");
                return Ok(false);
            }
        };
        if next == self.config {
            return Ok(false);
        }
        let pointer = self.pointer_window()?;
        for panel in &mut self.panels {
            let old = self.config.mode(panel.edge);
            let mode = next.mode(panel.edge);
            if mode != old {
                let inside = (panel.mapped && pointer == panel.window)
                    || (panel.sensor_mapped && pointer == panel.sensor);
                panel.reveal.set_mode(mode, inside);
                panel.pressed = None;
                panel.hovered = None;
            }
        }
        self.config = next;
        Ok(true)
    }

    fn place(&mut self, index: usize) -> Result<()> {
        let panel = &mut self.panels[index];
        let mode = self.config.mode(panel.edge);
        let show_sensor = self.shown && mode == RevealMode::Hover;
        let show_panel = self.shown && mode != RevealMode::Disabled && panel.reveal.visible();
        let sensor_rect = trigger_geometry(self.output, panel.edge, self.config.trigger_pixels);
        if panel.sensor_rect != sensor_rect {
            self.conn.configure_window(
                panel.sensor,
                &xproto::ConfigureWindowAux::new()
                    .x(sensor_rect.x)
                    .y(sensor_rect.y)
                    .width(sensor_rect.width)
                    .height(sensor_rect.height),
            )?;
            panel.sensor_rect = sensor_rect;
        }
        if panel.sensor_mapped != show_sensor {
            if show_sensor {
                self.conn.map_window(panel.sensor)?;
            } else {
                self.conn.unmap_window(panel.sensor)?;
            }
            panel.sensor_mapped = show_sensor;
        }
        let rect = panel_geometry(self.output, panel.edge, true, self.config.trigger_pixels);
        if panel.placement == rect && panel.mapped == show_panel && panel.applied_mode == Some(mode)
        {
            return Ok(());
        }
        panel.placement = rect;
        panel.applied_mode = Some(mode);
        if show_panel {
            self.conn.configure_window(
                panel.window,
                &xproto::ConfigureWindowAux::new()
                    .x(rect.x)
                    .y(rect.y)
                    .width(rect.width)
                    .height(rect.height)
                    .stack_mode(xproto::StackMode::ABOVE),
            )?;
        }
        // Only pinned panels reserve space. Interior monitor edges must never
        // advertise a deep root strut that eats a neighbouring output.
        let root = self
            .conn
            .get_geometry(self.conn.setup().roots[self.screen].root)?
            .reply()?;
        let mut strut = [0u32; 12];
        if show_panel && mode == RevealMode::Always {
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

        if !show_panel {
            self.conn.unmap_window(panel.window)?;
        }
        // Mapping is deferred until paint has a complete backing pixmap, so
        // the first revealed frame cannot briefly show the black background.
        panel.mapped = show_panel;
        // The full-edge input strip stays above the painted window. Crossing
        // between them is a transfer within one panel, not a pointer exit.
        if show_sensor {
            self.conn.configure_window(
                panel.sensor,
                &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE),
            )?;
        }
        Ok(())
    }

    fn timers(&mut self, now: Instant) -> Result<()> {
        if !self.shown {
            return Ok(());
        }
        if self.last_pointer_check.elapsed() >= std::time::Duration::from_millis(50)
            && self
                .panels
                .iter()
                .any(|panel| panel.mapped && self.config.mode(panel.edge) == RevealMode::Hover)
        {
            self.last_pointer_check = Instant::now();
            let pointer = self.pointer_window()?;
            for panel in &mut self.panels {
                if !panel.mapped
                    || self.config.mode(panel.edge) != RevealMode::Hover
                    || panel.pressed.is_some()
                {
                    continue;
                }
                let inside =
                    pointer == panel.window || (panel.sensor_mapped && pointer == panel.sensor);
                if inside {
                    panel.reveal.enter(now, &self.config);
                } else if panel.reveal.deadline().is_none() {
                    panel.reveal.leave(now, &self.config);
                }
            }
        }
        for index in 0..self.panels.len() {
            if !self.panels[index].reveal.visible()
                && self.panels[index]
                    .reveal
                    .deadline()
                    .is_some_and(|deadline| deadline <= now)
                && !self.pointer_in_panel(index)?
            {
                self.panels[index].reveal.leave(now, &self.config);
            }
            if self.panels[index].reveal.visible()
                && self.panels[index]
                    .reveal
                    .deadline()
                    .is_some_and(|deadline| deadline <= now)
                && self.pointer_in_panel(index)?
            {
                self.panels[index].reveal.enter(now, &self.config);
            }
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
                return false;
            }
        };
        let changed = self.tasks != tasks;
        self.tasks = tasks;
        changed
    }

    fn events(&mut self, now: Instant) -> Result<()> {
        let mut restack = false;
        while let Some(event) = self.conn.poll_for_event()? {
            if let Event::Error(error) = &event {
                log::error!("edge-shell X11 request failed: {error:?}");
                continue;
            }
            let changed = match &event {
                Event::MapNotify(e) if !e.override_redirect => Some(e.window),
                Event::ConfigureNotify(e) if !e.override_redirect => Some(e.window),
                _ => None,
            };
            if let Some(window) = changed {
                if !self
                    .panels
                    .iter()
                    .any(|panel| window == panel.window || window == panel.sensor)
                {
                    restack = true;
                }
                continue;
            }
            let window = match &event {
                Event::Expose(e) => e.window,
                Event::EnterNotify(e) => e.event,
                Event::LeaveNotify(e) => e.event,
                Event::MotionNotify(e) => e.event,
                Event::ButtonPress(e) => e.event,
                Event::ButtonRelease(e) => e.event,
                _ => continue,
            };
            let Some(index) = self
                .panels
                .iter()
                .position(|panel| panel.window == window || panel.sensor == window)
            else {
                continue;
            };
            match event {
                Event::Expose(e) if e.count == 0 => self.paint(index)?,
                // Activating a pointer grab synthesizes a crossing to the
                // grab window without moving the pointer. It must not change
                // hover ownership. Ungrab crossings still describe the actual
                // destination, and pointer-window polling recovers exits made
                // while another client's grab suppressed our crossing events.
                Event::EnterNotify(e) if e.mode != xproto::NotifyMode::GRAB => {
                    self.panels[index].reveal.enter(now, &self.config);
                }
                Event::LeaveNotify(e) if e.mode != xproto::NotifyMode::GRAB => {
                    if !self.pointer_in_panel(index)? {
                        self.panels[index].reveal.leave(now, &self.config);
                    }
                    self.panels[index].hovered = None;
                    self.paint(index)?;
                }
                Event::MotionNotify(e) => {
                    if e.event == self.panels[index].sensor {
                        continue;
                    }
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
        if restack {
            for panel in &self.panels {
                if panel.mapped {
                    self.conn.configure_window(
                        panel.window,
                        &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE),
                    )?;
                }
                if panel.sensor_mapped {
                    self.conn.configure_window(
                        panel.sensor,
                        &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE),
                    )?;
                }
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
        if !self.shown || !self.panels[index].mapped {
            return Ok(());
        }
        let panel = &mut self.panels[index];
        let p = panel.placement;
        let pixmap = match panel.back_buffer {
            Some((pixmap, width, height)) if width == p.width && height == p.height => pixmap,
            previous => {
                if let Some((pixmap, _, _)) = previous {
                    self.conn.free_pixmap(pixmap)?;
                }
                let pixmap = self.conn.generate_id()?;
                self.conn
                    .create_pixmap(
                        self.depth,
                        pixmap,
                        panel.window,
                        p.width as u16,
                        p.height as u16,
                    )?
                    .check()?;
                panel.back_buffer = Some((pixmap, p.width, p.height));
                pixmap
            }
        };
        let mut surface = ImageSurface::create(Format::ARgb32, p.width as i32, p.height as i32)?;
        let ctx = Context::new(&surface)?;
        let previous_hits = panel.hits.clone();
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
                32.0,
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
            material(&ctx, [x, y, w, h], dark)?;
            ctx.restore()?;
            rounded(
                &ctx,
                x + 0.5,
                y + 0.5,
                (w - 1.0).max(1.0),
                (h - 1.0).max(1.0),
                32.0,
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
        if previous_hits != panel.hits {
            let controls: Vec<_> = panel
                .hits
                .iter()
                .map(|hit| {
                    let mut value = serde_json::json!({"rect":hit.rect,"kind":"control"});
                    match &hit.action {
                        Action::FocusWindow(id) => {
                            value["kind"] = "task".into();
                            value["id"] = (*id).into();
                        }
                        Action::Model(UserAction::ViewTagOn { tag, .. }) => {
                            value["kind"] = "workspace".into();
                            value["index"] = tag.index().into();
                        }
                        Action::CycleTasks => value["kind"] = "task_overflow".into(),
                        _ => {}
                    }
                    value
                })
                .collect();
            let property = self
                .conn
                .intern_atom(false, b"_JWM_EDGE_CONTROLS")?
                .reply()?
                .atom;
            let utf8 = self.conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
            self.conn.change_property8(
                PropMode::REPLACE,
                panel.window,
                property,
                utf8,
                &serde_json::to_vec(&controls)?,
            )?;
        }
        surface.flush();
        // Limit each request to 32 KiB; large high-DPI docks must also work
        // without the BIG-REQUESTS extension.
        let stride = surface.stride() as usize;
        // Include the PutImage request header in the wire budget. Widths
        // such as 90 px otherwise produce 32,784-byte full chunks.
        let rows = ((32_768 - 24) / stride).max(1);
        let bytes = surface.data()?;
        for start in (0..p.height as usize).step_by(rows) {
            let count = rows.min(p.height as usize - start);
            self.conn.put_image(
                ImageFormat::Z_PIXMAP,
                pixmap,
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
        // Present one complete frame. Direct tiled writes to the window can
        // expose a partially uploaded sidebar to a compositing WM's damage
        // capture; an offscreen upload followed by one copy is atomic.
        self.conn.change_window_attributes(
            panel.window,
            &xproto::ChangeWindowAttributesAux::new().background_pixmap(pixmap),
        )?;
        self.conn.map_window(panel.window)?;
        self.conn.copy_area(
            pixmap,
            panel.window,
            panel.gc,
            0,
            0,
            0,
            0,
            p.width as u16,
            p.height as u16,
        )?;
        Ok(())
    }
}

fn config_stamp() -> Option<(std::time::SystemTime, u64)> {
    let (path, _) = BarConfig::default_path()?;
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}
