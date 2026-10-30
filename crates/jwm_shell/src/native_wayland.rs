//! Native wlr-layer-shell adapter. The Cairo scene, hit regions, actions and
//! reveal state are shared with the X11 adapter; only surfaces/input differ.
use anyhow::{Context as _, Result, bail};
use cairo::{Context, Format, ImageSurface};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};
use std::{
    io::{Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, QueueHandle, delegate_noop,
    globals::registry_queue_init,
    protocol::{wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_surface},
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use xbar_core::{
    BarEdge, BarPlacement, ThemeMode, UserAction,
    config::BarConfig,
    edge_shell::{EdgeReveal, RevealMode, panel_geometry, trigger_geometry},
};

use crate::edge_shell::{
    Action, Hit, Panel, PanelIdentity, Preview, ShellState,
    painter::{Painter, material, rounded},
    preview_delivery,
};

struct Shared {
    latest: Option<ShellState>,
    acknowledgements: Vec<UserAction>,
    stopped: bool,
}

pub struct NativeShell {
    shared: Arc<Mutex<Shared>>,
    wake: UnixStream,
    worker: Option<JoinHandle<()>>,
    panels: Vec<PanelIdentity>,
    alive: Arc<AtomicBool>,
}

impl NativeShell {
    pub fn start_managed(
        config: BarConfig,
        state: ShellState,
        scale: f64,
        connector: Option<String>,
        handler: Arc<dyn Fn(Action) + Send + Sync>,
    ) -> Result<Option<Self>> {
        if !config.edge_shell.enabled {
            return Ok(None);
        }
        static NEXT_WORKER: AtomicU64 = AtomicU64::new(1);
        let worker_identity = NEXT_WORKER.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(Mutex::new(Shared {
            latest: Some(state),
            acknowledgements: Vec::new(),
            stopped: false,
        }));
        let alive = Arc::new(AtomicBool::new(true));
        let worker_shared = shared.clone();
        let worker_alive = alive.clone();
        let (wake, mut receiver) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        receiver.set_nonblocking(true)?;
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let bootstrap_deadline = Instant::now() + Duration::from_secs(9);
        let worker = std::thread::Builder::new()
            .name("shell-wayland".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    let initial = worker_shared.lock().unwrap().latest.take().unwrap();
                    let conn = crate::wayland_bootstrap::connect(bootstrap_deadline, &worker_alive)
                        .context("connect native Wayland shell")?;
                    let mut bootstrap = crate::wayland_bootstrap::Deadline::start(
                        &conn,
                        bootstrap_deadline,
                        Arc::clone(&worker_alive),
                    )?;
                    let (globals, mut queue) = registry_queue_init::<Host>(&conn)?;
                    let qh = queue.handle();
                    let compositor = CompositorState::bind(&globals, &qh)?;
                    let shell = LayerShell::bind(&globals, &qh)
                        .context("JWM wlr-layer-shell global missing")?;
                    let shm = Shm::bind(&globals, &qh)?;
                    let viewporter =
                        globals.bind::<wp_viewporter::WpViewporter, _, _>(&qh, 1..=1, ())?;
                    let mut host = Host {
                        registry: RegistryState::new(&globals),
                        outputs: OutputState::new(&globals, &qh),
                        seats: SeatState::new(&globals, &qh),
                        compositor,
                        shm,
                        pool: None,
                        panels: Vec::new(),
                        pointer: None,
                        bound_output: None,
                        scale: scale.clamp(0.25, 8.0),
                        state: initial,
                        handler,
                        preview: None,
                        acknowledged: None,
                        last_preview: Instant::now(),
                        reported_shelf: None,
                        reported_dock: std::collections::HashMap::new(),
                        stopped: false,
                        error: None,
                    };
                    queue.roundtrip(&mut host)?;
                    let candidates: Vec<_> = host.outputs.outputs().collect();
                    let output = candidates
                        .iter()
                        .find(|output| {
                            host.outputs.info(output).is_some_and(|info| {
                                connector
                                    .as_ref()
                                    .is_some_and(|name| info.name.as_ref() == Some(name))
                            })
                        })
                        .or_else(|| (candidates.len() == 1).then(|| &candidates[0]))
                        .context("cannot uniquely bind shell to Wayland output connector")?
                        .clone();
                    host.bound_output = Some(output.clone());
                    host.pool = Some(SlotPool::new(1024 * 1024, &host.shm)?);
                    for edge in [BarEdge::Left, BarEdge::Right, BarEdge::Bottom] {
                        let namespace = format!(
                            "jwm-shell-{}-{}-{}-{}-{}",
                            host.state.snapshot.wm_session_id,
                            host.state.snapshot.monitor.0,
                            edge.as_str(),
                            std::process::id(),
                            worker_identity
                        );
                        let layer = shell.create_layer_surface(
                            &qh,
                            host.compositor.create_surface(&qh),
                            Layer::Top,
                            Some(namespace.clone()),
                            Some(&output),
                        );
                        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
                        let viewport = viewporter.get_viewport(layer.wl_surface(), &qh, ());
                        let sensor = shell.create_layer_surface(
                            &qh,
                            host.compositor.create_surface(&qh),
                            Layer::Top,
                            Some(format!("{namespace}-sensor")),
                            Some(&output),
                        );
                        sensor.set_keyboard_interactivity(KeyboardInteractivity::None);
                        let sensor_viewport = viewporter.get_viewport(sensor.wl_surface(), &qh, ());
                        let placement = panel_geometry(
                            host.state.output,
                            edge,
                            true,
                            config.edge_shell.trigger_pixels,
                        );
                        let scene = Panel {
                            task_offset: 0,
                            edge,
                            window: 0,
                            sensor: 0,
                            sensor_rect: trigger_geometry(
                                host.state.output,
                                edge,
                                config.edge_shell.trigger_pixels,
                            ),
                            mapped: false,
                            sensor_mapped: false,
                            applied_mode: None,
                            gc: 0,
                            back_buffer: None,
                            reveal: EdgeReveal::new(config.edge_shell.mode(edge)),
                            placement,
                            hits: Vec::new(),
                            hovered: None,
                            pressed: None,
                        };
                        host.panels.push(NativePanel {
                            scene,
                            layer,
                            sensor,
                            viewport,
                            sensor_viewport,
                            configured: false,
                            sensor_configured: false,
                            namespace,
                            pointer_inside: false,
                            pointer_position: None,
                            sensor_inside: false,
                            dirty: true,
                            buffers: Vec::new(),
                            sensor_buffers: Vec::new(),
                            buffer_pressure: false,
                        });
                    }
                    host.configure_surfaces(&qh)?;
                    queue.roundtrip(&mut host)?;
                    host.draw(&qh)?;
                    conn.flush()?;
                    // Native object IDs are local to this Wayland connection. Use
                    // the compositor's namespace inventory to register its actual
                    // backend-neutral WindowId, never a guessed wl_proxy ID.
                    let deadline =
                        (Instant::now() + Duration::from_secs(5)).min(bootstrap_deadline);
                    let identities = loop {
                        let inventory =
                            crate::ipc::request("get_windows", serde_json::Value::Null)?;
                        let rows = inventory.as_array().context("invalid window inventory")?;
                        let ids: Vec<_> = host
                            .panels
                            .iter()
                            .filter_map(|panel| {
                                rows.iter()
                                    .find(|row| {
                                        row.get("name").and_then(|name| name.as_str())
                                            == Some(panel.namespace.as_str())
                                    })
                                    .and_then(|row| row.get("id").and_then(|id| id.as_u64()))
                                    .map(|window| PanelIdentity {
                                        output: host.state.snapshot.monitor.0,
                                        role: panel.scene.edge,
                                        window,
                                    })
                            })
                            .collect();
                        if ids.len() == 3 {
                            break ids;
                        }
                        if Instant::now() >= deadline {
                            bail!("native layer surfaces did not enter JWM inventory");
                        }
                        queue.dispatch_pending(&mut host)?;
                        conn.flush()?;
                        std::thread::sleep(Duration::from_millis(20));
                    };
                    bootstrap.finish();
                    let _ = ready_tx.send(Ok::<_, String>(identities));
                    loop {
                        queue.dispatch_pending(&mut host)?;
                        {
                            let mut shared = worker_shared.lock().unwrap();
                            if shared.stopped || host.stopped {
                                break;
                            }
                            for action in shared.acknowledgements.drain(..) {
                                host.acknowledge(action);
                            }
                            if let Some(state) = shared.latest.take() {
                                drop(shared);
                                host.update(state, &qh)?;
                            }
                        }
                        if let Some(error) = host.error.take() {
                            bail!("native shell: {error}");
                        }
                        host.timers(&qh)?;
                        conn.flush()?;
                        if let Some(read) = queue.prepare_read() {
                            let mut fds = [
                                libc::pollfd {
                                    fd: read.connection_fd().as_raw_fd(),
                                    events: libc::POLLIN,
                                    revents: 0,
                                },
                                libc::pollfd {
                                    fd: receiver.as_raw_fd(),
                                    events: libc::POLLIN,
                                    revents: 0,
                                },
                            ];
                            let timeout = host.poll_timeout();
                            let result = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
                            if result < 0
                                && std::io::Error::last_os_error().kind()
                                    != std::io::ErrorKind::Interrupted
                            {
                                return Err(std::io::Error::last_os_error().into());
                            }
                            if fds[0].revents & libc::POLLIN != 0 {
                                read.read()?;
                            } else {
                                drop(read);
                            }
                            let mut bytes = [0; 64];
                            while receiver.read(&mut bytes).is_ok_and(|read| read > 0) {}
                        }
                    }
                    host.set_preview(None);
                    host.withdraw_geometry();
                    conn.flush()?;
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = ready_tx.send(Err(format!("{error:#}")));
                    log::error!("native shell worker stopped: {error:#}");
                }
                worker_alive.store(false, Ordering::Release);
            })?;
        let mut host = Self {
            shared,
            wake,
            worker: Some(worker),
            panels: Vec::new(),
            alive,
        };
        let panels = ready_rx
            .recv_timeout(Duration::from_secs(10))?
            .map_err(anyhow::Error::msg)?;
        host.panels = panels;
        Ok(Some(host))
    }

    pub fn update_managed(&mut self, state: ShellState) {
        self.shared.lock().unwrap().latest = Some(state);
        let _ = self.wake.write(&[1]);
    }
    pub fn acknowledge(&mut self, action: UserAction) {
        let mut shared = self.shared.lock().unwrap();
        if shared.acknowledgements.len() < 64 {
            shared.acknowledgements.push(action);
        }
        let _ = self.wake.write(&[1]);
    }
    pub fn panels(&self) -> &[PanelIdentity] {
        &self.panels
    }
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}
impl Drop for NativeShell {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        self.shared.lock().unwrap().stopped = true;
        let _ = self.wake.write(&[1]);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct NativePanel {
    scene: Panel,
    layer: LayerSurface,
    sensor: LayerSurface,
    viewport: wp_viewport::WpViewport,
    sensor_viewport: wp_viewport::WpViewport,
    configured: bool,
    sensor_configured: bool,
    namespace: String,
    pointer_inside: bool,
    pointer_position: Option<(f64, f64)>,
    sensor_inside: bool,
    dirty: bool,
    buffers: Vec<Buffer>,
    sensor_buffers: Vec<Buffer>,
    buffer_pressure: bool,
}
struct Host {
    registry: RegistryState,
    outputs: OutputState,
    seats: SeatState,
    compositor: CompositorState,
    shm: Shm,
    pool: Option<SlotPool>,
    panels: Vec<NativePanel>,
    pointer: Option<wl_pointer::WlPointer>,
    bound_output: Option<wl_output::WlOutput>,
    scale: f64,
    state: ShellState,
    handler: Arc<dyn Fn(Action) + Send + Sync>,
    preview: Option<Preview>,
    acknowledged: Option<Preview>,
    last_preview: Instant,
    reported_shelf: Option<(u64, u64, BarPlacement)>,
    reported_dock: std::collections::HashMap<xbar_core::WindowToken, xbar_core::DockItemGeometry>,
    stopped: bool,
    error: Option<String>,
}
impl Host {
    fn send(&self, action: Action) {
        (self.handler)(Action::Bound {
            session: self.state.snapshot.wm_session_id,
            generation: self.state.snapshot.wm_sequence.unwrap_or_default(),
            action: Box::new(action),
        });
    }
    fn config(&self) -> xbar_core::edge_shell::EdgeShellConfig {
        self.state.config.clone().unwrap_or_default()
    }
    fn shown(&self) -> bool {
        self.state.visible.unwrap_or(true) && self.config().enabled
    }
    fn logical(&self, physical: u32) -> u32 {
        (f64::from(physical) / self.scale).round().max(1.0) as u32
    }
    fn configure_surfaces(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let config = self.config();
        let shown = self.shown();
        for index in 0..self.panels.len() {
            let edge = self.panels[index].scene.edge;
            let p = panel_geometry(self.state.output, edge, true, config.trigger_pixels);
            let sensor_rect = trigger_geometry(self.state.output, edge, config.trigger_pixels);
            let (w, h) = surface_extent(self.state.output, p, edge);
            let (sw, sh) = surface_extent(self.state.output, sensor_rect, edge);
            let (lw, lh, lsw, lsh) = (
                self.logical(w),
                self.logical(h),
                self.logical(sw),
                self.logical(sh),
            );
            let cross = self.logical(if edge.is_vertical() {
                p.width
            } else {
                p.height
            }) as i32;
            let panel = &mut self.panels[index];
            let mode = config.mode(edge);
            if panel.scene.applied_mode != Some(mode) {
                panel
                    .scene
                    .reveal
                    .set_mode(mode, panel.pointer_inside || panel.sensor_inside);
                panel.scene.applied_mode = Some(mode);
            }
            panel.scene.placement = p;
            panel.scene.sensor_rect = sensor_rect;
            let anchor = match edge {
                BarEdge::Left => Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM,
                BarEdge::Right => Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM,
                _ => Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            };
            panel.layer.set_anchor(anchor);
            panel.sensor.set_anchor(anchor);
            panel.layer.set_size(
                if edge.is_vertical() { lw } else { 0 },
                if edge.is_vertical() { 0 } else { lh },
            );
            panel.sensor.set_size(
                if edge.is_vertical() { lsw } else { 0 },
                if edge.is_vertical() { 0 } else { lsh },
            );
            let pinned = shown && mode == RevealMode::Always;
            panel
                .layer
                .set_exclusive_zone(if pinned { cross } else { -1 });
            // Overlay inputs must use the actual output edge, independent of
            // any pinned sibling's exclusive rectangle.
            panel.sensor.set_exclusive_zone(-1);
            // Positive exclusive layers are arranged left/right/bottom. Undo
            // the side strips when stretching the bottom surface, retaining
            // the shared scene's exact physical desktop origin.
            let left = if pinned
                && edge == BarEdge::Bottom
                && config.mode(BarEdge::Left) == RevealMode::Always
            {
                (f64::from(
                    panel_geometry(
                        self.state.output,
                        BarEdge::Left,
                        true,
                        config.trigger_pixels,
                    )
                    .width,
                ) / self.scale)
                    .round() as i32
            } else {
                0
            };
            let right = if pinned
                && edge == BarEdge::Bottom
                && config.mode(BarEdge::Right) == RevealMode::Always
            {
                (f64::from(
                    panel_geometry(
                        self.state.output,
                        BarEdge::Right,
                        true,
                        config.trigger_pixels,
                    )
                    .width,
                ) / self.scale)
                    .round() as i32
            } else {
                0
            };
            panel.layer.set_margin(0, -right, 0, -left);
            panel.sensor.set_margin(0, 0, 0, 0);
            panel.viewport.set_destination(lw as i32, lh as i32);
            panel
                .sensor_viewport
                .set_destination(lsw as i32, lsh as i32);
            panel.layer.commit();
            panel.sensor.commit();
            panel.dirty = true;
        }
        self.update_input(qh)?;
        Ok(())
    }
    fn update_input(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let shown = self.shown();
        let config = self.config();
        for panel in &self.panels {
            let region = self.compositor.wl_compositor().create_region(qh, ());
            if shown && panel.scene.reveal.visible() {
                let p = panel.scene.placement;
                let (x, y) = content_offset(self.state.output, p, panel.scene.edge);
                region.add(
                    (f64::from(x) / self.scale).round() as i32,
                    (f64::from(y) / self.scale).round() as i32,
                    (f64::from(p.width) / self.scale).round().max(1.0) as i32,
                    (f64::from(p.height) / self.scale).round().max(1.0) as i32,
                );
            }
            panel.layer.wl_surface().set_input_region(Some(&region));
            region.destroy();
            let sensor = self.compositor.wl_compositor().create_region(qh, ());
            if shown
                && config.mode(panel.scene.edge) != RevealMode::Disabled
                && (config.mode(panel.scene.edge) == RevealMode::Hover
                    || !panel.scene.reveal.visible())
            {
                let p = panel.scene.sensor_rect;
                let (x, y) = content_offset(self.state.output, p, panel.scene.edge);
                sensor.add(
                    (f64::from(x) / self.scale).round() as i32,
                    (f64::from(y) / self.scale).round() as i32,
                    (f64::from(p.width) / self.scale).round().max(1.0) as i32,
                    (f64::from(p.height) / self.scale).round().max(1.0) as i32,
                );
            }
            panel.sensor.wl_surface().set_input_region(Some(&sensor));
            sensor.destroy();
            panel.layer.commit();
            panel.sensor.commit();
        }
        Ok(())
    }
    fn update(&mut self, state: ShellState, qh: &QueueHandle<Self>) -> Result<()> {
        if self.state.snapshot.wm_session_id != state.snapshot.wm_session_id
            || self.state.snapshot.wm_sequence != state.snapshot.wm_sequence
        {
            self.set_preview(None);
            self.acknowledged = None;
            self.reported_shelf = None;
            self.reported_dock.clear();
            for panel in &mut self.panels {
                panel.scene.pressed = None;
                panel.scene.hovered = None;
            }
        }
        if state.visible == Some(false) {
            self.set_preview(None);
        }
        self.state = state;
        self.configure_surfaces(qh)?;
        self.draw(qh)
    }
    fn draw(&mut self, _qh: &QueueHandle<Self>) -> Result<()> {
        let shown = self.shown();
        let output = self.state.output;
        let snapshot = &self.state.snapshot;
        let tasks = self.state.tasks.as_deref().unwrap_or_default();
        let mut next_hover = None;
        for panel in &mut self.panels {
            if !panel.configured || !panel.dirty {
                continue;
            }
            let p = panel.scene.placement;
            let (width, height) = surface_extent(output, p, panel.scene.edge);
            let pool = self
                .pool
                .as_mut()
                .context("native shell buffer pool not initialized")?;
            let Some(buffer_index) = reusable_buffer(pool, &mut panel.buffers, width, height)?
            else {
                // The compositor has not released either frame. Keep the latest
                // coalesced scene dirty rather than growing shared memory.
                if cfg!(debug_assertions) && !panel.buffer_pressure {
                    log::info!(
                        "native {:?} framebuffer pressure at generation {:?}",
                        panel.scene.edge,
                        snapshot.wm_sequence
                    );
                }
                panel.buffer_pressure = true;
                continue;
            };
            if cfg!(debug_assertions) && panel.buffer_pressure {
                log::info!(
                    "native {:?} framebuffer pressure recovered at generation {:?}",
                    panel.scene.edge,
                    snapshot.wm_sequence
                );
            }
            panel.buffer_pressure = false;
            let mut surface = ImageSurface::create(Format::ARgb32, width as i32, height as i32)?;
            let ctx = Context::new(&surface)?;
            if shown && panel.scene.reveal.visible() {
                let (dx, dy) = content_offset(output, p, panel.scene.edge);
                ctx.translate(f64::from(dx), f64::from(dy));
                let (x, y, w, h) = if panel.scene.edge.is_vertical() {
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
                let dark = snapshot.theme == ThemeMode::Dark;
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
                panel.scene.hits.clear();
                let mut painter = Painter {
                    ctx: &ctx,
                    panel: &mut panel.scene,
                    snapshot,
                    tasks,
                    dark,
                };
                ctx.save()?;
                ctx.rectangle(x, y, w, h);
                ctx.clip();
                painter.draw(x, y, w, h)?;
                ctx.restore()?;
            } else {
                panel.scene.hits.clear();
            }
            drop(ctx);
            surface.flush();
            let data = surface.data()?;
            let buffer = &panel.buffers[buffer_index];
            let canvas = pool
                .canvas(buffer)
                .context("native buffer became busy while drawing")?;
            canvas.copy_from_slice(&data);
            buffer.attach_to(panel.layer.wl_surface())?;
            panel
                .layer
                .wl_surface()
                .damage_buffer(0, 0, width as i32, height as i32);
            panel.layer.commit();
            if panel.sensor_configured {
                let (sw, sh) = surface_extent(output, panel.scene.sensor_rect, panel.scene.edge);
                if let Some(index) = reusable_buffer(pool, &mut panel.sensor_buffers, sw, sh)? {
                    let buffer = &panel.sensor_buffers[index];
                    if let Some(canvas) = pool.canvas(buffer) {
                        canvas.fill(0);
                        buffer.attach_to(panel.sensor.wl_surface())?;
                        panel
                            .sensor
                            .wl_surface()
                            .damage_buffer(0, 0, sw as i32, sh as i32);
                        panel.sensor.commit();
                    }
                }
            }
            let previous_hover = panel.scene.hovered;
            if panel.pointer_inside
                && let Some((sx, sy)) = panel.pointer_position
            {
                let (dx, dy) = content_offset(output, p, panel.scene.edge);
                let (x, y) = (sx - f64::from(dx), sy - f64::from(dy));
                panel.scene.hovered = panel.scene.hits.iter().position(|hit| hit.contains(x, y));
                next_hover =
                    Some(panel.scene.hovered.and_then(
                        |index| match panel.scene.hits[index].action {
                            Action::Model(action) => Preview::from_action(action),
                            _ => None,
                        },
                    ));
            }
            panel.dirty = previous_hover != panel.scene.hovered;
        }
        if let Some(preview) = next_hover {
            self.set_preview(preview);
        }
        self.report_geometry();
        Ok(())
    }
    fn withdraw_geometry(&mut self) {
        if self.reported_shelf.take().is_none() && self.reported_dock.is_empty() {
            return;
        }
        let session = self.state.snapshot.wm_session_id;
        let generation = self.state.snapshot.wm_sequence.unwrap_or_default();
        let windows: Vec<_> = self
            .reported_dock
            .drain()
            .map(|(window, _)| Some(window))
            .chain(std::iter::once(None))
            .collect();
        for window in windows {
            self.send(Action::Model(UserAction::SetDockGeometry {
                window,
                wm_session_id: session,
                minimized_generation: generation,
                geometry: Default::default(),
            }));
        }
    }

    fn report_geometry(&mut self) {
        let Some(panel) = self
            .panels
            .iter()
            .find(|panel| panel.scene.edge == BarEdge::Bottom)
        else {
            return;
        };
        if !self.shown() || !panel.scene.reveal.visible() {
            self.withdraw_geometry();
            return;
        }
        let placement = panel.scene.placement;
        let items: std::collections::HashMap<_, _> = panel
            .scene
            .hits
            .iter()
            .filter_map(|hit| {
                if let Action::Model(UserAction::RestoreWindow {
                    window, geometry, ..
                }) = hit.action
                {
                    Some((window, geometry))
                } else {
                    None
                }
            })
            .collect();
        let session = self.state.snapshot.wm_session_id;
        let generation = self.state.snapshot.wm_sequence.unwrap_or_default();
        let shelf = (session, generation, placement);
        if self.reported_shelf == Some(shelf) && self.reported_dock == items {
            return;
        }
        self.reported_shelf = Some(shelf);
        self.send(Action::Model(UserAction::SetDockGeometry {
            window: None,
            wm_session_id: session,
            minimized_generation: generation,
            geometry: xbar_core::DockItemGeometry::new(
                placement.x,
                placement.y,
                placement.width,
                placement.height,
            ),
        }));
        for (window, geometry) in &items {
            self.send(Action::Model(UserAction::SetDockGeometry {
                window: Some(*window),
                wm_session_id: session,
                minimized_generation: generation,
                geometry: *geometry,
            }));
        }
        for window in self
            .reported_dock
            .keys()
            .filter(|window| !items.contains_key(window))
        {
            self.send(Action::Model(UserAction::SetDockGeometry {
                window: Some(*window),
                wm_session_id: session,
                minimized_generation: generation,
                geometry: Default::default(),
            }));
        }
        self.reported_dock = items;
    }

    fn poll_timeout(&self) -> i32 {
        let reveal = self
            .panels
            .iter()
            .filter(|panel| panel.scene.pressed.is_none())
            .filter_map(|panel| panel.scene.reveal.deadline())
            .min();
        let preview = if self.preview != self.acknowledged {
            Some(self.last_preview + Duration::from_millis(100))
        } else if self.preview.is_some() {
            Some(self.last_preview + Duration::from_millis(250))
        } else {
            None
        };
        reveal
            .into_iter()
            .chain(preview)
            .min()
            .map_or(-1, |deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(i32::MAX as u128) as i32
            })
    }

    fn timers(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let now = Instant::now();
        let mut changed = false;
        for panel in &mut self.panels {
            if panel.scene.pressed.is_none() && panel.scene.reveal.tick(now) {
                panel.dirty = true;
                changed = true;
            }
        }
        if changed {
            self.update_input(qh)?;
        }
        if self.last_preview.elapsed() >= Duration::from_millis(100) {
            self.send_preview(self.last_preview.elapsed() >= Duration::from_millis(250));
        }
        self.draw(qh)
    }
    fn set_preview(&mut self, preview: Option<Preview>) {
        let preview = preview.filter(|preview| preview.is_current(&self.state.snapshot));
        if self.preview != preview {
            self.preview = preview;
            self.send_preview(false);
        }
    }
    fn send_preview(&mut self, renewal: bool) {
        self.preview = self
            .preview
            .filter(|preview| preview.is_current(&self.state.snapshot));
        self.acknowledged = self
            .acknowledged
            .filter(|preview| preview.epoch_current(&self.state.snapshot));
        if let Some(action) = preview_delivery(self.preview, self.acknowledged, renewal) {
            self.send(Action::Model(action));
            self.last_preview = Instant::now();
        }
    }
    fn acknowledge(&mut self, action: UserAction) {
        if let UserAction::PreviewWindow { visible, .. } = action {
            let preview = Preview::from_action(action);
            if !preview.is_some_and(|preview| preview.epoch_current(&self.state.snapshot)) {
                return;
            }
            if visible {
                self.acknowledged = preview;
            } else if self.acknowledged == preview {
                self.acknowledged = None;
            }
            self.send_preview(false);
        }
    }

    fn slider(&self, hit: &Hit, y: f64) {
        if let Some((audio, top, height)) = hit.slider {
            let percent = ((1.0 - (y - top) / height).clamp(0.0, 1.0) * 100.0).round() as i32;
            self.send(Action::Level { audio, percent });
        }
    }
    fn input(&mut self, event: &PointerEvent) -> Result<()> {
        let Some((index, sensor)) = self.panels.iter().enumerate().find_map(|(index, panel)| {
            if &event.surface == panel.layer.wl_surface() {
                Some((index, false))
            } else if &event.surface == panel.sensor.wl_surface() {
                Some((index, true))
            } else {
                None
            }
        }) else {
            return Ok(());
        };
        let config = self.config();
        let now = Instant::now();
        let p = self.panels[index].scene.placement;
        let (dx, dy) = content_offset(self.state.output, p, self.panels[index].scene.edge);
        let x = event.position.0 * self.scale - f64::from(dx);
        let y = event.position.1 * self.scale - f64::from(dy);
        match event.kind {
            PointerEventKind::Enter { .. } => {
                let panel = &mut self.panels[index];
                if sensor {
                    panel.sensor_inside = true;
                } else {
                    panel.pointer_inside = true;
                    panel.pointer_position =
                        Some((event.position.0 * self.scale, event.position.1 * self.scale));
                }
                panel.scene.reveal.enter(now, &config);
                panel.dirty = true;
                if !sensor {
                    panel.scene.hovered =
                        panel.scene.hits.iter().position(|hit| hit.contains(x, y));
                    let preview =
                        panel
                            .scene
                            .hovered
                            .and_then(|hit| match panel.scene.hits[hit].action {
                                Action::Model(action) => Preview::from_action(action),
                                _ => None,
                            });
                    self.set_preview(preview);
                }
            }
            PointerEventKind::Leave { .. } => {
                let panel = &mut self.panels[index];
                if sensor {
                    panel.sensor_inside = false;
                } else {
                    panel.pointer_inside = false;
                    panel.pointer_position = None;
                    panel.scene.hovered = None;
                }
                if !panel.sensor_inside && !panel.pointer_inside {
                    panel.scene.reveal.leave(now, &config);
                }
                panel.dirty = true;
                self.set_preview(None);
            }
            PointerEventKind::Motion { .. } if !sensor => {
                let panel = &mut self.panels[index];
                panel.pointer_position =
                    Some((event.position.0 * self.scale, event.position.1 * self.scale));
                panel.scene.hovered = panel.scene.hits.iter().position(|hit| hit.contains(x, y));
                panel.dirty = true;
                let pressed = panel.scene.pressed.clone();
                let preview =
                    panel
                        .scene
                        .hovered
                        .and_then(|hit| match panel.scene.hits[hit].action {
                            Action::Model(action) => Preview::from_action(action),
                            _ => None,
                        });
                if let Some(hit) = pressed {
                    self.slider(&hit, y);
                }
                self.set_preview(preview);
            }
            PointerEventKind::Press { button: 0x110, .. } if !sensor => {
                let panel = &mut self.panels[index];
                let hit = panel
                    .scene
                    .hits
                    .iter()
                    .find(|hit| hit.contains(x, y))
                    .cloned();
                panel.scene.pressed = hit.clone();
                panel.dirty = true;
                if let Some(hit) = hit {
                    self.slider(&hit, y);
                }
            }
            PointerEventKind::Release { button: 0x110, .. } if !sensor => {
                let panel = &mut self.panels[index];
                let hit = panel.scene.pressed.take();
                panel.dirty = true;
                if let Some(hit) = hit {
                    if hit.slider.is_some() {
                        self.slider(&hit, y);
                    } else if hit.contains(x, y) {
                        if hit.action == Action::HidePanel {
                            let panel = &mut self.panels[index];
                            panel.scene.reveal.dismiss();
                            panel.scene.hovered = None;
                            if panel.scene.edge == BarEdge::Bottom {
                                self.set_preview(None);
                                self.withdraw_geometry();
                            }
                        } else if hit.action == Action::CycleTasks {
                            self.panels[index].scene.task_offset += 1;
                        } else {
                            self.send(hit.action);
                        }
                    }
                }
                let panel = &mut self.panels[index];
                if !panel.pointer_inside && !panel.sensor_inside {
                    panel.scene.reveal.leave(now, &config);
                }
            }
            PointerEventKind::Axis { vertical, .. } if !sensor => {
                let hit = self.panels[index]
                    .scene
                    .hits
                    .iter()
                    .find(|hit| hit.contains(x, y));
                if let Some(hit) = hit
                    && let Some((audio, ..)) = hit.slider
                {
                    let step = if vertical.discrete != 0 {
                        -vertical.discrete * 5
                    } else if vertical.absolute < 0.0 {
                        5
                    } else if vertical.absolute > 0.0 {
                        -5
                    } else {
                        0
                    };
                    self.send(Action::Model(if audio {
                        UserAction::AdjustVolume(step)
                    } else {
                        UserAction::AdjustBrightness(step)
                    }));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// At most two frames per surface may await wl_buffer.release. SlotPool's
/// allocator otherwise grows indefinitely while an IPC/compositor is paused.
fn reusable_buffer(
    pool: &mut SlotPool,
    buffers: &mut Vec<Buffer>,
    width: u32,
    height: u32,
) -> Result<Option<usize>> {
    buffers
        .retain(|buffer| buffer.stride() == width as i32 * 4 && buffer.height() == height as i32);
    if let Some(index) = buffers
        .iter()
        .position(|buffer| pool.canvas(buffer).is_some())
    {
        return Ok(Some(index));
    }
    if buffers.len() >= 2 {
        return Ok(None);
    }
    let (buffer, _) = pool.create_buffer(
        width as i32,
        height as i32,
        width as i32 * 4,
        wl_shm::Format::Argb8888,
    )?;
    buffers.push(buffer);
    Ok(Some(buffers.len() - 1))
}

fn surface_extent(
    output: xbar_core::MonitorGeometry,
    p: BarPlacement,
    edge: BarEdge,
) -> (u32, u32) {
    if edge.is_vertical() {
        (p.width, output.height.clamp(1, 16384))
    } else {
        (output.width.clamp(1, 16384), p.height)
    }
}
fn content_offset(
    output: xbar_core::MonitorGeometry,
    p: BarPlacement,
    edge: BarEdge,
) -> (i32, i32) {
    if edge.is_vertical() {
        (0, p.y.saturating_sub(output.y))
    } else {
        (p.x.saturating_sub(output.x), 0)
    }
}

impl CompositorHandler for Host {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}
impl OutputHandler for Host {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.bound_output.as_ref() == Some(&output) {
            self.stopped = true;
        }
    }
}
impl LayerShellHandler for Host {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.stopped = true;
    }
    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        for panel in &mut self.panels {
            if &panel.layer == layer {
                panel.configured = true;
                panel.dirty = true;
            }
            if &panel.sensor == layer {
                panel.sensor_configured = true;
                panel.dirty = true;
            }
        }
        if let Err(error) = self.draw(qh) {
            self.error = Some(error.to_string());
        }
    }
}
impl SeatHandler for Host {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seats.get_pointer(qh, &seat) {
                Ok(pointer) => self.pointer = Some(pointer),
                Err(error) => self.error = Some(error.to_string()),
            }
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            pointer.release();
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}
impl PointerHandler for Host {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if let Err(error) = self.input(event) {
                self.error = Some(error.to_string());
            }
        }
        if let Err(error) = self.update_input(qh).and_then(|_| self.draw(qh)) {
            self.error = Some(error.to_string());
        }
    }
}
impl ShmHandler for Host {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
delegate_compositor!(Host);
delegate_output!(Host);
delegate_shm!(Host);
delegate_seat!(Host);
delegate_pointer!(Host);
delegate_layer!(Host);
delegate_registry!(Host);
delegate_noop!(Host: ignore wl_region::WlRegion);
delegate_noop!(Host: ignore wp_viewporter::WpViewporter);
delegate_noop!(Host: ignore wp_viewport::WpViewport);
impl ProvidesRegistryState for Host {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_full_axis_surfaces_retain_exact_shared_scene_anchor() {
        let output = xbar_core::MonitorGeometry {
            x: -1280,
            y: 120,
            width: 1280,
            height: 800,
        };
        for edge in [BarEdge::Left, BarEdge::Right, BarEdge::Bottom] {
            let panel = panel_geometry(output, edge, true, 2);
            let (w, h) = surface_extent(output, panel, edge);
            let (x, y) = content_offset(output, panel, edge);
            assert!(x >= 0 && y >= 0);
            assert!(x as u32 + panel.width <= w && y as u32 + panel.height <= h);
            if edge.is_vertical() {
                assert_eq!(h, output.height);
            } else {
                assert_eq!(w, output.width);
            }
        }
    }
}
